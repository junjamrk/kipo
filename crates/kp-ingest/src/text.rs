// Encoding precedence follows SAG-Wiki/core/src/document_text.rs (MIT).
use std::borrow::Cow;
use std::ops::Range;
use std::sync::LazyLock;

use encoding_rs::{Encoding, UTF_8};
use kp_model::{estimate_batch_tokens, AnchorSpec};
use kp_store::ChunkInput;
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

use crate::{IngestError, Result};

pub(crate) const TARGET_CHUNK_TOKENS: usize = 512;
pub(crate) const HARD_MAX_CHUNK_TOKENS: usize = 768;

static ENCODING_DECLARATION: LazyLock<std::result::Result<regex::Regex, regex::Error>> =
    LazyLock::new(|| {
        regex::Regex::new(
            r#"(?i)(?:\A(?:[^\r\n]*\r?\n)?[ \t]*\#[^\r\n]*?coding\s*[:=]\s*|<meta\b[^>]*?charset\s*=\s*["']?|\A<\?xml\b[^>]*?encoding\s*=\s*["'])([a-z0-9._-]+)"#,
        )
    });

pub(crate) fn decode(bytes: &[u8]) -> Result<Cow<'_, str>> {
    Ok(decode_with_utf8_offset(bytes)?.0)
}

pub(crate) fn decode_with_utf8_offset(bytes: &[u8]) -> Result<(Cow<'_, str>, Option<u64>)> {
    let (encoding, bom) = if let Some(bom) = Encoding::for_bom(bytes) {
        bom
    } else {
        let prefix = String::from_utf8_lossy(&bytes[..bytes.len().min(4096)]);
        let declaration = ENCODING_DECLARATION.as_ref().map_err(Clone::clone)?;
        let encoding = if let Some(captures) = declaration.captures(&prefix) {
            let label = captures
                .get(1)
                .ok_or(IngestError::Invalid("invalid encoding declaration"))?;
            Encoding::for_label_no_replacement(label.as_str().as_bytes())
                .ok_or(IngestError::Invalid("unsupported declared encoding"))?
        } else if let Ok(text) = std::str::from_utf8(bytes) {
            return Ok((Cow::Borrowed(text), Some(0)));
        } else {
            let mut detector = chardetng::EncodingDetector::new();
            detector.feed(bytes, true);
            detector.guess(None, false)
        };
        (encoding, 0)
    };
    let text = encoding
        .decode_without_bom_handling_and_without_replacement(&bytes[bom..])
        .ok_or(IngestError::Invalid("malformed text encoding"))?;
    if encoding != UTF_8 && text.len() > crate::MAX_INPUT_BYTES {
        return Err(IngestError::Limit(crate::MAX_INPUT_BYTES));
    }
    let utf8_offset = (encoding == UTF_8).then_some(bom as u64);
    Ok((text, utf8_offset))
}

pub(crate) fn line_anchor(source: &str, range: Range<usize>) -> Result<AnchorSpec> {
    line_anchor_with_offset(source, range, Some(0))
}

pub(crate) fn line_anchor_with_offset(
    source: &str,
    range: Range<usize>,
    byte_offset: Option<u64>,
) -> Result<AnchorSpec> {
    let start = source[..range.start]
        .bytes()
        .filter(|b| *b == b'\n')
        .count()
        + 1;
    let end = source[..range.end].bytes().filter(|b| *b == b'\n').count()
        + usize::from(range.end > range.start && !source[..range.end].ends_with('\n'));
    Ok(AnchorSpec {
        start_byte: byte_offset.map(|off| range.start as u64 + off),
        end_byte: byte_offset.map(|off| range.end as u64 + off),
        start_line: Some(
            u32::try_from(start).map_err(|_| IngestError::Invalid("too many text lines"))?,
        ),
        end_line: Some(
            u32::try_from(end.max(start))
                .map_err(|_| IngestError::Invalid("too many text lines"))?,
        ),
        ..AnchorSpec::default()
    })
}

fn extend_line_end(source: &str, pos: usize) -> usize {
    let bytes = source.as_bytes();
    if pos >= bytes.len() {
        return bytes.len();
    }
    if bytes[pos] == b'\r' && bytes.get(pos + 1) == Some(&b'\n') {
        pos + 2
    } else if bytes[pos] == b'\n' {
        pos + 1
    } else {
        pos
    }
}

