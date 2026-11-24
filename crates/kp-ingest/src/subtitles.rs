use kp_model::AnchorSpec;
use kp_store::ChunkInput;
use serde_json::Value;

use crate::{text::line_anchor, IngestError, Result};

pub(crate) fn parse(source: &str) -> Result<Vec<ChunkInput>> {
    let lines: Vec<_> = source
        .split_inclusive('\n')
        .scan(0, |offset, line| {
            let start = *offset;
            *offset += line.len();
            Some((start, line))
        })
        .collect();
    let mut chunks = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let (offset, line) = lines[i];
        if let Some((start, end)) = line.trim().split_once("-->") {
            let start_ms = timestamp(start.trim())?;
            let end_ms = timestamp(
                end.split_whitespace()
                    .next()
                    .ok_or(IngestError::Invalid("missing cue end"))?,
            )?;
            if end_ms <= start_ms {
                return Err(IngestError::Invalid("cue end must follow start"));
            }
            let cue_start = if i > 0
                && !lines[i - 1].1.trim().is_empty()
                && !lines[i - 1].1.starts_with("WEBVTT")
            {
                lines[i - 1].0
            } else {
                offset
            };
            let body_start = offset + line.len();
            i += 1;
            while i < lines.len() && !lines[i].1.trim().is_empty() {
                i += 1;
            }
            let body_end = lines.get(i).map(|l| l.0).unwrap_or(source.len());
            let mut anchor = line_anchor(source, cue_start..body_end)?;
            anchor.start_ms = Some(start_ms);
            anchor.end_ms = Some(end_ms);
            chunks.push(ChunkInput {
                heading: format!("Cue {}", chunks.len() + 1),
                body: source[body_start..body_end].trim_end().into(),
                anchor,
            });
        } else if line.trim().starts_with("NOTE") || matches!(line.trim(), "STYLE" | "REGION") {
            while i < lines.len() && !lines[i].1.trim().is_empty() {
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    Ok(chunks)
}

fn timestamp(text: &str) -> Result<i64> {
    let parts: Vec<_> = text.split(':').collect();
    let (hours, minutes, seconds) = match parts.as_slice() {
        [m, s] => ("0", *m, *s),
        [h, m, s] => (*h, *m, *s),
        _ => return Err(IngestError::Invalid("invalid cue timestamp")),
    };
    let (seconds, millis) = seconds
        .split_once(['.', ','])
        .ok_or(IngestError::Invalid("cue needs milliseconds"))?;
    let number = |s: &str| -> Result<i64> {
        if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
            return Err(IngestError::Invalid("invalid cue timestamp"));
        }
        s.parse()
            .map_err(|_| IngestError::Invalid("invalid cue timestamp"))
    };
    let (h, m, s, ms) = (
        number(hours)?,
        number(minutes)?,
        number(seconds)?,
        number(millis)?,
    );
    if m >= 60 || s >= 60 || millis.len() != 3 {
        return Err(IngestError::Invalid("invalid cue timestamp"));
    }
    h.checked_mul(3_600_000)
        .and_then(|v| v.checked_add(m * 60_000 + s * 1000 + ms))
        .ok_or(IngestError::Invalid("cue timestamp overflow"))
}

pub(crate) fn parse_lrc(source: &str) -> Result<Vec<ChunkInput>> {
    let mut entries: Vec<(i64, usize, usize, String)> = Vec::new();
    let mut byte_offset = 0usize;
    for raw_line in source.split_inclusive('\n') {
        let start_b = byte_offset;
        byte_offset += raw_line.len();
        let trimmed = raw_line.trim();
        if !trimmed.starts_with('[') {
            continue;
        }
        let Some(close) = trimmed.find(']') else {
            continue;
        };
        let tag = &trimmed[1..close];
        let Some(ms) = parse_lrc_tag_ms(tag) else {
            continue;
        };
        let lyric = trimmed[close + 1..].trim();
        if lyric.is_empty() {
            continue;
        }
        entries.push((ms, start_b, byte_offset, lyric.to_owned()));
    }
    let mut chunks = Vec::with_capacity(entries.len());
    for (idx, (start_ms, start_b, end_b, body)) in entries.iter().enumerate() {
        let next_ms = entries
            .get(idx + 1)
            .map(|e| e.0)
            .filter(|&n| n > *start_ms)
            .unwrap_or_else(|| start_ms.saturating_add(5_000));
        let mut anchor = line_anchor(source, *start_b..*end_b)?;
        anchor.start_ms = Some(*start_ms);
        anchor.end_ms = Some(next_ms);
        chunks.push(ChunkInput {
            heading: format!("Track {}", idx + 1),
            body: body.clone(),
            anchor,
        });
    }
    Ok(chunks)
}

fn parse_lrc_tag_ms(tag: &str) -> Option<i64> {
    let parts: Vec<&str> = tag.split(':').collect();
    let (h, m, sec_part) = match parts.as_slice() {
        [m, s] => (0_i64, m.parse::<i64>().ok()?, *s),
        [h, m, s] => (h.parse::<i64>().ok()?, m.parse::<i64>().ok()?, *s),
        _ => return None,
    };
    if !(0..60).contains(&m) || h < 0 {
        return None;
    }
    let (s_str, frac_str) = match sec_part.split_once(['.', ',']) {
        Some((s, f)) => (s, f),
        None => (sec_part, "0"),
    };
    let s: i64 = s_str.parse().ok()?;
    if !(0..60).contains(&s) || !frac_str.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let ms: i64 = match frac_str.len() {
        0 => 0,
        1 => frac_str.parse::<i64>().ok()? * 100,
        2 => frac_str.parse::<i64>().ok()? * 10,
        _ => frac_str[..3].parse::<i64>().ok()?,
    };
    h.checked_mul(3_600_000)?
        .checked_add(m * 60_000 + s * 1000 + ms)
}

pub(crate) fn try_parse_asr_json(source: &str) -> Option<Vec<ChunkInput>> {
    let root: Value = serde_json::from_str(source).ok()?;
    let segments = root
        .get("segments")
        .or_else(|| root.get("utterances"))
        .or_else(|| root.get("transcript_segments"))
        .and_then(Value::as_array)?;
    if segments.is_empty() {
        return None;
    }
    let mut chunks = Vec::new();
    for (idx, seg) in segments.iter().enumerate() {
        let text = seg
            .get("text")
            .or_else(|| seg.get("transcript"))
            .or_else(|| seg.get("content"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())?;
        let start_ms = seg.get("start_ms").and_then(Value::as_i64).or_else(|| {
            seg.get("start")
                .and_then(Value::as_f64)
                .map(|s| (s * 1000.0).round() as i64)
        })?;
        let end_ms = seg
            .get("end_ms")
            .and_then(Value::as_i64)
            .or_else(|| {
                seg.get("end")
                    .and_then(Value::as_f64)
                    .map(|s| (s * 1000.0).round() as i64)
            })
            .unwrap_or_else(|| start_ms.saturating_add(3_000))
            .max(start_ms.saturating_add(1));
        let speaker = seg
            .get("speaker")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let heading = match speaker {
            Some(sp) => format!("Segment {} ({sp})", idx + 1),
            None => format!("Segment {}", idx + 1),
        };
        let body = match speaker {
            Some(sp) if sp.parse::<kp_model::ActorId>().is_ok() => {
                format!("[speaker:{sp}] {text}")
            }
            Some(sp) => format!("{sp}: {text}"),
            None => text.to_owned(),
        };
        chunks.push(ChunkInput {
            heading,
            body,
            anchor: AnchorSpec {
                start_ms: Some(start_ms),
                end_ms: Some(end_ms),
                ..AnchorSpec::default()
            },
        });
    }
    (!chunks.is_empty()).then_some(chunks)
}
