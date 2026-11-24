// Encoding precedence follows SAG-Wiki/core/src/document_text.rs (MIT).
use std::borrow::Cow;
use std::sync::LazyLock;

use encoding_rs::{Encoding, UTF_8};
use kp_model::AnchorSpec;
use kp_store::ChunkInput;
use pulldown_cmark::{Event, Parser, Tag, TagEnd};

use crate::{IngestError, Result};

static ENCODING_DECLARATION: LazyLock<std::result::Result<regex::Regex, regex::Error>> =
    LazyLock::new(|| {
        regex::Regex::new(
            r#"(?i)(?:\A(?:[^\r\n]*\r?\n)?[ \t]*\#[^\r\n]*?coding\s*[:=]\s*|<meta\b[^>]*?charset\s*=\s*["']?|\A<\?xml\b[^>]*?encoding\s*=\s*["'])([a-z0-9._-]+)"#,
        )
    });

pub(crate) fn decode(bytes: &[u8]) -> Result<Cow<'_, str>> {
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
            return Ok(Cow::Borrowed(text));
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
    Ok(text)
}

pub(crate) fn line_anchor(source: &str, range: std::ops::Range<usize>) -> Result<AnchorSpec> {
    let start = source[..range.start]
        .bytes()
        .filter(|b| *b == b'\n')
        .count()
        + 1;
    let end = source[..range.end].bytes().filter(|b| *b == b'\n').count()
        + usize::from(!source[..range.end].ends_with('\n'));
    Ok(AnchorSpec {
        start_byte: Some(range.start as u64),
        end_byte: Some(range.end as u64),
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

pub(crate) fn markdown(source: &str) -> Result<Vec<ChunkInput>> {
    let mut sections = Vec::new();
    let mut heading = String::new();
    let mut heading_start = None;
    let mut breadcrumbs: Vec<(u8, String)> = Vec::new();
    for (event, range) in Parser::new(source).into_offset_iter() {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                heading.clear();
                heading_start = Some((range.start, level as u8));
            }
            Event::Text(text) | Event::Code(text) if heading_start.is_some() => {
                heading.push_str(&text)
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some((start, level)) = heading_start.take() {
                    breadcrumbs.retain(|(depth, _)| *depth < level);
                    breadcrumbs.push((level, heading.clone()));
                    sections.push((
                        start,
                        breadcrumbs
                            .iter()
                            .map(|(_, h)| h.as_str())
                            .collect::<Vec<_>>()
                            .join(" > "),
                    ));
                }
            }
            _ => {}
        }
    }
    if sections.first().is_none_or(|(start, _)| *start > 0) {
        sections.insert(0, (0, String::new()));
    }
    let mut chunks = Vec::new();
    for (i, (start, heading)) in sections.iter().enumerate() {
        let end = sections.get(i + 1).map(|s| s.0).unwrap_or(source.len());
        if !source[*start..end].trim().is_empty() {
            chunks.push(ChunkInput {
                heading: heading.clone(),
                body: source[*start..end].into(),
                anchor: line_anchor(source, *start..end)?,
            });
        }
    }
    Ok(chunks)
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
