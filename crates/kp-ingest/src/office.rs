// Restricted text-part selection and aggregate ZIP/XML limits adapt SAG-Wiki/office_text.rs (MIT).
use std::collections::{BTreeMap, BTreeSet};
use std::io::Cursor;

use kp_model::AnchorSpec;
use kp_store::ChunkInput;
use quick_xml::{events::Event, Reader};
use zip::ZipArchive;

use crate::{IngestError, Result};

pub(crate) fn parse(bytes: &[u8], extension: &str, cap: usize) -> Result<Vec<ChunkInput>> {
    let mut zip = ZipArchive::new(Cursor::new(bytes))?;
    if zip.len() > 10_000 {
        return Err(IngestError::Invalid("too many Office ZIP entries"));
    }
    let mut names = Vec::new();
    let mut total = 0;
    for name in zip.file_names() {
        if name.len() > 4096
            || name.contains('\\')
            || name.starts_with('/')
            || name.split('/').any(|p| p == ".." || p == ".")
        {
            return Err(IngestError::Invalid("invalid Office ZIP path"));
        }
        if allowed(name, extension) {
            names.push(name.to_owned());
        }
    }
    let mut parts = BTreeMap::new();
    for name in names {
        let entry = zip.by_name(&name)?;
        if entry.size() > cap.saturating_sub(total) as u64 {
            return Err(IngestError::Limit(cap));
        }
        let bytes = crate::read_bounded(entry, cap.saturating_sub(total))?;
        total += bytes.len();
        parts.insert(name, bytes);
    }
    let mut output = Output {
        chunks: Vec::new(),
        bytes: 0,
        cap,
        max_chunks: 10_000,
    };
    match extension {
        "docx" => paragraphs(part(&parts, "word/document.xml")?, None, &mut output)?,
        "pptx" => slides(&parts, &mut output)?,
        "xlsx" => sheets(&parts, &mut output)?,
        _ => return Err(IngestError::Invalid("unsupported Office format")),
    }
    Ok(output.chunks)
}

fn allowed(name: &str, extension: &str) -> bool {
    match extension {
        "docx" => name == "word/document.xml",
        "pptx" => {
            matches!(
                name,
                "ppt/presentation.xml" | "ppt/_rels/presentation.xml.rels"
            ) || numbered(name, "ppt/slides/slide")
        }
        "xlsx" => {
            matches!(
                name,
                "xl/workbook.xml" | "xl/_rels/workbook.xml.rels" | "xl/sharedStrings.xml"
            ) || numbered(name, "xl/worksheets/sheet")
        }
        _ => false,
    }
}

fn numbered(name: &str, prefix: &str) -> bool {
    name.strip_prefix(prefix)
        .and_then(|s| s.strip_suffix(".xml"))
        .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
}

fn part<'a>(parts: &'a BTreeMap<String, Vec<u8>>, name: &str) -> Result<&'a [u8]> {
    parts
        .get(name)
        .map(Vec::as_slice)
        .ok_or(IngestError::Invalid("missing Office text part"))
}

struct Output {
    chunks: Vec<ChunkInput>,
    bytes: usize,
    cap: usize,
    max_chunks: usize,
}

impl Output {
    // Reserve before copying shared text or repeated metadata into a chunk.
    fn reserve(&mut self, bytes: usize) -> Result<()> {
        if self.chunks.len() >= self.max_chunks {
            return Err(IngestError::Invalid("too many Office chunks"));
        }
        if bytes > self.cap.saturating_sub(self.bytes) {
            return Err(IngestError::Limit(self.cap));
        }
        self.bytes += bytes;
        Ok(())
    }
}

fn paragraphs(xml: &[u8], slide: Option<u32>, output: &mut Output) -> Result<()> {
    let mut paragraph = 0;
    visit_elements(xml, b"p", output.cap, output.max_chunks, |p| {
        paragraph += 1;
        if p.text.trim().is_empty() {
            return Ok(());
        }
        let heading = slide
            .map(|s| format!("Slide {s}"))
            .unwrap_or_else(|| format!("Paragraph {paragraph}"));
        output.reserve(p.text.len() + heading.len())?;
        output.chunks.push(ChunkInput {
            heading,
            body: p.text,
            anchor: AnchorSpec {
                paragraph: Some(paragraph),
                slide,
                ..AnchorSpec::default()
            },
        });
        Ok(())
    })
}

fn relationships(
    xml: &[u8],
    prefix: &str,
    cap: usize,
    max_elements: usize,
) -> Result<BTreeMap<String, String>> {
    let mut result = BTreeMap::new();
    visit_elements(xml, b"Relationship", cap, max_elements, |rel| {
        if rel.attrs.get("TargetMode").is_some_and(|v| v == "External") {
            return Ok(());
        }
        if let (Some(id), Some(target)) = (rel.attrs.get("Id"), rel.attrs.get("Target")) {
            if target.split('/').any(|p| p == "..") || target.contains('\\') {
                return Err(IngestError::Invalid("unsafe Office relationship"));
            }
            let target = if target.starts_with('/') {
                target.trim_start_matches('/').into()
            } else {
                format!("{prefix}/{target}")
            };
            result.insert(id.clone(), target);
        }
        Ok(())
    })?;
    Ok(result)
}