fn format_breadcrumbs(breadcrumbs: &[(u8, String)]) -> String {
    breadcrumbs
        .iter()
        .map(|(_, h)| h.as_str())
        .filter(|h| !h.is_empty())
        .collect::<Vec<_>>()
        .join(" > ")
}

fn with_heading_suffix(base: &str, suffix: &str) -> String {
    if base.is_empty() {
        suffix.to_owned()
    } else {
        format!("{base} {suffix}")
    }
}

fn push_heading_text(buf: &mut String, piece: &str) {
    buf.push_str(piece);
}

fn push_heading_break(buf: &mut String) {
    if !buf.is_empty() && !buf.ends_with(' ') {
        buf.push(' ');
    }
}

fn split_oversized_line_ranges(
    source: &str,
    range: Range<usize>,
    target_tokens: usize,
    hard_max_tokens: usize,
) -> Vec<Range<usize>> {
    if estimate_batch_tokens(&source[range.clone()]) <= hard_max_tokens {
        return vec![range];
    }
    let slice = &source[range.clone()];
    let mut sentence_units: Vec<Range<usize>> = Vec::new();
    let mut unit_start = 0usize;
    let mut chars = slice.char_indices().peekable();
    while let Some((idx, ch)) = chars.next() {
        let is_sentence_end = matches!(ch, '.' | '!' | '?' | '。' | '！' | '？' | '；');
        if is_sentence_end {
            let mut end_idx = idx + ch.len_utf8();
            while let Some(&(next_idx, next_ch)) = chars.peek() {
                if next_ch.is_whitespace() && next_ch != '\n' {
                    end_idx = next_idx + next_ch.len_utf8();
                    chars.next();
                } else {
                    break;
                }
            }
            if matches!(ch, '.' | '!' | '?')
                && end_idx < slice.len()
                && end_idx == idx + ch.len_utf8()
            {
                continue;
            }
            sentence_units.push((range.start + unit_start)..(range.start + end_idx));
            unit_start = end_idx;
        }
    }
    if unit_start < slice.len() {
        sentence_units.push((range.start + unit_start)..range.end);
    }

    let mut fine_units: Vec<Range<usize>> = Vec::new();
    for unit in sentence_units {
        if estimate_batch_tokens(&source[unit.clone()]) <= hard_max_tokens {
            fine_units.push(unit);
            continue;
        }
        let sub = &source[unit.clone()];
        let mut cur_start = 0usize;
        let mut cur_end = 0usize;
        for (idx, ch) in sub.char_indices() {
            let next_end = idx + ch.len_utf8();
            if next_end > cur_start
                && estimate_batch_tokens(&sub[cur_start..next_end]) > target_tokens
            {
                let split_at = if cur_end > cur_start {
                    cur_end
                } else {
                    next_end
                };
                fine_units.push((unit.start + cur_start)..(unit.start + split_at));
                cur_start = split_at;
            }
            if ch.is_whitespace() || next_end - cur_start >= 64 {
                cur_end = next_end;
            }
        }
        if cur_start < sub.len() {
            fine_units.push((unit.start + cur_start)..unit.end);
        }
    }

    pack_contiguous_ranges(source, &fine_units, target_tokens)
}

fn pack_contiguous_ranges(
    source: &str,
    units: &[Range<usize>],
    target_tokens: usize,
) -> Vec<Range<usize>> {
    let mut out: Vec<Range<usize>> = Vec::new();
    let mut current: Option<Range<usize>> = None;
    for unit in units {
        if unit.is_empty() {
            continue;
        }
        match &mut current {
            None => current = Some(unit.clone()),
            Some(cur) => {
                let candidate = cur.start..unit.end;
                if estimate_batch_tokens(&source[candidate.clone()]) <= target_tokens {
                    *cur = candidate;
                } else {
                    if !source[cur.clone()].trim().is_empty() {
                        out.push(cur.clone());
                    }
                    *cur = unit.clone();
                }
            }
        }
    }
    if let Some(cur) = current {
        if !source[cur.clone()].trim().is_empty() {
            out.push(cur);
        }
    }
    out
}

