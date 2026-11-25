// Restricted text-part selection and aggregate ZIP/XML limits adapt SAG-Wiki/office_text.rs (MIT).
use std::collections::{BTreeMap, BTreeSet};
use std::io::Cursor;

use kp_model::{estimate_batch_tokens, AnchorSpec};
use kp_store::ChunkInput;
use quick_xml::{
    events::{BytesStart, Event},
    Reader,
};
use zip::ZipArchive;

use crate::text::{
    format_pipe_row, format_pipe_separator, render_pipe_table_chunks, split_prose_ranges,
    HARD_MAX_CHUNK_TOKENS, TARGET_CHUNK_TOKENS,
};
use crate::{IngestError, Result};

const MAX_XML_NODES: usize = 2_000_000;
const MAX_XML_DEPTH: usize = 128;

pub(crate) fn parse(bytes: &[u8], extension: &str, cap: usize) -> Result<Vec<ChunkInput>> {
    let mut zip = ZipArchive::new(Cursor::new(bytes))?;
    if zip.len() > 10_000 {
        return Err(IngestError::Invalid("too many Office ZIP entries"));
    }
    let mut names = Vec::new();
    let mut total = 0;
    for name in zip.file_names() {
        let name = name?;
        let name_str = name.as_ref();
        if name_str.len() > 4096
            || name_str.contains('\\')
            || name_str.starts_with('/')
            || name_str.split('/').any(|p| p == ".." || p == ".")
        {
            return Err(IngestError::Invalid("invalid Office ZIP path"));
        }
        if allowed(name_str, extension) {
            names.push(name.into_owned());
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
        "docx" => parse_docx(&parts, &mut output)?,
        "pptx" => slides(&parts, &mut output)?,
        "xlsx" => sheets(&parts, &mut output)?,
        _ => return Err(IngestError::Invalid("unsupported Office format")),
    }
    Ok(output.chunks)
}

fn allowed(name: &str, extension: &str) -> bool {
    match extension {
        "docx" => matches!(name, "word/document.xml" | "word/styles.xml"),
        "pptx" => {
            matches!(
                name,
                "ppt/presentation.xml" | "ppt/_rels/presentation.xml.rels"
            ) || numbered(name, "ppt/slides/slide")
                || numbered_rels(name, "ppt/slides/_rels/slide")
                || numbered(name, "ppt/notesSlides/notesSlide")
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

fn numbered_rels(name: &str, prefix: &str) -> bool {
    name.strip_prefix(prefix)
        .and_then(|s| s.strip_suffix(".xml.rels"))
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

    fn push_chunk(&mut self, chunk: ChunkInput) -> Result<()> {
        self.reserve(chunk.heading.len().saturating_add(chunk.body.len()))?;
        self.chunks.push(chunk);
        Ok(())
    }
}

fn resolve_opc_target(base_dir: &str, target: &str) -> Result<String> {
    if target.is_empty() || target.contains('\\') {
        return Err(IngestError::Invalid("unsafe Office relationship"));
    }
    let mut segments: Vec<&str> = if target.starts_with('/') {
        Vec::new()
    } else {
        base_dir.split('/').filter(|s| !s.is_empty()).collect()
    };
    for seg in target.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                segments
                    .pop()
                    .ok_or(IngestError::Invalid("unsafe Office relationship"))?;
            }
            other => segments.push(other),
        }
    }
    if segments.is_empty() {
        return Err(IngestError::Invalid("unsafe Office relationship"));
    }
    Ok(segments.join("/"))
}

fn decode_attrs<R: std::io::BufRead>(
    e: &BytesStart<'_>,
    reader: &Reader<R>,
) -> Result<BTreeMap<String, String>> {
    let mut attrs = BTreeMap::new();
    for attr in e.attributes() {
        let attr = attr.map_err(|_| IngestError::Invalid("invalid XML attribute"))?;
        let key = std::str::from_utf8(attr.key.as_ref())
            .map_err(|_| IngestError::Invalid("invalid XML attribute name"))?;
        let val = attr
            .decode_and_unescape_value(reader.decoder())?
            .into_owned();
        attrs.insert(key.to_owned(), val);
    }
    Ok(attrs)
}

fn local_attr<'a>(attrs: &'a BTreeMap<String, String>, local: &str) -> Option<&'a str> {
    if let Some(v) = attrs.get(local) {
        return Some(v.as_str());
    }
    attrs.iter().find_map(|(k, v)| {
        k.rsplit_once(':')
            .and_then(|(_, l)| (l == local).then_some(v.as_str()))
    })
}

fn append_bounded(dst: &mut String, text: &str, used: &mut usize, cap: usize) -> Result<()> {
    if text.len() > cap.saturating_sub(*used) {
        return Err(IngestError::Limit(cap));
    }
    *used += text.len();
    dst.push_str(text);
    Ok(())
}

fn decode_general_ref(raw: &quick_xml::events::BytesRef<'_>) -> Result<String> {
    let name = raw
        .decode()
        .map_err(|_| IngestError::Invalid("invalid XML reference"))?;
    let reference = format!("&{name};");
    Ok(quick_xml::escape::unescape(&reference)
        .map_err(|_| IngestError::Invalid("unknown XML entity"))?
        .into_owned())
}

fn format_breadcrumbs(breadcrumbs: &[(u8, String)]) -> String {
    breadcrumbs
        .iter()
        .map(|(_, h)| h.as_str())
        .filter(|h| !h.is_empty())
        .collect::<Vec<_>>()
        .join(" > ")
}

fn heading_level_from_name(raw: &str) -> Option<u8> {
    let compact: String = raw
        .trim()
        .chars()
        .filter(|c| !c.is_ascii_whitespace() && *c != '-' && *c != '_')
        .map(|c| c.to_ascii_lowercase())
        .collect();
    if compact == "title" {
        return Some(1);
    }
    if compact == "subtitle" {
        return Some(2);
    }
    if let Some(rest) = compact.strip_prefix("heading") {
        if let Ok(lvl) = rest.parse::<u8>() {
            if (1..=6).contains(&lvl) {
                return Some(lvl);
            }
        }
    }
    None
}