fn numbered_parts(parts: &BTreeMap<String, Vec<u8>>, prefix: &str) -> Vec<String> {
    let mut names: Vec<_> = parts
        .keys()
        .filter(|n| numbered(n, prefix))
        .cloned()
        .collect();
    names.sort_by_key(|n| {
        n.trim_start_matches(prefix)
            .trim_end_matches(".xml")
            .parse::<u32>()
            .unwrap_or(u32::MAX)
    });
    names
}

fn slides(parts: &BTreeMap<String, Vec<u8>>, output: &mut Output) -> Result<()> {
    let cap = output.cap;
    let max_chunks = output.max_chunks;
    let mut seen = BTreeSet::new();
    let mut slide = 0;
    let mut emit = |name: &str| {
        if seen.insert(name.to_owned()) {
            slide += 1;
            paragraphs(part(parts, name)?, Some(slide), output)?;
        }
        Ok(())
    };
    if let (Some(presentation), Some(rels)) = (
        parts.get("ppt/presentation.xml"),
        parts.get("ppt/_rels/presentation.xml.rels"),
    ) {
        let relationships = relationships(rels, "ppt", cap, max_chunks)?;
        visit_elements(presentation, b"sldId", cap, max_chunks, |s| {
            let path = s
                .attrs
                .get("r:id")
                .and_then(|id| relationships.get(id))
                .ok_or(IngestError::Invalid("unresolved slide relationship"))?;
            emit(path)
        })
    } else {
        for name in numbered_parts(parts, "ppt/slides/slide") {
            emit(&name)?;
        }
        Ok(())
    }
}

fn sheets(parts: &BTreeMap<String, Vec<u8>>, output: &mut Output) -> Result<()> {
    let shared = parts
        .get("xl/sharedStrings.xml")
        .map(|xml| elements(xml, b"si", output.cap, output.max_chunks))
        .transpose()?
        .unwrap_or_default();
    let cap = output.cap;
    let max_chunks = output.max_chunks;
    let mut seen = BTreeSet::new();
    let mut emit = |path: &str, sheet: &str| {
        if !seen.insert(path.to_owned()) {
            return Ok(());
        }
        visit_elements(part(parts, path)?, b"c", cap, max_chunks, |cell| {
            let body = if cell.attrs.get("t").is_some_and(|t| t == "s") {
                let index: usize = cell
                    .text
                    .trim()
                    .parse()
                    .map_err(|_| IngestError::Invalid("invalid shared string index"))?;
                &shared
                    .get(index)
                    .ok_or(IngestError::Invalid("missing shared string"))?
                    .text
            } else {
                &cell.text
            };
            if body.trim().is_empty() {
                return Ok(());
            }
            let coordinate = cell
                .attrs
                .get("r")
                .ok_or(IngestError::Invalid("missing cell coordinate"))?;
            output.reserve(body.len() + 2 * sheet.len() + 2 * coordinate.len() + 1)?;
            output.chunks.push(ChunkInput {
                heading: format!("{sheet}!{coordinate}"),
                body: body.clone(),
                anchor: AnchorSpec {
                    sheet: Some(sheet.into()),
                    cell: Some(coordinate.clone()),
                    ..AnchorSpec::default()
                },
            });
            Ok(())
        })
    };
    if let (Some(workbook), Some(rels)) = (
        parts.get("xl/workbook.xml"),
        parts.get("xl/_rels/workbook.xml.rels"),
    ) {
        let relationships = relationships(rels, "xl", cap, max_chunks)?;
        visit_elements(workbook, b"sheet", cap, max_chunks, |s| {
            let path = s
                .attrs
                .get("r:id")
                .and_then(|id| relationships.get(id))
                .ok_or(IngestError::Invalid("unresolved sheet relationship"))?;
            let name = s
                .attrs
                .get("name")
                .ok_or(IngestError::Invalid("missing sheet name"))?;
            emit(path, name)
        })
    } else {
        for path in numbered_parts(parts, "xl/worksheets/sheet") {
            emit(&path, &path)?;
        }
        Ok(())
    }
}

#[derive(Default)]
struct Element {
    attrs: BTreeMap<String, String>,
    text: String,
}

fn elements(xml: &[u8], selected: &[u8], cap: usize, max_elements: usize) -> Result<Vec<Element>> {
    let mut result = Vec::new();
    visit_elements(xml, selected, cap, max_elements, |element| {
        result.push(element);
        Ok(())
    })?;
    Ok(result)
}