pub(crate) fn split_prose_ranges(
    source: &str,
    range: Range<usize>,
    target_tokens: usize,
    hard_max_tokens: usize,
) -> Vec<Range<usize>> {
    if range.is_empty() || source[range.clone()].trim().is_empty() {
        return Vec::new();
    }
    if estimate_batch_tokens(&source[range.clone()]) <= target_tokens {
        return vec![range];
    }

    let bytes = source.as_bytes();
    let mut paragraphs: Vec<Range<usize>> = Vec::new();
    let mut p_start = range.start;
    let mut i = range.start;
    while i < range.end {
        if bytes[i] == b'\n' {
            let mut j = i + 1;
            let mut saw_second_newline = false;
            while j < range.end && matches!(bytes[j], b' ' | b'\t' | b'\r' | b'\n') {
                if bytes[j] == b'\n' {
                    saw_second_newline = true;
                }
                j += 1;
            }
            if saw_second_newline {
                paragraphs.push(p_start..j);
                p_start = j;
                i = j;
                continue;
            }
        }
        i += 1;
    }
    if p_start < range.end {
        paragraphs.push(p_start..range.end);
    }

    let mut units: Vec<Range<usize>> = Vec::new();
    for para in paragraphs {
        if estimate_batch_tokens(&source[para.clone()]) <= hard_max_tokens {
            units.push(para);
            continue;
        }
        let mut line_start = para.start;
        let mut k = para.start;
        while k < para.end {
            if bytes[k] == b'\n' {
                let line_range = line_start..(k + 1);
                units.extend(split_oversized_line_ranges(
                    source,
                    line_range,
                    target_tokens,
                    hard_max_tokens,
                ));
                line_start = k + 1;
            }
            k += 1;
        }
        if line_start < para.end {
            units.extend(split_oversized_line_ranges(
                source,
                line_start..para.end,
                target_tokens,
                hard_max_tokens,
            ));
        }
    }

    pack_contiguous_ranges(source, &units, target_tokens)
}

pub(crate) fn plain_text_bytes(bytes: &[u8]) -> Result<Vec<ChunkInput>> {
    let (source, utf8_offset) = decode_with_utf8_offset(bytes)?;
    plain_text_str(&source, utf8_offset)
}

pub(crate) fn plain_text_str(source: &str, byte_offset: Option<u64>) -> Result<Vec<ChunkInput>> {
    if source.is_empty() {
        return Ok(vec![ChunkInput {
            heading: String::new(),
            body: String::new(),
            anchor: line_anchor_with_offset(source, 0..0, byte_offset)?,
        }]);
    }
    let ranges = split_prose_ranges(
        source,
        0..source.len(),
        TARGET_CHUNK_TOKENS,
        HARD_MAX_CHUNK_TOKENS,
    );
    if ranges.is_empty() {
        return Ok(Vec::new());
    }
    let mut chunks = Vec::with_capacity(ranges.len());
    for r in ranges {
        chunks.push(ChunkInput {
            heading: String::new(),
            body: source[r.clone()].to_owned(),
            anchor: line_anchor_with_offset(source, r, byte_offset)?,
        });
    }
    Ok(chunks)
}

struct CsvRecord {
    range: Range<usize>,
    fields: Vec<String>,
}

fn parse_rfc4180_records(source: &str) -> Vec<CsvRecord> {
    let bytes = source.as_bytes();
    let len = bytes.len();
    let mut records = Vec::new();
    let mut pos = 0usize;

    while pos < len {
        let record_start = pos;
        let mut fields = Vec::new();
        let mut field = String::new();
        let mut in_quotes = false;

        while pos < len {
            let b = bytes[pos];
            if in_quotes {
                if b == b'"' {
                    if bytes.get(pos + 1) == Some(&b'"') {
                        field.push('"');
                        pos += 2;
                    } else {
                        in_quotes = false;
                        pos += 1;
                    }
                } else {
                    let ch = source[pos..].chars().next().unwrap_or('\u{FFFD}');
                    field.push(ch);
                    pos += ch.len_utf8();
                }
            } else {
                match b {
                    b'"' if field.is_empty() => {
                        in_quotes = true;
                        pos += 1;
                    }
                    b',' => {
                        fields.push(std::mem::take(&mut field));
                        pos += 1;
                    }
                    b'\r' => {
                        pos += 1;
                        if bytes.get(pos) == Some(&b'\n') {
                            pos += 1;
                        }
                        break;
                    }
                    b'\n' => {
                        pos += 1;
                        break;
                    }
                    _ => {
                        let ch = source[pos..].chars().next().unwrap_or('\u{FFFD}');
                        field.push(ch);
                        pos += ch.len_utf8();
                    }
                }
            }
        }
        fields.push(field);
        if !source[record_start..pos].trim().is_empty() {
            records.push(CsvRecord {
                range: record_start..pos,
                fields,
            });
        }
    }
    records
}