fn parse_docx_styles(xml: &[u8], cap: usize) -> Result<BTreeMap<String, u8>> {
    #[derive(Default)]
    struct StyleDef {
        name: Option<String>,
        based_on: Option<String>,
        outline_lvl: Option<u8>,
    }

    let mut reader = Reader::from_reader(xml);
    let mut stack: Vec<Vec<u8>> = Vec::new();
    let mut nodes = 0usize;
    let mut raw_styles: BTreeMap<String, StyleDef> = BTreeMap::new();
    let mut current: Option<(usize, String, StyleDef)> = None;

    loop {
        nodes += 1;
        if nodes > MAX_XML_NODES {
            return Err(IngestError::Invalid("too many XML nodes"));
        }
        match reader.read_event()? {
            event @ (Event::Start(_) | Event::Empty(_)) => {
                let empty = matches!(event, Event::Empty(_));
                let e = match &event {
                    Event::Start(e) | Event::Empty(e) => e,
                    _ => return Err(IngestError::Invalid("invalid XML element")),
                };
                let local = e.local_name();
                if matches!(local.as_ref(), b"binData" | b"binaryData") {
                    return Err(IngestError::Invalid(
                        "inline Office binary data is unsupported",
                    ));
                }
                if local.as_ref() == b"style" && current.is_none() {
                    let attrs = decode_attrs(e, &reader)?;
                    if let Some(style_id) = local_attr(&attrs, "styleId") {
                        if raw_styles.len() < 10_000 {
                            if empty {
                                raw_styles.insert(style_id.to_owned(), StyleDef::default());
                            } else {
                                current =
                                    Some((stack.len(), style_id.to_owned(), StyleDef::default()));
                            }
                        }
                    }
                } else if let Some((_, _, def)) = &mut current {
                    match local.as_ref() {
                        b"name" => {
                            let attrs = decode_attrs(e, &reader)?;
                            if let Some(val) = local_attr(&attrs, "val") {
                                if val.len() <= cap {
                                    def.name = Some(val.to_owned());
                                }
                            }
                        }
                        b"basedOn" => {
                            let attrs = decode_attrs(e, &reader)?;
                            if let Some(val) = local_attr(&attrs, "val") {
                                if val.len() <= cap {
                                    def.based_on = Some(val.to_owned());
                                }
                            }
                        }
                        b"outlineLvl" => {
                            let attrs = decode_attrs(e, &reader)?;
                            if let Some(val) = local_attr(&attrs, "val") {
                                if let Ok(n) = val.trim().parse::<u8>() {
                                    if n <= 5 {
                                        def.outline_lvl = Some(n + 1);
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
                if !empty {
                    stack.push(local.as_ref().to_vec());
                    if stack.len() > MAX_XML_DEPTH {
                        return Err(IngestError::Invalid("Office XML nesting limit exceeded"));
                    }
                }
            }
            Event::End(_) => {
                stack.pop().ok_or(IngestError::Invalid("unbalanced XML"))?;
                if current.as_ref().is_some_and(|(d, _, _)| *d == stack.len()) {
                    if let Some((_, id, def)) = current.take() {
                        raw_styles.insert(id, def);
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

    let mut resolved = BTreeMap::new();
    for id in raw_styles.keys() {
        let mut cursor = Some(id.as_str());
        for _ in 0..8 {
            let Some(cur_id) = cursor else {
                break;
            };
            let Some(def) = raw_styles.get(cur_id) else {
                if let Some(lvl) = heading_level_from_name(cur_id) {
                    resolved.insert(id.clone(), lvl);
                }
                break;
            };
            if let Some(lvl) = def
                .outline_lvl
                .or_else(|| def.name.as_deref().and_then(heading_level_from_name))
                .or_else(|| heading_level_from_name(cur_id))
            {
                resolved.insert(id.clone(), lvl);
                break;
            }
            cursor = def.based_on.as_deref();
        }
    }
    Ok(resolved)
}

enum DocxItem {
    Paragraph {
        style_id: Option<String>,
        outline_lvl: Option<u8>,
        is_list: bool,
        ilvl: u8,
        text: String,
    },
    Table {
        rows: Vec<Vec<String>>,
        paragraph_count: u32,
    },
}

struct PendingList {
    start_para: u32,
    heading_path: String,
    lead_in: Option<String>,
    items: Vec<String>,
}

fn flush_docx_list(pending: &mut Option<PendingList>, output: &mut Output) -> Result<()> {
    let Some(list) = pending.take() else {
        return Ok(());
    };
    if list.items.is_empty() {
        return Ok(());
    }
    let base_heading = if list.heading_path.is_empty() {
        format!("Paragraph {}", list.start_para)
    } else {
        list.heading_path.clone()
    };
    let anchor = AnchorSpec {
        paragraph: Some(list.start_para),
        ..AnchorSpec::default()
    };

    let mut full = String::new();
    if let Some(lead) = &list.lead_in {
        full.push_str(lead);
        full.push('\n');
    }
    full.push_str(&list.items.join("\n"));

    if estimate_batch_tokens(&full) <= HARD_MAX_CHUNK_TOKENS {
        output.push_chunk(ChunkInput {
            heading: base_heading,
            body: full,
            anchor,
        })?;
        return Ok(());
    }

    let cont_heading = match &list.lead_in {
        Some(ctx) => format!("{base_heading} [Context: {ctx}]"),
        None => base_heading.clone(),
    };
    let mut cur_body = list
        .lead_in
        .as_ref()
        .map(|l| format!("{l}\n"))
        .unwrap_or_default();
    let mut cur_count = 0usize;
    let mut slice_idx = 0usize;

    for item in &list.items {
        let candidate_tokens = estimate_batch_tokens(&cur_body) + estimate_batch_tokens(item) + 1;
        if cur_count > 0 && candidate_tokens > TARGET_CHUNK_TOKENS {
            output.push_chunk(ChunkInput {
                heading: if slice_idx == 0 {
                    base_heading.clone()
                } else {
                    cont_heading.clone()
                },
                body: cur_body.trim_end_matches('\n').to_owned(),
                anchor: anchor.clone(),
            })?;
            slice_idx += 1;
            cur_body.clear();
            cur_count = 0;
        }
        if !cur_body.is_empty() && !cur_body.ends_with('\n') {
            cur_body.push('\n');
        }
        cur_body.push_str(item);
        cur_count += 1;
    }
    if cur_count > 0 {
        output.push_chunk(ChunkInput {
            heading: if slice_idx == 0 {
                base_heading
            } else {
                cont_heading
            },
            body: cur_body.trim_end_matches('\n').to_owned(),
            anchor,
        })?;
    }
    Ok(())
}

fn emit_docx_prose(
    text: String,
    heading: String,
    paragraph: u32,
    output: &mut Output,
) -> Result<()> {
    let anchor = AnchorSpec {
        paragraph: Some(paragraph),
        ..AnchorSpec::default()
    };
    if estimate_batch_tokens(&text) <= HARD_MAX_CHUNK_TOKENS {
        output.push_chunk(ChunkInput {
            heading,
            body: text,
            anchor,
        })
    } else {
        for r in split_prose_ranges(
            &text,
            0..text.len(),
            TARGET_CHUNK_TOKENS,
            HARD_MAX_CHUNK_TOKENS,
        ) {
            output.push_chunk(ChunkInput {
                heading: heading.clone(),
                body: text[r].to_owned(),
                anchor: anchor.clone(),
            })?;
        }
        Ok(())
    }
}

fn parse_docx(parts: &BTreeMap<String, Vec<u8>>, output: &mut Output) -> Result<()> {
    let style_levels = match parts.get("word/styles.xml") {
        Some(styles_xml) => parse_docx_styles(styles_xml, output.cap)?,
        None => BTreeMap::new(),
    };
    let doc_xml = part(parts, "word/document.xml")?;
    let items = read_docx_items(doc_xml, output.cap, output.max_chunks)?;

    let mut breadcrumbs: Vec<(u8, String)> = Vec::new();
    let mut paragraph_idx: u32 = 0;
    let mut pending_list: Option<PendingList> = None;
    let mut last_was_plain_colon = false;
    let mut pending_heading_only: Option<(u32, String, String)> = None;

    for item in items {
        match item {
            DocxItem::Paragraph {
                style_id,
                outline_lvl,
                is_list,
                ilvl,
                text,
            } => {
                paragraph_idx += 1;
                if text.trim().is_empty() {
                    continue;
                }
                let heading_lvl = outline_lvl.or_else(|| {
                    style_id.as_deref().and_then(|id| {
                        style_levels
                            .get(id)
                            .copied()
                            .or_else(|| heading_level_from_name(id))
                    })
                });

                if let Some(lvl) = heading_lvl {
                    flush_docx_list(&mut pending_list, output)?;
                    last_was_plain_colon = false;
                    let clean = text.trim().to_owned();
                    breadcrumbs.retain(|(d, _)| *d < lvl);
                    breadcrumbs.push((lvl, clean.clone()));
                    pending_heading_only =
                        Some((paragraph_idx, format_breadcrumbs(&breadcrumbs), clean));
                } else if is_list {
                    pending_heading_only = None;
                    let heading_path = format_breadcrumbs(&breadcrumbs);
                    let indent = "  ".repeat(usize::from(ilvl.min(4)));
                    let bullet = format!("{indent}- {}", text.trim());

                    let can_append = pending_list
                        .as_ref()
                        .is_some_and(|pl| pl.heading_path == heading_path);
                    if !can_append {
                        flush_docx_list(&mut pending_list, output)?;
                        let mut lead_in = None;
                        let mut start_para = paragraph_idx;
                        if last_was_plain_colon {
                            let expected_h = if heading_path.is_empty() {
                                format!("Paragraph {}", paragraph_idx.saturating_sub(1))
                            } else {
                                heading_path.clone()
                            };
                            if output
                                .chunks
                                .last()
                                .is_some_and(|c| c.heading == expected_h)
                            {
                                if let Some(popped) = output.chunks.pop() {
                                    output.bytes = output
                                        .bytes
                                        .saturating_sub(popped.heading.len() + popped.body.len());
                                    start_para = popped.anchor.paragraph.unwrap_or(paragraph_idx);
                                    lead_in = Some(popped.body.trim().to_owned());
                                }
                            }
                        }
                        pending_list = Some(PendingList {
                            start_para,
                            heading_path,
                            lead_in,
                            items: vec![bullet],
                        });
                    } else if let Some(pl) = &mut pending_list {
                        pl.items.push(bullet);
                    }
                    last_was_plain_colon = false;
                } else {
                    flush_docx_list(&mut pending_list, output)?;
                    pending_heading_only = None;
                    let heading_path = format_breadcrumbs(&breadcrumbs);
                    let heading = if heading_path.is_empty() {
                        format!("Paragraph {paragraph_idx}")
                    } else {
                        heading_path
                    };
                    let trimmed = text.trim();
                    last_was_plain_colon = trimmed.ends_with(':') || trimmed.ends_with('：');
                    emit_docx_prose(text, heading, paragraph_idx, output)?;
                }
            }
            DocxItem::Table {
                rows,
                paragraph_count,
            } => {
                flush_docx_list(&mut pending_list, output)?;
                last_was_plain_colon = false;
                let start_para = paragraph_idx + 1;
                paragraph_idx += paragraph_count.max(1);
                let heading_path = format_breadcrumbs(&breadcrumbs);
                let heading = if heading_path.is_empty() {
                    format!("Paragraph {start_para}")
                } else {
                    heading_path
                };
                let anchor = AnchorSpec {
                    paragraph: Some(start_para),
                    ..AnchorSpec::default()
                };
                let tbl_chunks = render_pipe_table_chunks(&rows, &heading, &anchor);
                if !tbl_chunks.is_empty() {
                    pending_heading_only = None;
                    for ch in tbl_chunks {
                        output.push_chunk(ch)?;
                    }
                }
            }
        }
    }

    flush_docx_list(&mut pending_list, output)?;
    if output.chunks.is_empty() {
        if let Some((para, heading, body)) = pending_heading_only {
            output.push_chunk(ChunkInput {
                heading,
                body,
                anchor: AnchorSpec {
                    paragraph: Some(para),
                    ..AnchorSpec::default()
                },
            })?;
        }
    }
    Ok(())
}

fn read_docx_items(xml: &[u8], cap: usize, max_elements: usize) -> Result<Vec<DocxItem>> {
    struct ParaCapture {
        depth: usize,
        ppr_depth: Option<usize>,
        style_id: Option<String>,
        outline_lvl: Option<u8>,
        is_list: bool,
        ilvl: u8,
        text: String,
    }
    struct TableCapture {
        depth: usize,
        nested_tbl_depth: usize,
        in_row: bool,
        cell_depth: Option<usize>,
        cell_has_para: bool,
        current_cell: String,
        current_row: Vec<String>,
        rows: Vec<Vec<String>>,
        paragraph_count: u32,
    }

    let mut reader = Reader::from_reader(xml);
    let mut stack: Vec<Vec<u8>> = Vec::new();
    let mut nodes = 0usize;
    let mut elements = 0usize;
    let mut text_bytes = 0usize;
    let mut items = Vec::new();
    let mut para: Option<ParaCapture> = None;
    let mut table: Option<TableCapture> = None;

    loop {
        nodes += 1;
        if nodes > MAX_XML_NODES {
            return Err(IngestError::Invalid("too many XML nodes"));
        }
        match reader.read_event()? {
            event @ (Event::Start(_) | Event::Empty(_)) => {
                let empty = matches!(event, Event::Empty(_));
                let e = match &event {
                    Event::Start(e) | Event::Empty(e) => e,
                    _ => return Err(IngestError::Invalid("invalid XML element")),
                };
                let local = e.local_name();
                if matches!(local.as_ref(), b"binData" | b"binaryData") {
                    return Err(IngestError::Invalid(
                        "inline Office binary data is unsupported",
                    ));
                }

                if para.is_none() && table.is_none() {
                    if local.as_ref() == b"p" {
                        elements += 1;
                        if elements > max_elements {
                            return Err(IngestError::Invalid("too many Office XML elements"));
                        }
                        if empty {
                            items.push(DocxItem::Paragraph {
                                style_id: None,
                                outline_lvl: None,
                                is_list: false,
                                ilvl: 0,
                                text: String::new(),
                            });
                        } else {
                            para = Some(ParaCapture {
                                depth: stack.len(),
                                ppr_depth: None,
                                style_id: None,
                                outline_lvl: None,
                                is_list: false,
                                ilvl: 0,
                                text: String::new(),
                            });
                        }
                    } else if local.as_ref() == b"tbl" && !empty {
                        elements += 1;
                        if elements > max_elements {
                            return Err(IngestError::Invalid("too many Office XML elements"));
                        }
                        table = Some(TableCapture {
                            depth: stack.len(),
                            nested_tbl_depth: 0,
                            in_row: false,
                            cell_depth: None,
                            cell_has_para: false,
                            current_cell: String::new(),
                            current_row: Vec::new(),
                            rows: Vec::new(),
                            paragraph_count: 0,
                        });
                    }
                } else if let Some(p) = &mut para {
                    let is_direct_child_of_outer_p = stack.len() == p.depth + 1;
                    if local.as_ref() == b"pPr" && is_direct_child_of_outer_p && !empty {
                        p.ppr_depth = Some(stack.len());
                    } else if p.ppr_depth.is_some() {
                        match local.as_ref() {
                            b"pStyle" => {
                                let attrs = decode_attrs(e, &reader)?;
                                if let Some(val) = local_attr(&attrs, "val") {
                                    p.style_id = Some(val.to_owned());
                                }
                            }
                            b"outlineLvl" => {
                                let attrs = decode_attrs(e, &reader)?;
                                if let Some(val) = local_attr(&attrs, "val") {
                                    if let Ok(n) = val.trim().parse::<u8>() {
                                        if n <= 5 {
                                            p.outline_lvl = Some(n + 1);
                                        }
                                    }
                                }
                            }
                            b"numPr" => {
                                p.is_list = true;
                            }
                            b"ilvl" => {
                                let attrs = decode_attrs(e, &reader)?;
                                if let Some(val) = local_attr(&attrs, "val") {
                                    if let Ok(n) = val.trim().parse::<u8>() {
                                        p.ilvl = n;
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    match local.as_ref() {
                        b"tab" => append_bounded(&mut p.text, "\t", &mut text_bytes, cap)?,
                        b"br" | b"cr" => append_bounded(&mut p.text, "\n", &mut text_bytes, cap)?,
                        _ => {}
                    }
                } else if let Some(t) = &mut table {
                    if local.as_ref() == b"tbl" && !empty {
                        t.nested_tbl_depth += 1;
                    } else if t.nested_tbl_depth == 0 {
                        match local.as_ref() {
                            b"tr" if !empty => {
                                elements += 1;
                                if elements > max_elements {
                                    return Err(IngestError::Invalid(
                                        "too many Office XML elements",
                                    ));
                                }
                                t.in_row = true;
                                t.current_row.clear();
                            }
                            b"tc" if t.in_row && t.cell_depth.is_none() => {
                                if empty {
                                    t.current_row.push(String::new());
                                } else {
                                    t.cell_depth = Some(stack.len());
                                    t.cell_has_para = false;
                                    t.current_cell.clear();
                                }
                            }
                            b"p" if t.cell_depth.is_some() => {
                                t.paragraph_count += 1;
                                if t.cell_has_para && !t.current_cell.ends_with(' ') {
                                    append_bounded(&mut t.current_cell, " ", &mut text_bytes, cap)?;
                                }
                                t.cell_has_para = true;
                            }
                            b"tab" if t.cell_depth.is_some() => {
                                append_bounded(&mut t.current_cell, " ", &mut text_bytes, cap)?;
                            }
                            b"br" | b"cr" if t.cell_depth.is_some() => {
                                append_bounded(&mut t.current_cell, " ", &mut text_bytes, cap)?;
                            }
                            _ => {}
                        }
                    } else if local.as_ref() == b"p" && t.cell_depth.is_some() {
                        if t.cell_has_para && !t.current_cell.ends_with(' ') {
                            append_bounded(&mut t.current_cell, " ", &mut text_bytes, cap)?;
                        }
                        t.cell_has_para = true;
                    }
                }

                if !empty {
                    stack.push(local.as_ref().to_vec());
                    if stack.len() > MAX_XML_DEPTH {
                        return Err(IngestError::Invalid("Office XML nesting limit exceeded"));
                    }
                }
            }
            Event::End(e) => {
                stack.pop().ok_or(IngestError::Invalid("unbalanced XML"))?;
                let local = e.local_name();
                if let Some(p) = &mut para {
                    if p.ppr_depth.is_some_and(|d| d == stack.len()) {
                        p.ppr_depth = None;
                    }
                    if p.depth == stack.len() {
                        if let Some(done) = para.take() {
                            items.push(DocxItem::Paragraph {
                                style_id: done.style_id,
                                outline_lvl: done.outline_lvl,
                                is_list: done.is_list,
                                ilvl: done.ilvl,
                                text: done.text,
                            });
                        }
                    }
                } else if let Some(t) = &mut table {
                    if t.depth == stack.len() {
                        if let Some(done) = table.take() {
                            items.push(DocxItem::Table {
                                rows: done.rows,
                                paragraph_count: done.paragraph_count,
                            });
                        }
                    } else if local.as_ref() == b"tbl" && t.nested_tbl_depth > 0 {
                        t.nested_tbl_depth -= 1;
                    } else if t.nested_tbl_depth == 0 {
                        if t.cell_depth.is_some_and(|d| d == stack.len()) {
                            t.cell_depth = None;
                            t.current_row.push(std::mem::take(&mut t.current_cell));
                        } else if local.as_ref() == b"tr" && t.in_row {
                            t.in_row = false;
                            if !t.current_row.is_empty() {
                                t.rows.push(std::mem::take(&mut t.current_row));
                            }
                        }
                    }
                }
            }
            Event::Text(e) => {
                if stack
                    .last()
                    .is_some_and(|n| matches!(n.as_slice(), b"t" | b"v"))
                {
                    let decoded = e
                        .decode()
                        .map_err(|_| IngestError::Invalid("invalid XML text"))?;
                    if let Some(p) = &mut para {
                        append_bounded(&mut p.text, &decoded, &mut text_bytes, cap)?;
                    } else if let Some(t) = &mut table {
                        if t.cell_depth.is_some() {
                            append_bounded(&mut t.current_cell, &decoded, &mut text_bytes, cap)?;
                        }
                    }
                }
            }
            Event::GeneralRef(e) => {
                if stack
                    .last()
                    .is_some_and(|n| matches!(n.as_slice(), b"t" | b"v"))
                {
                    let unescaped = decode_general_ref(&e)?;
                    if let Some(p) = &mut para {
                        append_bounded(&mut p.text, &unescaped, &mut text_bytes, cap)?;
                    } else if let Some(t) = &mut table {
                        if t.cell_depth.is_some() {
                            append_bounded(&mut t.current_cell, &unescaped, &mut text_bytes, cap)?;
                        }
                    }
                }
            }
            Event::CData(e) => {
                if stack
                    .last()
                    .is_some_and(|n| matches!(n.as_slice(), b"t" | b"v"))
                {
                    let raw = std::str::from_utf8(&e)
                        .map_err(|_| IngestError::Invalid("invalid XML CDATA"))?;
                    if let Some(p) = &mut para {
                        append_bounded(&mut p.text, raw, &mut text_bytes, cap)?;
                    } else if let Some(t) = &mut table {
                        if t.cell_depth.is_some() {
                            append_bounded(&mut t.current_cell, raw, &mut text_bytes, cap)?;
                        }
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
    Ok(items)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SlideShapeRole {
    Body,
    Title,
    Ignore,
}

struct ParsedSlideContent {
    title: Option<String>,
    body_lines: Vec<String>,
    tables: Vec<Vec<Vec<String>>>,
}

fn parse_slide_or_notes_xml(
    xml: &[u8],
    cap: usize,
    total_elements: &mut usize,
    max_elements: usize,
    is_notes: bool,
) -> Result<ParsedSlideContent> {
    struct ShapeState {
        depth: usize,
        role: SlideShapeRole,
        para_depth: Option<usize>,
        current_para: String,
        paras: Vec<String>,
    }
    struct SlideTableState {
        depth: usize,
        in_row: bool,
        cell_depth: Option<usize>,
        cell_has_para: bool,
        current_cell: String,
        current_row: Vec<String>,
        rows: Vec<Vec<String>>,
    }

    let mut reader = Reader::from_reader(xml);
    let mut stack: Vec<Vec<u8>> = Vec::new();
    let mut nodes = 0usize;
    let mut text_bytes = 0usize;

    let mut title: Option<String> = None;
    let mut body_lines: Vec<String> = Vec::new();
    let mut tables: Vec<Vec<Vec<String>>> = Vec::new();

    let mut shape: Option<ShapeState> = None;
    let mut table: Option<SlideTableState> = None;
    let mut loose_para: Option<(usize, String)> = None;

    loop {
        nodes += 1;
        if nodes > MAX_XML_NODES {
            return Err(IngestError::Invalid("too many XML nodes"));
        }
        match reader.read_event()? {
            event @ (Event::Start(_) | Event::Empty(_)) => {
                let empty = matches!(event, Event::Empty(_));
                let e = match &event {
                    Event::Start(e) | Event::Empty(e) => e,
                    _ => return Err(IngestError::Invalid("invalid XML element")),
                };
                let local = e.local_name();
                if matches!(local.as_ref(), b"binData" | b"binaryData") {
                    return Err(IngestError::Invalid(
                        "inline Office binary data is unsupported",
                    ));
                }

                if shape.is_none() && table.is_none() && loose_para.is_none() {
                    match local.as_ref() {
                        b"sp" if !empty => {
                            shape = Some(ShapeState {
                                depth: stack.len(),
                                role: SlideShapeRole::Body,
                                para_depth: None,
                                current_para: String::new(),
                                paras: Vec::new(),
                            });
                        }
                        b"tbl" if !empty => {
                            *total_elements += 1;
                            if *total_elements > max_elements {
                                return Err(IngestError::Invalid("too many Office XML elements"));
                            }
                            table = Some(SlideTableState {
                                depth: stack.len(),
                                in_row: false,
                                cell_depth: None,
                                cell_has_para: false,
                                current_cell: String::new(),
                                current_row: Vec::new(),
                                rows: Vec::new(),
                            });
                        }
                        b"p" => {
                            *total_elements += 1;
                            if *total_elements > max_elements {
                                return Err(IngestError::Invalid("too many Office XML elements"));
                            }
                            if !empty {
                                loose_para = Some((stack.len(), String::new()));
                            }
                        }
                        _ => {}
                    }
                } else if let Some(sp) = &mut shape {
                    match local.as_ref() {
                        b"ph" => {
                            let attrs = decode_attrs(e, &reader)?;
                            match local_attr(&attrs, "type") {
                                Some("title" | "ctrTitle") if !is_notes => {
                                    sp.role = SlideShapeRole::Title;
                                }
                                Some("sldNum" | "dt" | "ftr" | "hdr" | "sldImg") => {
                                    sp.role = SlideShapeRole::Ignore;
                                }
                                _ => {}
                            }
                        }
                        b"p" if sp.para_depth.is_none() => {
                            *total_elements += 1;
                            if *total_elements > max_elements {
                                return Err(IngestError::Invalid("too many Office XML elements"));
                            }
                            if !empty {
                                sp.para_depth = Some(stack.len());
                                sp.current_para.clear();
                            }
                        }
                        b"tab" if sp.para_depth.is_some() => {
                            append_bounded(&mut sp.current_para, "\t", &mut text_bytes, cap)?;
                        }
                        b"br" if sp.para_depth.is_some() => {
                            append_bounded(&mut sp.current_para, "\n", &mut text_bytes, cap)?;
                        }
                        _ => {}
                    }
                } else if let Some(t) = &mut table {
                    match local.as_ref() {
                        b"tr" if !empty => {
                            *total_elements += 1;
                            if *total_elements > max_elements {
                                return Err(IngestError::Invalid("too many Office XML elements"));
                            }
                            t.in_row = true;
                            t.current_row.clear();
                        }
                        b"tc" if t.in_row && t.cell_depth.is_none() => {
                            if empty {
                                t.current_row.push(String::new());
                            } else {
                                t.cell_depth = Some(stack.len());
                                t.cell_has_para = false;
                                t.current_cell.clear();
                            }
                        }
                        b"p" if t.cell_depth.is_some() => {
                            if t.cell_has_para && !t.current_cell.ends_with(' ') {
                                append_bounded(&mut t.current_cell, " ", &mut text_bytes, cap)?;
                            }
                            t.cell_has_para = true;
                        }
                        b"tab" | b"br" if t.cell_depth.is_some() => {
                            append_bounded(&mut t.current_cell, " ", &mut text_bytes, cap)?;
                        }
                        _ => {}
                    }
                } else if let Some((_, lp)) = &mut loose_para {
                    match local.as_ref() {
                        b"tab" => append_bounded(lp, "\t", &mut text_bytes, cap)?,
                        b"br" => append_bounded(lp, "\n", &mut text_bytes, cap)?,
                        _ => {}
                    }
                }

                if !empty {
                    stack.push(local.as_ref().to_vec());
                    if stack.len() > MAX_XML_DEPTH {
                        return Err(IngestError::Invalid("Office XML nesting limit exceeded"));
                    }
                }
            }
            Event::End(e) => {
                stack.pop().ok_or(IngestError::Invalid("unbalanced XML"))?;
                let local = e.local_name();
                if let Some(sp) = &mut shape {
                    if sp.para_depth.is_some_and(|d| d == stack.len()) {
                        sp.para_depth = None;
                        let p = std::mem::take(&mut sp.current_para);
                        if !p.trim().is_empty() {
                            sp.paras.push(p);
                        }
                    } else if sp.depth == stack.len() {
                        if let Some(done) = shape.take() {
                            match done.role {
                                SlideShapeRole::Title => {
                                    let joined = done
                                        .paras
                                        .iter()
                                        .map(|s| s.trim())
                                        .filter(|s| !s.is_empty())
                                        .collect::<Vec<_>>()
                                        .join(" ");
                                    if !joined.is_empty() && title.is_none() {
                                        title = Some(joined);
                                    }
                                }
                                SlideShapeRole::Body => {
                                    body_lines.extend(done.paras);
                                }
                                SlideShapeRole::Ignore => {}
                            }
                        }
                    }
                } else if let Some(t) = &mut table {
                    if t.depth == stack.len() {
                        if let Some(done) = table.take() {
                            if !done.rows.is_empty() {
                                tables.push(done.rows);
                            }
                        }
                    } else if t.cell_depth.is_some_and(|d| d == stack.len()) {
                        t.cell_depth = None;
                        t.current_row.push(std::mem::take(&mut t.current_cell));
                    } else if local.as_ref() == b"tr" && t.in_row {
                        t.in_row = false;
                        if !t.current_row.is_empty() {
                            t.rows.push(std::mem::take(&mut t.current_row));
                        }
                    }
                } else if loose_para.as_ref().is_some_and(|(d, _)| *d == stack.len()) {
                    if let Some((_, p)) = loose_para.take() {
                        if !p.trim().is_empty() {
                            body_lines.push(p);
                        }
                    }
                }
            }
            Event::Text(e) => {
                if stack
                    .last()
                    .is_some_and(|n| matches!(n.as_slice(), b"t" | b"v"))
                {
                    let decoded = e
                        .decode()
                        .map_err(|_| IngestError::Invalid("invalid XML text"))?;
                    if let Some(sp) = &mut shape {
                        if sp.para_depth.is_some() {
                            append_bounded(&mut sp.current_para, &decoded, &mut text_bytes, cap)?;
                        }
                    } else if let Some(t) = &mut table {
                        if t.cell_depth.is_some() {
                            append_bounded(&mut t.current_cell, &decoded, &mut text_bytes, cap)?;
                        }
                    } else if let Some((_, lp)) = &mut loose_para {
                        append_bounded(lp, &decoded, &mut text_bytes, cap)?;
                    }
                }
            }
            Event::GeneralRef(e) => {
                if stack
                    .last()
                    .is_some_and(|n| matches!(n.as_slice(), b"t" | b"v"))
                {
                    let unescaped = decode_general_ref(&e)?;
                    if let Some(sp) = &mut shape {
                        if sp.para_depth.is_some() {
                            append_bounded(&mut sp.current_para, &unescaped, &mut text_bytes, cap)?;
                        }
                    } else if let Some(t) = &mut table {
                        if t.cell_depth.is_some() {
                            append_bounded(&mut t.current_cell, &unescaped, &mut text_bytes, cap)?;
                        }
                    } else if let Some((_, lp)) = &mut loose_para {
                        append_bounded(lp, &unescaped, &mut text_bytes, cap)?;
                    }
                }
            }
            Event::CData(e) => {
                if stack
                    .last()
                    .is_some_and(|n| matches!(n.as_slice(), b"t" | b"v"))
                {
                    let raw = std::str::from_utf8(&e)
                        .map_err(|_| IngestError::Invalid("invalid XML CDATA"))?;
                    if let Some(sp) = &mut shape {
                        if sp.para_depth.is_some() {
                            append_bounded(&mut sp.current_para, raw, &mut text_bytes, cap)?;
                        }
                    } else if let Some(t) = &mut table {
                        if t.cell_depth.is_some() {
                            append_bounded(&mut t.current_cell, raw, &mut text_bytes, cap)?;
                        }
                    } else if let Some((_, lp)) = &mut loose_para {
                        append_bounded(lp, raw, &mut text_bytes, cap)?;
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

    Ok(ParsedSlideContent {
        title,
        body_lines,
        tables,
    })
}

fn emit_slide_lines(
    lines: &[String],
    heading: &str,
    slide: u32,
    output: &mut Output,
) -> Result<()> {
    if lines.is_empty() {
        return Ok(());
    }
    let anchor = AnchorSpec {
        slide: Some(slide),
        ..AnchorSpec::default()
    };
    let mut cur = String::new();
    for line in lines {
        if estimate_batch_tokens(line) > HARD_MAX_CHUNK_TOKENS {
            if !cur.is_empty() {
                output.push_chunk(ChunkInput {
                    heading: heading.to_owned(),
                    body: std::mem::take(&mut cur),
                    anchor: anchor.clone(),
                })?;
            }
            for r in split_prose_ranges(
                line,
                0..line.len(),
                TARGET_CHUNK_TOKENS,
                HARD_MAX_CHUNK_TOKENS,
            ) {
                output.push_chunk(ChunkInput {
                    heading: heading.to_owned(),
                    body: line[r].to_owned(),
                    anchor: anchor.clone(),
                })?;
            }
            continue;
        }
        let next_tokens = estimate_batch_tokens(&cur) + estimate_batch_tokens(line) + 1;
        if !cur.is_empty() && next_tokens > TARGET_CHUNK_TOKENS {
            output.push_chunk(ChunkInput {
                heading: heading.to_owned(),
                body: std::mem::take(&mut cur),
                anchor: anchor.clone(),
            })?;
        }
        if !cur.is_empty() {
            cur.push('\n');
        }
        cur.push_str(line);
    }
    if !cur.is_empty() {
        output.push_chunk(ChunkInput {
            heading: heading.to_owned(),
            body: cur,
            anchor,
        })?;
    }
    Ok(())
}

fn find_notes_path_for_slide(
    parts: &BTreeMap<String, Vec<u8>>,
    slide_path: &str,
    cap: usize,
    max_elements: usize,
) -> Result<Option<String>> {
    if let Some(stem) = slide_path
        .strip_prefix("ppt/slides/")
        .and_then(|s| s.strip_suffix(".xml"))
    {
        let rels_path = format!("ppt/slides/_rels/{stem}.xml.rels");
        if let Some(rels_xml) = parts.get(&rels_path) {
            let rels = relationships(rels_xml, "ppt/slides", cap, max_elements)?;
            for target in rels.into_values() {
                if target.starts_with("ppt/notesSlides/") && parts.contains_key(&target) {
                    return Ok(Some(target));
                }
            }
        }
        if let Some(num) = stem.strip_prefix("slide") {
            let fallback = format!("ppt/notesSlides/notesSlide{num}.xml");
            if parts.contains_key(&fallback) {
                return Ok(Some(fallback));
            }
        }
    }
    Ok(None)
}

fn slides(parts: &BTreeMap<String, Vec<u8>>, output: &mut Output) -> Result<()> {
    let cap = output.cap;
    let max_chunks = output.max_chunks;
    let mut seen = BTreeSet::new();
    let mut slide_num = 0u32;
    let mut total_elements = 0usize;

    let mut emit = |name: &str| -> Result<()> {
        if !seen.insert(name.to_owned()) {
            return Ok(());
        }
        slide_num += 1;
        let parsed = parse_slide_or_notes_xml(
            part(parts, name)?,
            cap,
            &mut total_elements,
            max_chunks,
            false,
        )?;
        let heading = match parsed.title.as_deref() {
            Some(t) if !t.is_empty() => format!("Slide {slide_num} > {t}"),
            _ => format!("Slide {slide_num}"),
        };
        let anchor = AnchorSpec {
            slide: Some(slide_num),
            ..AnchorSpec::default()
        };

        emit_slide_lines(&parsed.body_lines, &heading, slide_num, output)?;
        for tbl in &parsed.tables {
            for ch in render_pipe_table_chunks(tbl, &heading, &anchor) {
                output.push_chunk(ch)?;
            }
        }
        if parsed.body_lines.is_empty() && parsed.tables.is_empty() {
            if let Some(title_text) = parsed.title.filter(|t| !t.is_empty()) {
                output.push_chunk(ChunkInput {
                    heading: heading.clone(),
                    body: title_text,
                    anchor,
                })?;
            }
        }

        if let Some(notes_path) = find_notes_path_for_slide(parts, name, cap, max_chunks)? {
            if let Some(notes_xml) = parts.get(&notes_path) {
                let notes = parse_slide_or_notes_xml(
                    notes_xml,
                    cap,
                    &mut total_elements,
                    max_chunks,
                    true,
                )?;
                let notes_heading = format!("{heading} > Notes");
                emit_slide_lines(&notes.body_lines, &notes_heading, slide_num, output)?;
                let notes_anchor = AnchorSpec {
                    slide: Some(slide_num),
                    ..AnchorSpec::default()
                };
                for tbl in &notes.tables {
                    for ch in render_pipe_table_chunks(tbl, &notes_heading, &notes_anchor) {
                        output.push_chunk(ch)?;
                    }
                }
            }
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
                .or_else(|| s.attrs.get("id"))
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

fn parse_cell_coord(coord: &str) -> Option<(usize, u32)> {
    let split = coord.find(|c: char| c.is_ascii_digit())?;
    let (col_str, row_str) = coord.split_at(split);
    if col_str.is_empty() || !col_str.bytes().all(|b| b.is_ascii_alphabetic()) {
        return None;
    }
    let row_num = row_str.parse::<u32>().ok().filter(|&r| r >= 1)?;
    let mut col_idx = 0usize;
    for b in col_str.bytes() {
        let digit = usize::from(b.to_ascii_uppercase() - b'A' + 1);
        col_idx = col_idx.checked_mul(26)?.checked_add(digit)?;
    }
    Some((col_idx - 1, row_num))
}

fn col_index_to_letters(mut idx: usize) -> String {
    let mut chars = Vec::new();
    loop {
        let rem = (idx % 26) as u8;
        chars.push(char::from(b'A' + rem));
        if idx < 26 {
            break;
        }
        idx = idx / 26 - 1;
    }
    chars.into_iter().rev().collect()
}

fn parse_shared_strings(xml: &[u8], cap: usize) -> Result<Vec<String>> {
    let mut reader = Reader::from_reader(xml);
    let mut stack: Vec<Vec<u8>> = Vec::new();
    let mut nodes = 0usize;
    let mut text_bytes = 0usize;
    let mut strings = Vec::new();
    let mut current_si: Option<(usize, String)> = None;
    let mut rph_depth: Option<usize> = None;

    loop {
        nodes += 1;
        if nodes > MAX_XML_NODES {
            return Err(IngestError::Invalid("too many XML nodes"));
        }
        match reader.read_event()? {
            event @ (Event::Start(_) | Event::Empty(_)) => {
                let empty = matches!(event, Event::Empty(_));
                let e = match &event {
                    Event::Start(e) | Event::Empty(e) => e,
                    _ => return Err(IngestError::Invalid("invalid XML element")),
                };
                let local = e.local_name();
                if matches!(local.as_ref(), b"binData" | b"binaryData") {
                    return Err(IngestError::Invalid(
                        "inline Office binary data is unsupported",
                    ));
                }
                if local.as_ref() == b"si" && current_si.is_none() {
                    if strings.len() >= 250_000 {
                        return Err(IngestError::Invalid("too many Office XML elements"));
                    }
                    if empty {
                        strings.push(String::new());
                    } else {
                        current_si = Some((stack.len(), String::new()));
                    }
                } else if current_si.is_some()
                    && local.as_ref() == b"rPh"
                    && !empty
                    && rph_depth.is_none()
                {
                    rph_depth = Some(stack.len());
                }
                if !empty {
                    stack.push(local.as_ref().to_vec());
                    if stack.len() > MAX_XML_DEPTH {
                        return Err(IngestError::Invalid("Office XML nesting limit exceeded"));
                    }
                }
            }
            Event::End(_) => {
                stack.pop().ok_or(IngestError::Invalid("unbalanced XML"))?;
                if rph_depth.is_some_and(|d| d == stack.len()) {
                    rph_depth = None;
                }
                if current_si.as_ref().is_some_and(|(d, _)| *d == stack.len()) {
                    if let Some((_, s)) = current_si.take() {
                        strings.push(s);
                    }
                }
            }
            Event::Text(e) => {
                if let Some((_, buf)) = &mut current_si {
                    if rph_depth.is_none() && stack.last().is_some_and(|n| n.as_slice() == b"t") {
                        let decoded = e
                            .decode()
                            .map_err(|_| IngestError::Invalid("invalid XML text"))?;
                        append_bounded(buf, &decoded, &mut text_bytes, cap)?;
                    }
                }
            }
            Event::GeneralRef(e) => {
                if let Some((_, buf)) = &mut current_si {
                    if rph_depth.is_none() && stack.last().is_some_and(|n| n.as_slice() == b"t") {
                        let unescaped = decode_general_ref(&e)?;
                        append_bounded(buf, &unescaped, &mut text_bytes, cap)?;
                    }
                }
            }
            Event::CData(e) => {
                if let Some((_, buf)) = &mut current_si {
                    if rph_depth.is_none() && stack.last().is_some_and(|n| n.as_slice() == b"t") {
                        let raw = std::str::from_utf8(&e)
                            .map_err(|_| IngestError::Invalid("invalid XML CDATA"))?;
                        append_bounded(buf, raw, &mut text_bytes, cap)?;
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
    Ok(strings)
}

fn parse_worksheet_into_chunks(
    xml: &[u8],
    sheet_name: &str,
    shared: &[String],
    output: &mut Output,
) -> Result<()> {
    struct CellCapture {
        depth: usize,
        coord: Option<String>,
        cell_type: Option<String>,
        v_text: String,
        inline_text: String,
    }

    let mut reader = Reader::from_reader(xml);
    let mut stack: Vec<Vec<u8>> = Vec::new();
    let mut nodes = 0usize;
    let mut xml_text_bytes = 0usize;
    let mut total_cells = 0usize;
    let mut cells_in_current_row = 0usize;
    let mut current_row_num: u32 = 0;
    let mut next_col_idx: usize = 0;
    let mut cell: Option<CellCapture> = None;
    let mut rows_map: BTreeMap<u32, BTreeMap<usize, String>> = BTreeMap::new();

    loop {
        nodes += 1;
        if nodes > MAX_XML_NODES {
            return Err(IngestError::Invalid("too many XML nodes"));
        }
        match reader.read_event()? {
            event @ (Event::Start(_) | Event::Empty(_)) => {
                let empty = matches!(event, Event::Empty(_));
                let e = match &event {
                    Event::Start(e) | Event::Empty(e) => e,
                    _ => return Err(IngestError::Invalid("invalid XML element")),
                };
                let local = e.local_name();
                if matches!(local.as_ref(), b"binData" | b"binaryData") {
                    return Err(IngestError::Invalid(
                        "inline Office binary data is unsupported",
                    ));
                }

                if local.as_ref() == b"row" && cell.is_none() {
                    let attrs = decode_attrs(e, &reader)?;
                    current_row_num = local_attr(&attrs, "r")
                        .and_then(|r| r.parse::<u32>().ok())
                        .unwrap_or_else(|| current_row_num.saturating_add(1));
                    next_col_idx = 0;
                    cells_in_current_row = 0;
                } else if local.as_ref() == b"c" && cell.is_none() {
                    total_cells += 1;
                    cells_in_current_row += 1;
                    if total_cells > 500_000 || cells_in_current_row > 4_096 {
                        return Err(IngestError::Invalid("too many Office XML elements"));
                    }
                    if !empty {
                        let attrs = decode_attrs(e, &reader)?;
                        cell = Some(CellCapture {
                            depth: stack.len(),
                            coord: local_attr(&attrs, "r").map(str::to_owned),
                            cell_type: local_attr(&attrs, "t").map(str::to_owned),
                            v_text: String::new(),
                            inline_text: String::new(),
                        });
                    }
                }

                if !empty {
                    stack.push(local.as_ref().to_vec());
                    if stack.len() > MAX_XML_DEPTH {
                        return Err(IngestError::Invalid("Office XML nesting limit exceeded"));
                    }
                }
            }
            Event::End(_) => {
                stack.pop().ok_or(IngestError::Invalid("unbalanced XML"))?;
                if cell.as_ref().is_some_and(|c| c.depth == stack.len()) {
                    if let Some(done) = cell.take() {
                        let resolved: String = match done.cell_type.as_deref() {
                            Some("s") => {
                                let trimmed = done.v_text.trim();
                                if trimmed.is_empty() {
                                    String::new()
                                } else {
                                    let idx: usize = trimmed.parse().map_err(|_| {
                                        IngestError::Invalid("invalid shared string index")
                                    })?;
                                    shared
                                        .get(idx)
                                        .ok_or(IngestError::Invalid("missing shared string"))?
                                        .clone()
                                }
                            }
                            Some("inlineStr") => {
                                if !done.inline_text.is_empty() {
                                    done.inline_text
                                } else {
                                    done.v_text
                                }
                            }
                            Some("b") => {
                                let trimmed = done.v_text.trim();
                                if trimmed.is_empty() {
                                    String::new()
                                } else if trimmed == "1" || trimmed.eq_ignore_ascii_case("true") {
                                    "true".to_owned()
                                } else {
                                    "false".to_owned()
                                }
                            }
                            _ => {
                                if !done.v_text.is_empty() {
                                    done.v_text
                                } else {
                                    done.inline_text
                                }
                            }
                        };

                        if !resolved.trim().is_empty() {
                            output.reserve(resolved.len())?;
                            let (col_idx, row_num) = match done.coord.as_deref() {
                                Some(r) => parse_cell_coord(r)
                                    .ok_or(IngestError::Invalid("invalid cell coordinate"))?,
                                None => (next_col_idx, current_row_num.max(1)),
                            };
                            next_col_idx = col_idx + 1;
                            rows_map
                                .entry(row_num)
                                .or_default()
                                .insert(col_idx, resolved);
                        } else {
                            next_col_idx += 1;
                        }
                    }
                }
            }
            Event::Text(e) => {
                if let Some(c) = &mut cell {
                    let decoded = e
                        .decode()
                        .map_err(|_| IngestError::Invalid("invalid XML text"))?;
                    match stack.last().map(Vec::as_slice) {
                        Some(b"v") => append_bounded(
                            &mut c.v_text,
                            &decoded,
                            &mut xml_text_bytes,
                            output.cap,
                        )?,
                        Some(b"t") => append_bounded(
                            &mut c.inline_text,
                            &decoded,
                            &mut xml_text_bytes,
                            output.cap,
                        )?,
                        _ => {}
                    }
                }
            }
            Event::GeneralRef(e) => {
                if let Some(c) = &mut cell {
                    let unescaped = decode_general_ref(&e)?;
                    match stack.last().map(Vec::as_slice) {
                        Some(b"v") => append_bounded(
                            &mut c.v_text,
                            &unescaped,
                            &mut xml_text_bytes,
                            output.cap,
                        )?,
                        Some(b"t") => append_bounded(
                            &mut c.inline_text,
                            &unescaped,
                            &mut xml_text_bytes,
                            output.cap,
                        )?,
                        _ => {}
                    }
                }
            }
            Event::CData(e) => {
                if let Some(c) = &mut cell {
                    let raw = std::str::from_utf8(&e)
                        .map_err(|_| IngestError::Invalid("invalid XML CDATA"))?;
                    match stack.last().map(Vec::as_slice) {
                        Some(b"v") => {
                            append_bounded(&mut c.v_text, raw, &mut xml_text_bytes, output.cap)?
                        }
                        Some(b"t") => append_bounded(
                            &mut c.inline_text,
                            raw,
                            &mut xml_text_bytes,
                            output.cap,
                        )?,
                        _ => {}
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

    if rows_map.is_empty() {
        return Ok(());
    }

    let min_col = rows_map
        .values()
        .filter_map(|r| r.keys().next().copied())
        .min()
        .unwrap_or(0);
    let max_col = rows_map
        .values()
        .filter_map(|r| r.keys().next_back().copied())
        .max()
        .unwrap_or(0);
    if max_col.saturating_sub(min_col) > 1_024 {
        return Err(IngestError::Invalid("too many Office XML elements"));
    }
    let num_cols = (max_col - min_col + 1).max(1);
    let max_col_letters = col_index_to_letters(max_col);

    let mut ordered_rows: Vec<(u32, Vec<String>)> = Vec::with_capacity(rows_map.len());
    for (r_num, cells) in rows_map {
        let mut vec_row = vec![String::new(); num_cols];
        for (c_idx, val) in cells {
            if c_idx >= min_col && c_idx <= max_col {
                vec_row[c_idx - min_col] = val;
            }
        }
        ordered_rows.push((r_num, vec_row));
    }

    let (header_r, header_cells) = &ordered_rows[0];
    let header_prefix = format!(
        "{}\n{}",
        format_pipe_row(header_cells, num_cols),
        format_pipe_separator(num_cols)
    );

    if ordered_rows.len() == 1 {
        let heading = format!("{sheet_name} (rows {header_r}..{header_r})");
        let cell_span = format!("A{header_r}:{max_col_letters}{header_r}");
        output.reserve(heading.len() + cell_span.len() + 16)?;
        if output.chunks.len() >= output.max_chunks {
            return Err(IngestError::Invalid("too many Office chunks"));
        }
        output.chunks.push(ChunkInput {
            heading,
            body: header_prefix,
            anchor: AnchorSpec {
                sheet: Some(sheet_name.to_owned()),
                cell: Some(cell_span),
                ..AnchorSpec::default()
            },
        });
        return Ok(());
    }

    let mut cur_body = header_prefix.clone();
    let mut batch_start_r = *header_r;
    let mut batch_end_r = *header_r;
    let mut rows_in_batch = 0usize;

    for (r_num, row_cells) in ordered_rows.iter().skip(1) {
        let line = format_pipe_row(row_cells, num_cols);
        let candidate_tokens = estimate_batch_tokens(&cur_body) + estimate_batch_tokens(&line) + 1;
        if rows_in_batch > 0 && candidate_tokens > TARGET_CHUNK_TOKENS {
            let heading = format!("{sheet_name} (rows {batch_start_r}..{batch_end_r})");
            let cell_span = format!("A{batch_start_r}:{max_col_letters}{batch_end_r}");
            output.reserve(heading.len() + header_prefix.len() + cell_span.len())?;
            if output.chunks.len() >= output.max_chunks {
                return Err(IngestError::Invalid("too many Office chunks"));
            }
            output.chunks.push(ChunkInput {
                heading,
                body: cur_body,
                anchor: AnchorSpec {
                    sheet: Some(sheet_name.to_owned()),
                    cell: Some(cell_span),
                    ..AnchorSpec::default()
                },
            });
            cur_body = header_prefix.clone();
            batch_start_r = *r_num;
            rows_in_batch = 0;
        }
        cur_body.push('\n');
        cur_body.push_str(&line);
        batch_end_r = *r_num;
        rows_in_batch += 1;
    }

    if rows_in_batch > 0 {
        let heading = format!("{sheet_name} (rows {batch_start_r}..{batch_end_r})");
        let cell_span = format!("A{batch_start_r}:{max_col_letters}{batch_end_r}");
        output.reserve(heading.len() + header_prefix.len() + cell_span.len())?;
        if output.chunks.len() >= output.max_chunks {
            return Err(IngestError::Invalid("too many Office chunks"));
        }
        output.chunks.push(ChunkInput {
            heading,
            body: cur_body,
            anchor: AnchorSpec {
                sheet: Some(sheet_name.to_owned()),
                cell: Some(cell_span),
                ..AnchorSpec::default()
            },
        });
    }
    Ok(())
}

fn sheets(parts: &BTreeMap<String, Vec<u8>>, output: &mut Output) -> Result<()> {
    let shared = parts
        .get("xl/sharedStrings.xml")
        .map(|xml| parse_shared_strings(xml, output.cap))
        .transpose()?
        .unwrap_or_default();
    let cap = output.cap;
    let max_chunks = output.max_chunks;
    let mut seen = BTreeSet::new();
    let mut emit = |path: &str, sheet: &str| {
        if !seen.insert(path.to_owned()) {
            return Ok(());
        }
        parse_worksheet_into_chunks(part(parts, path)?, sheet, &shared, output)
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
                .or_else(|| s.attrs.get("id"))
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
            let resolved = resolve_opc_target(prefix, target)?;
            result.insert(id.clone(), resolved);
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

#[derive(Default)]
struct Element {
    attrs: BTreeMap<String, String>,
}

fn visit_elements(
    xml: &[u8],
    selected: &[u8],
    _cap: usize,
    max_elements: usize,
    mut emit: impl FnMut(Element) -> Result<()>,
) -> Result<()> {
    let mut reader = Reader::from_reader(xml);
    let mut count = 0;
    let mut capture: Option<(usize, Element)> = None;
    let mut stack = Vec::new();
    let mut nodes = 0;
    loop {
        nodes += 1;
        if nodes > MAX_XML_NODES {
            return Err(IngestError::Invalid("too many XML nodes"));
        }
        match reader.read_event()? {
            event @ (Event::Start(_) | Event::Empty(_)) => {
                let empty = matches!(event, Event::Empty(_));
                let e = match &event {
                    Event::Start(e) | Event::Empty(e) => e,
                    _ => return Err(IngestError::Invalid("invalid XML element")),
                };
                let name = e.local_name();
                if matches!(name.as_ref(), b"binData" | b"binaryData") {
                    return Err(IngestError::Invalid(
                        "inline Office binary data is unsupported",
                    ));
                }
                if name.as_ref() == selected && capture.is_none() {
                    count += 1;
                    if count > max_elements {
                        return Err(IngestError::Invalid("too many Office XML elements"));
                    }
                    let element = Element {
                        attrs: decode_attrs(e, &reader)?,
                    };
                    if empty {
                        emit(element)?;
                    } else {
                        capture = Some((stack.len(), element));
                    }
                }
                if !empty {
                    stack.push(name.as_ref().to_vec());
                }
                if stack.len() > MAX_XML_DEPTH {
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