fn visit_elements(
    xml: &[u8],
    selected: &[u8],
    cap: usize,
    max_elements: usize,
    mut emit: impl FnMut(Element) -> Result<()>,
) -> Result<()> {
    let mut reader = Reader::from_reader(xml);
    let mut count = 0;
    let mut text_bytes = 0;
    let mut capture: Option<(usize, Element)> = None;
    let mut stack = Vec::new();
    let mut nodes = 0;
    loop {
        nodes += 1;
        if nodes > 1_000_000 {
            return Err(IngestError::Invalid("too many XML nodes"));
        }
        match reader.read_event()? {
            event @ (Event::Start(_) | Event::Empty(_)) => {
                let empty = matches!(event, Event::Empty(_));
                let e = match event {
                    Event::Start(e) | Event::Empty(e) => e,
                    _ => return Err(IngestError::Invalid("invalid XML element")),
                };
                let name = e.local_name();
                if matches!(name.as_ref(), b"binData" | b"binaryData") {
                    return Err(IngestError::Invalid(
                        "inline Office binary data is unsupported",
                    ));
                }
                // Nested paragraphs belong to the outer capture, in document order.
                if name.as_ref() == selected && capture.is_none() {
                    count += 1;
                    if count > max_elements {
                        return Err(IngestError::Invalid("too many Office XML elements"));
                    }
                    let mut element = Element::default();
                    for attr in e.attributes() {
                        let attr =
                            attr.map_err(|_| IngestError::Invalid("invalid XML attribute"))?;
                        let key = std::str::from_utf8(attr.key.as_ref())
                            .map_err(|_| IngestError::Invalid("invalid XML attribute name"))?;
                        element.attrs.insert(
                            key.into(),
                            attr.decode_and_unescape_value(reader.decoder())?
                                .into_owned(),
                        );
                    }
                    if empty {
                        emit(element)?;
                    } else {
                        capture = Some((stack.len(), element));
                    }
                }
                if let Some((_, element)) = &mut capture {
                    match name.as_ref() {
                        b"tab" => append_text(element, "\t", &mut text_bytes, cap)?,
                        b"br" => append_text(element, "\n", &mut text_bytes, cap)?,
                        _ => {}
                    }
                }
                if !empty {
                    stack.push(name.as_ref().to_vec());
                }
                if stack.len() > 128 {
                    return Err(IngestError::Invalid("Office XML nesting limit exceeded"));
                }
            }
            Event::End(_) => {
                stack.pop().ok_or(IngestError::Invalid("unbalanced XML"))?;
                if capture
                    .as_ref()
                    .is_some_and(|(depth, _)| *depth == stack.len())
                {
                    if let Some((_, element)) = capture.take() {
                        emit(element)?;
                    }
                }
            }
            Event::Text(e) => {
                if let Some((_, element)) = &mut capture {
                    if stack
                        .last()
                        .is_some_and(|n| matches!(n.as_slice(), b"t" | b"v"))
                    {
                        append_text(
                            element,
                            &e.decode()
                                .map_err(|_| IngestError::Invalid("invalid XML text"))?,
                            &mut text_bytes,
                            cap,
                        )?;
                    }
                }
            }
            Event::GeneralRef(e) => {
                if let Some((_, element)) = &mut capture {
                    if stack
                        .last()
                        .is_some_and(|n| matches!(n.as_slice(), b"t" | b"v"))
                    {
                        let name = e
                            .decode()
                            .map_err(|_| IngestError::Invalid("invalid XML reference"))?;
                        let reference = format!("&{name};");
                        append_text(
                            element,
                            &quick_xml::escape::unescape(&reference)
                                .map_err(|_| IngestError::Invalid("unknown XML entity"))?,
                            &mut text_bytes,
                            cap,
                        )?;
                    }
                }
            }
            Event::CData(e) => {
                if let Some((_, element)) = &mut capture {
                    if stack
                        .last()
                        .is_some_and(|n| matches!(n.as_slice(), b"t" | b"v"))
                    {
                        append_text(
                            element,
                            std::str::from_utf8(&e)
                                .map_err(|_| IngestError::Invalid("invalid XML CDATA"))?,
                            &mut text_bytes,
                            cap,
                        )?;
                    }
                }
            }
            Event::DocType(_) => return Err(IngestError::Invalid("Office XML DTD is forbidden")),
            Event::Eof => {
                if !stack.is_empty() {
                    return Err(IngestError::Invalid("incomplete XML"));
                }
                break;
            }
            _ => {}
        }
    }
    Ok(())
}

fn append_text(element: &mut Element, text: &str, bytes: &mut usize, cap: usize) -> Result<()> {
    if text.len() > cap.saturating_sub(*bytes) {
        return Err(IngestError::Limit(cap));
    }
    *bytes += text.len();
    element.text.push_str(text);
    Ok(())
}