pub(crate) fn csv_bytes(bytes: &[u8]) -> Result<Vec<ChunkInput>> {
    let (source, utf8_offset) = decode_with_utf8_offset(bytes)?;
    csv_str(&source, utf8_offset)
}

pub(crate) fn csv_str(source: &str, byte_offset: Option<u64>) -> Result<Vec<ChunkInput>> {
    let records = parse_rfc4180_records(source);
    if records.is_empty() {
        return Ok(Vec::new());
    }
    let cols = records[0]
        .fields
        .iter()
        .map(|c| c.trim())
        .filter(|c| !c.is_empty())
        .collect::<Vec<_>>()
        .join(" | ");
    let heading = if cols.is_empty() {
        "CSV".to_owned()
    } else {
        format!("CSV [Columns: {cols}]")
    };

    let mut chunks = Vec::new();
    let mut batch_start = records[0].range.start;
    let mut batch_end = records[0].range.end;

    for rec in records.iter().skip(1) {
        let candidate = batch_start..rec.range.end;
        if estimate_batch_tokens(&source[candidate.clone()]) <= TARGET_CHUNK_TOKENS {
            batch_end = rec.range.end;
        } else {
            chunks.push(ChunkInput {
                heading: heading.clone(),
                body: source[batch_start..batch_end].to_owned(),
                anchor: line_anchor_with_offset(source, batch_start..batch_end, byte_offset)?,
            });
            batch_start = rec.range.start;
            batch_end = rec.range.end;
            if estimate_batch_tokens(&source[batch_start..batch_end]) > HARD_MAX_CHUNK_TOKENS {
                for sub in split_prose_ranges(
                    source,
                    batch_start..batch_end,
                    TARGET_CHUNK_TOKENS,
                    HARD_MAX_CHUNK_TOKENS,
                ) {
                    chunks.push(ChunkInput {
                        heading: heading.clone(),
                        body: source[sub.clone()].to_owned(),
                        anchor: line_anchor_with_offset(source, sub, byte_offset)?,
                    });
                }
                batch_start = batch_end;
            }
        }
    }

    if batch_start < batch_end && !source[batch_start..batch_end].trim().is_empty() {
        chunks.push(ChunkInput {
            heading,
            body: source[batch_start..batch_end].to_owned(),
            anchor: line_anchor_with_offset(source, batch_start..batch_end, byte_offset)?,
        });
    }
    Ok(chunks)
}

#[derive(Clone, Debug)]
struct MdSegment {
    heading: String,
    range: Range<usize>,
    mergeable: bool,
    is_colon_lead_in: Option<String>,
}

pub(crate) fn markdown(source: &str) -> Result<Vec<ChunkInput>> {
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut breadcrumbs: Vec<(u8, String)> = Vec::new();
    let mut pending_heading_start: Option<usize> = None;
    let mut segments: Vec<MdSegment> = Vec::new();

    let mut block_depth = 0usize;
    let mut in_heading: Option<(usize, u8, String)> = None;

    let mut in_paragraph: Option<(usize, String)> = None;

    let mut in_table: Option<usize> = None;
    let mut in_table_head = false;
    let mut current_head_cell: Option<String> = None;
    let mut table_headers: Vec<String> = Vec::new();
    let mut table_row_ranges: Vec<Range<usize>> = Vec::new();

    let mut list_depth = 0usize;
    let mut outer_list_start: Option<usize> = None;
    let mut outer_item_ranges: Vec<Range<usize>> = Vec::new();

    let mut in_code_block: Option<(usize, Option<String>)> = None;
    let mut generic_block_start: Option<usize> = None;

    for (event, range) in Parser::new_ext(source, options).into_offset_iter() {
        match event {
            Event::Start(tag) => {
                if block_depth == 0 {
                    match &tag {
                        Tag::Heading { level, .. } => {
                            in_heading = Some((range.start, *level as u8, String::new()));
                        }
                        Tag::Paragraph => {
                            in_paragraph = Some((range.start, String::new()));
                        }
                        Tag::Table(_) => {
                            in_table = Some(range.start);
                            table_headers.clear();
                            table_row_ranges.clear();
                        }
                        Tag::List(_) => {
                            outer_list_start = Some(range.start);
                            outer_item_ranges.clear();
                            list_depth = 1;
                        }
                        Tag::CodeBlock(kind) => {
                            let lang = match kind {
                                CodeBlockKind::Fenced(info) => info
                                    .split_whitespace()
                                    .next()
                                    .filter(|s| !s.is_empty())
                                    .map(str::to_owned),
                                CodeBlockKind::Indented => None,
                            };
                            in_code_block = Some((range.start, lang));
                        }
                        _ => {
                            generic_block_start = Some(range.start);
                        }
                    }
                } else {
                    match &tag {
                        Tag::List(_) if outer_list_start.is_some() => {
                            list_depth += 1;
                        }
                        Tag::Item if list_depth == 1 => {
                            outer_item_ranges.push(range.clone());
                        }
                        Tag::TableHead if in_table.is_some() => {
                            in_table_head = true;
                        }
                        Tag::TableCell if in_table_head => {
                            current_head_cell = Some(String::new());
                        }
                        Tag::TableRow if in_table.is_some() && !in_table_head => {
                            table_row_ranges.push(range.clone());
                        }
                        _ => {}
                    }
                }
                block_depth += 1;
            }
            Event::End(tag_end) => {
                block_depth = block_depth.saturating_sub(1);
                if block_depth > 0 {
                    match tag_end {
                        TagEnd::List(_) if list_depth > 1 => {
                            list_depth -= 1;
                        }
                        TagEnd::Item if list_depth == 1 => {
                            if let Some(last) = outer_item_ranges.last_mut() {
                                last.end = extend_line_end(source, range.end);
                            }
                        }
                        TagEnd::TableCell if in_table_head => {
                            if let Some(cell) = current_head_cell.take() {
                                table_headers.push(cell.trim().to_owned());
                            }
                        }
                        TagEnd::TableHead => {
                            in_table_head = false;
                        }
                        TagEnd::TableRow if in_table.is_some() => {
                            if let Some(last) = table_row_ranges.last_mut() {
                                last.end = extend_line_end(source, range.end);
                            }
                        }
                        _ => {}
                    }
                    continue;
                }

                match tag_end {
                    TagEnd::Heading(_) => {
                        if let Some((h_start, level, h_text)) = in_heading.take() {
                            let clean = h_text.trim().to_owned();
                            breadcrumbs.retain(|(d, _)| *d < level);
                            breadcrumbs.push((level, clean));
                            pending_heading_start = Some(h_start);
                        }
                    }
                    TagEnd::Paragraph => {
                        if let Some((p_start, p_text)) = in_paragraph.take() {
                            let start = pending_heading_start.take().unwrap_or(p_start);
                            let end = extend_line_end(source, range.end);
                            let heading = format_breadcrumbs(&breadcrumbs);
                            let trimmed_p = p_text.trim();
                            let colon_lead = (trimmed_p.ends_with(':')
                                || trimmed_p.ends_with('：'))
                            .then(|| trimmed_p.to_owned());
                            emit_prose_or_split(
                                &mut segments,
                                source,
                                heading,
                                start..end,
                                colon_lead,
                            );
                        }
                    }
                    TagEnd::Table => {
                        if let Some(t_start) = in_table.take() {
                            let start = pending_heading_start.take().unwrap_or(t_start);
                            let end = extend_line_end(source, range.end);
                            let heading = format_breadcrumbs(&breadcrumbs);
                            emit_markdown_table(
                                &mut segments,
                                source,
                                heading,
                                start..end,
                                &table_headers,
                                &table_row_ranges,
                            );
                        }
                    }
                    TagEnd::List(_) => {
                        list_depth = 0;
                        if let Some(l_start) = outer_list_start.take() {
                            let start = pending_heading_start.take().unwrap_or(l_start);
                            let end = extend_line_end(source, range.end);
                            let heading = format_breadcrumbs(&breadcrumbs);
                            emit_markdown_list(
                                &mut segments,
                                source,
                                heading,
                                start..end,
                                &outer_item_ranges,
                            );
                        }
                    }
                    TagEnd::CodeBlock => {
                        if let Some((c_start, lang)) = in_code_block.take() {
                            let start = pending_heading_start.take().unwrap_or(c_start);
                            let end = extend_line_end(source, range.end);
                            let heading = format_breadcrumbs(&breadcrumbs);
                            emit_markdown_code(
                                &mut segments,
                                source,
                                heading,
                                start..end,
                                lang.as_deref(),
                            );
                        }
                    }
                    _ => {
                        if let Some(g_start) = generic_block_start.take() {
                            let start = pending_heading_start.take().unwrap_or(g_start);
                            let end = extend_line_end(source, range.end);
                            let heading = format_breadcrumbs(&breadcrumbs);
                            emit_prose_or_split(&mut segments, source, heading, start..end, None);
                        }
                    }
                }
            }
            Event::Text(text) | Event::Code(text) => {
                if let Some((_, _, h_buf)) = &mut in_heading {
                    push_heading_text(h_buf, &text);
                } else if let Some((_, p_buf)) = &mut in_paragraph {
                    p_buf.push_str(&text);
                } else if let Some(cell_buf) = &mut current_head_cell {
                    push_heading_text(cell_buf, &text);
                } else if block_depth == 0 {
                    let start = pending_heading_start.take().unwrap_or(range.start);
                    let end = extend_line_end(source, range.end);
                    let heading = format_breadcrumbs(&breadcrumbs);
                    emit_prose_or_split(&mut segments, source, heading, start..end, None);
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if let Some((_, _, h_buf)) = &mut in_heading {
                    push_heading_break(h_buf);
                } else if let Some((_, p_buf)) = &mut in_paragraph {
                    p_buf.push(' ');
                } else if let Some(cell_buf) = &mut current_head_cell {
                    push_heading_break(cell_buf);
                }
            }
            Event::Html(_) | Event::InlineHtml(_) if block_depth == 0 => {
                let start = pending_heading_start.take().unwrap_or(range.start);
                let end = extend_line_end(source, range.end);
                let heading = format_breadcrumbs(&breadcrumbs);
                emit_prose_or_split(&mut segments, source, heading, start..end, None);
            }
            _ => {}
        }
    }

    if segments.is_empty() {
        if !source.trim().is_empty() {
            return Ok(vec![ChunkInput {
                heading: format_breadcrumbs(&breadcrumbs),
                body: source.to_owned(),
                anchor: line_anchor(source, 0..source.len())?,
            }]);
        }
        return Ok(Vec::new());
    }

    let mut merged: Vec<MdSegment> = Vec::with_capacity(segments.len());
    for seg in segments {
        if source[seg.range.clone()].trim().is_empty() {
            continue;
        }
        if let Some(prev) = merged.last_mut() {
            if prev.mergeable
                && seg.mergeable
                && prev.heading == seg.heading
                && seg.range.start >= prev.range.end
                && source[prev.range.end..seg.range.start].trim().is_empty()
            {
                let combined = prev.range.start..seg.range.end;
                if estimate_batch_tokens(&source[combined.clone()]) <= TARGET_CHUNK_TOKENS {
                    prev.range = combined;
                    prev.is_colon_lead_in = seg.is_colon_lead_in;
                    continue;
                }
            }
        }
        merged.push(seg);
    }

    if let Some(last) = merged.last_mut() {
        if pending_heading_start.is_none()
            && last.range.end < source.len()
            && source[last.range.end..].trim().is_empty()
        {
            last.range.end = source.len();
        }
    }

    let mut chunks = Vec::with_capacity(merged.len());
    for seg in merged {
        chunks.push(ChunkInput {
            heading: seg.heading,
            body: source[seg.range.clone()].to_owned(),
            anchor: line_anchor(source, seg.range)?,
        });
    }
    Ok(chunks)
}

fn emit_prose_or_split(
    segments: &mut Vec<MdSegment>,
    source: &str,
    heading: String,
    range: Range<usize>,
    colon_lead: Option<String>,
) {
    if estimate_batch_tokens(&source[range.clone()]) <= HARD_MAX_CHUNK_TOKENS {
        segments.push(MdSegment {
            heading,
            range,
            mergeable: true,
            is_colon_lead_in: colon_lead,
        });
        return;
    }
    let split = split_prose_ranges(source, range, TARGET_CHUNK_TOKENS, HARD_MAX_CHUNK_TOKENS);
    let last_idx = split.len().saturating_sub(1);
    for (idx, sub) in split.into_iter().enumerate() {
        segments.push(MdSegment {
            heading: heading.clone(),
            range: sub,
            mergeable: false,
            is_colon_lead_in: (idx == last_idx).then(|| colon_lead.clone()).flatten(),
        });
    }
}

fn emit_markdown_table(
    segments: &mut Vec<MdSegment>,
    source: &str,
    heading: String,
    range: Range<usize>,
    headers: &[String],
    row_ranges: &[Range<usize>],
) {
    if estimate_batch_tokens(&source[range.clone()]) <= HARD_MAX_CHUNK_TOKENS
        || row_ranges.is_empty()
    {
        segments.push(MdSegment {
            heading,
            range,
            mergeable: true,
            is_colon_lead_in: None,
        });
        return;
    }

    let cols = headers
        .iter()
        .filter(|h| !h.is_empty())
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(" | ");
    let cont_heading = if cols.is_empty() {
        heading.clone()
    } else {
        with_heading_suffix(&heading, &format!("[Columns: {cols}]"))
    };

    let mut slice_start = range.start;
    let mut slice_end = row_ranges[0].end;
    let mut slice_idx = 0usize;

    for row in row_ranges.iter().skip(1) {
        let candidate = slice_start..row.end;
        if estimate_batch_tokens(&source[candidate]) <= TARGET_CHUNK_TOKENS {
            slice_end = row.end;
        } else {
            let h = if slice_idx == 0 {
                heading.clone()
            } else {
                cont_heading.clone()
            };
            segments.push(MdSegment {
                heading: h,
                range: slice_start..slice_end,
                mergeable: false,
                is_colon_lead_in: None,
            });
            slice_idx += 1;
            slice_start = slice_end;
            slice_end = row.end;
        }
    }
    if slice_start < range.end {
        let h = if slice_idx == 0 {
            heading
        } else {
            cont_heading
        };
        segments.push(MdSegment {
            heading: h,
            range: slice_start..range.end,
            mergeable: false,
            is_colon_lead_in: None,
        });
    }
}

fn emit_markdown_list(
    segments: &mut Vec<MdSegment>,
    source: &str,
    heading: String,
    mut range: Range<usize>,
    item_ranges: &[Range<usize>],
) {
    let mut lead_in_text: Option<String> = None;
    if let Some(prev) = segments.last() {
        if prev.heading == heading
            && prev.is_colon_lead_in.is_some()
            && range.start >= prev.range.end
            && source[prev.range.end..range.start].trim().is_empty()
        {
            if let Some(popped) = segments.pop() {
                lead_in_text = popped.is_colon_lead_in;
                range.start = popped.range.start;
            }
        }
    }

    if estimate_batch_tokens(&source[range.clone()]) <= HARD_MAX_CHUNK_TOKENS
        || item_ranges.is_empty()
    {
        segments.push(MdSegment {
            heading,
            range,
            mergeable: true,
            is_colon_lead_in: None,
        });
        return;
    }

    let cont_heading = match lead_in_text.as_deref() {
        Some(ctx) => with_heading_suffix(&heading, &format!("[Context: {ctx}]")),
        None => heading.clone(),
    };

    let mut slice_start = range.start;
    let mut slice_end = item_ranges[0].end;
    let mut slice_idx = 0usize;

    for item in item_ranges.iter().skip(1) {
        let candidate = slice_start..item.end;
        if estimate_batch_tokens(&source[candidate]) <= TARGET_CHUNK_TOKENS {
            slice_end = item.end;
        } else {
            let h = if slice_idx == 0 {
                heading.clone()
            } else {
                cont_heading.clone()
            };
            segments.push(MdSegment {
                heading: h,
                range: slice_start..slice_end,
                mergeable: false,
                is_colon_lead_in: None,
            });
            slice_idx += 1;
            slice_start = slice_end;
            slice_end = item.end;
        }
    }
    if slice_start < range.end {
        let h = if slice_idx == 0 {
            heading
        } else {
            cont_heading
        };
        segments.push(MdSegment {
            heading: h,
            range: slice_start..range.end,
            mergeable: false,
            is_colon_lead_in: None,
        });
    }
}

fn emit_markdown_code(
    segments: &mut Vec<MdSegment>,
    source: &str,
    heading: String,
    range: Range<usize>,
    lang: Option<&str>,
) {
    if estimate_batch_tokens(&source[range.clone()]) <= HARD_MAX_CHUNK_TOKENS {
        segments.push(MdSegment {
            heading,
            range,
            mergeable: true,
            is_colon_lead_in: None,
        });
        return;
    }
    let cont_heading = match lang {
        Some(l) if !l.is_empty() => with_heading_suffix(&heading, &format!("[Code: {l}]")),
        _ => heading.clone(),
    };
    let split = split_prose_ranges(source, range, TARGET_CHUNK_TOKENS, HARD_MAX_CHUNK_TOKENS);
    for (idx, sub) in split.into_iter().enumerate() {
        segments.push(MdSegment {
            heading: if idx == 0 {
                heading.clone()
            } else {
                cont_heading.clone()
            },
            range: sub,
            mergeable: false,
            is_colon_lead_in: None,
        });
    }
}

pub(crate) fn markdown_bytes(bytes: &[u8]) -> Result<Vec<ChunkInput>> {
    // Markdown byte spans address the immutable original, not a transcoded copy.
    let source = std::str::from_utf8(bytes)
        .map_err(|_| IngestError::Invalid("Markdown must be UTF-8 for exact byte anchors"))?;
    let body = source.strip_prefix('\u{feff}').unwrap_or(source);
    let offset = (source.len() - body.len()) as u64;
    let mut chunks = markdown(body)?;
    for chunk in &mut chunks {
        chunk.anchor.start_byte = chunk.anchor.start_byte.map(|start| start + offset);
        chunk.anchor.end_byte = chunk.anchor.end_byte.map(|end| end + offset);
    }
    Ok(chunks)
}

pub(crate) fn sanitize_pipe_cell(raw: &str) -> String {
    raw.replace('\r', "")
        .replace('\n', " ")
        .replace('|', "\\|")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn format_pipe_row(cells: &[String], num_cols: usize) -> String {
    let mut out = String::from("|");
    for col in 0..num_cols {
        let val = cells.get(col).map(String::as_str).unwrap_or("");
        out.push(' ');
        out.push_str(&sanitize_pipe_cell(val));
        out.push_str(" |");
    }
    out
}

pub(crate) fn format_pipe_separator(num_cols: usize) -> String {
    let mut out = String::from("|");
    for _ in 0..num_cols {
        out.push_str(" --- |");
    }
    out
}

pub(crate) fn render_pipe_table_chunks(
    rows: &[Vec<String>],
    heading: &str,
    base_anchor: &AnchorSpec,
) -> Vec<ChunkInput> {
    let non_empty: Vec<&Vec<String>> = rows
        .iter()
        .filter(|r| r.iter().any(|c| !c.trim().is_empty()))
        .collect();
    if non_empty.is_empty() {
        return Vec::new();
    }
    let num_cols = non_empty.iter().map(|r| r.len()).max().unwrap_or(1).max(1);
    let header_line = format_pipe_row(non_empty[0], num_cols);
    let sep_line = format_pipe_separator(num_cols);
    let header_prefix = format!("{header_line}\n{sep_line}");

    if non_empty.len() == 1 {
        return vec![ChunkInput {
            heading: heading.to_owned(),
            body: header_prefix,
            anchor: base_anchor.clone(),
        }];
    }

    let mut full_body = header_prefix.clone();
    for row in non_empty.iter().skip(1) {
        full_body.push('\n');
        full_body.push_str(&format_pipe_row(row, num_cols));
    }
    if estimate_batch_tokens(&full_body) <= HARD_MAX_CHUNK_TOKENS {
        return vec![ChunkInput {
            heading: heading.to_owned(),
            body: full_body,
            anchor: base_anchor.clone(),
        }];
    }

    let mut chunks = Vec::new();
    let mut cur_body = header_prefix.clone();
    let mut cur_rows = 0usize;

    for row in non_empty.iter().skip(1) {
        let row_line = format_pipe_row(row, num_cols);
        let candidate_len = estimate_batch_tokens(&cur_body) + estimate_batch_tokens(&row_line) + 1;
        if cur_rows > 0 && candidate_len > TARGET_CHUNK_TOKENS {
            chunks.push(ChunkInput {
                heading: heading.to_owned(),
                body: cur_body,
                anchor: base_anchor.clone(),
            });
            cur_body = header_prefix.clone();
            cur_rows = 0;
        }
        cur_body.push('\n');
        cur_body.push_str(&row_line);
        cur_rows += 1;
    }
    if cur_rows > 0 {
        chunks.push(ChunkInput {
            heading: heading.to_owned(),
            body: cur_body,
            anchor: base_anchor.clone(),
        });
    }
    chunks
}
