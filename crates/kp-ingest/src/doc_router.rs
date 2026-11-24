// Native routing and explicit NeedsOcr follow SAG-Wiki/core/src/anydoc.rs (MIT).
use std::path::Path;

use kp_derive::{base_fact, Extractor, FunnelOptions, Memory};
use kp_model::{AnchorSpec, ContentHash, ValidityWindow};
use kp_store::{ChunkInput, RawSourceInput, Remembered, Store, StoredSource};
use serde::Serialize;

use crate::{code_intel, office, text, web_and_media, IngestError, Result, MAX_INPUT_BYTES};

pub use crate::web_and_media::add_url;

pub struct Document {
    pub media_type: &'static str,
    pub chunks: Vec<ChunkInput>,
    pub needs_ocr: bool,
}

#[derive(Serialize)]
pub struct AddedDocument {
    pub uri: String,
    pub source: StoredSource,
    pub extracted: Remembered,
    pub chunks: usize,
    pub needs_ocr: bool,
}

pub fn handles(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        matches!(
            e.to_ascii_lowercase().as_str(),
            "md" | "markdown"
                | "mdx"
                | "txt"
                | "csv"
                | "json"
                | "rs"
                | "py"
                | "ts"
                | "tsx"
                | "js"
                | "jsx"
                | "go"
                | "docx"
                | "pptx"
                | "xlsx"
                | "pdf"
                | "html"
                | "htm"
        )
    })
}

pub fn parse(path: &Path, bytes: &[u8]) -> Result<Document> {
    if bytes.len() > MAX_INPUT_BYTES {
        return Err(IngestError::Limit(MAX_INPUT_BYTES));
    }
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let mut document = match extension.as_str() {
        "docx" | "pptx" | "xlsx" => Document {
            media_type: match extension.as_str() {
                "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
                "pptx" => {
                    "application/vnd.openxmlformats-officedocument.presentationml.presentation"
                }
                _ => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            },
            chunks: office::parse(bytes, &extension, MAX_INPUT_BYTES)?,
            needs_ocr: false,
        },
        "pdf" => pdf(bytes)?,
        "md" | "mdx" | "markdown" => Document {
            media_type: "text/markdown",
            chunks: text::markdown_bytes(bytes)?,
            needs_ocr: false,
        },
        "html" | "htm" => Document {
            media_type: "text/html",
            chunks: web_and_media::parse_html(&text::decode(bytes)?)?,
            needs_ocr: false,
        },
        "rs" | "py" | "ts" | "tsx" | "js" | "jsx" | "go" => {
            let source = std::str::from_utf8(bytes)
                .map_err(|_| IngestError::Invalid("source code must be UTF-8"))?;
            let symbols = code_intel::symbols(&path.with_extension(&extension), source)?;
            let mut chunks: Vec<_> = symbols
                .into_iter()
                .map(|s| ChunkInput {
                    heading: s.symbol_name.clone(),
                    body: source[s.start_byte..s.end_byte].into(),
                    anchor: AnchorSpec {
                        symbol: Some(s.symbol_name),
                        start_line: Some(s.start_line),
                        end_line: Some(s.end_line),
                        start_byte: Some(s.start_byte as u64),
                        end_byte: Some(s.end_byte as u64),
                        sig_hash: Some(s.sig_hash),
                        body_hash: Some(s.body_hash),
                        ..AnchorSpec::default()
                    },
                })
                .collect();
            if chunks.is_empty() && !source.is_empty() {
                chunks = text::plain_text_str(source, Some(0))?;
            }
            Document {
                media_type: "text/x-source",
                chunks,
                needs_ocr: false,
            }
        }
        "csv" => Document {
            media_type: "text/csv",
            chunks: text::csv_bytes(bytes)?,
            needs_ocr: false,
        },
        "txt" | "json" => {
            let media_type = if extension == "json" {
                "application/json"
            } else {
                "text/plain"
            };
            Document {
                media_type,
                chunks: text::plain_text_bytes(bytes)?,
                needs_ocr: false,
            }
        }
        _ => return Err(IngestError::Invalid("unsupported document format")),
    };
    let mut size = 0usize;
    if document.chunks.len() > 100_000 {
        return Err(IngestError::Invalid("too many document chunks"));
    }
    for chunk in &mut document.chunks {
        size = size
            .checked_add(chunk.body.len())
            .ok_or(IngestError::Limit(MAX_INPUT_BYTES))?;
        if size > MAX_INPUT_BYTES {
            return Err(IngestError::Limit(MAX_INPUT_BYTES));
        }
        chunk.anchor.file_path = Some(path.to_string_lossy().into_owned());
    }
    Ok(document)
}

fn fold_pdf_ligatures(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '\u{FB00}' => out.push_str("ff"),
            '\u{FB01}' => out.push_str("fi"),
            '\u{FB02}' => out.push_str("fl"),
            '\u{FB03}' => out.push_str("ffi"),
            '\u{FB04}' => out.push_str("ffl"),
            '\u{FB05}' | '\u{FB06}' => out.push_str("st"),
            other => out.push(other),
        }
    }
    out
}

fn normalize_pdf_furniture_key(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.chars().count() > 160 {
        return None;
    }
    Some(
        trimmed
            .chars()
            .map(|c| if c.is_ascii_digit() { '#' } else { c })
            .collect(),
    )
}

fn strip_repeating_pdf_headers_footers(pages: &mut [(u32, String, bool)]) {
    if pages.len() < 3 {
        return;
    }
    let mut first_counts: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    let mut last_counts: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();

    for (_, text, _) in pages.iter() {
        let lines: Vec<&str> = text.lines().collect();
        if let Some(first) = lines.iter().find(|l| !l.trim().is_empty()) {
            if let Some(key) = normalize_pdf_furniture_key(first) {
                *first_counts.entry(key).or_insert(0) += 1;
            }
        }
        if let Some(last) = lines.iter().rev().find(|l| !l.trim().is_empty()) {
            if let Some(key) = normalize_pdf_furniture_key(last) {
                *last_counts.entry(key).or_insert(0) += 1;
            }
        }
    }

    for (_, text, _) in pages.iter_mut() {
        let lines: Vec<&str> = text.lines().collect();
        let first_idx = lines.iter().position(|l| !l.trim().is_empty());
        let last_idx = lines.iter().rposition(|l| !l.trim().is_empty());
        let drop_first = first_idx.is_some_and(|idx| {
            normalize_pdf_furniture_key(lines[idx])
                .is_some_and(|k| first_counts.get(&k).copied().unwrap_or(0) >= 3)
        });
        let drop_last = last_idx.is_some_and(|idx| {
            normalize_pdf_furniture_key(lines[idx])
                .is_some_and(|k| last_counts.get(&k).copied().unwrap_or(0) >= 3)
        });
        if !drop_first && !drop_last {
            continue;
        }
        let mut rebuilt = Vec::with_capacity(lines.len());
        for (idx, line) in lines.into_iter().enumerate() {
            if (drop_first && Some(idx) == first_idx) || (drop_last && Some(idx) == last_idx) {
                continue;
            }
            rebuilt.push(line);
        }
        *text = rebuilt.join("\n");
    }
}

fn dict_has_image_xobject(pdf: &lopdf::Document, resources: &lopdf::Dictionary, depth: u8) -> bool {
    if depth > 2 {
        return false;
    }
    let Ok(xobj_entry) = resources.get(b"XObject") else {
        return false;
    };
    let xobj_dict = match xobj_entry {
        lopdf::Object::Dictionary(d) => Some(d),
        lopdf::Object::Reference(id) => pdf.get_dictionary(*id).ok(),
        _ => None,
    };
    let Some(dict) = xobj_dict else {
        return false;
    };
    for (_, val) in dict.iter() {
        let stream = match val {
            lopdf::Object::Stream(s) => Some(s),
            lopdf::Object::Reference(id) => {
                pdf.get_object(*id).ok().and_then(|o| o.as_stream().ok())
            }
            _ => None,
        };
        let Some(s) = stream else {
            continue;
        };
        match s.dict.get(b"Subtype").and_then(lopdf::Object::as_name) {
            Ok(b"Image") => return true,
            Ok(b"Form") => {
                let nested_res = match s.dict.get(b"Resources") {
                    Ok(lopdf::Object::Dictionary(d)) => Some(d),
                    Ok(lopdf::Object::Reference(id)) => pdf.get_dictionary(*id).ok(),
                    _ => None,
                };
                if nested_res.is_some_and(|r| dict_has_image_xobject(pdf, r, depth + 1)) {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

fn page_has_image_xobject(pdf: &lopdf::Document, page_id: lopdf::ObjectId) -> bool {
    let Ok((inline_res, res_ids)) = pdf.get_page_resources(page_id) else {
        return false;
    };
    if inline_res.is_some_and(|d| dict_has_image_xobject(pdf, d, 0)) {
        return true;
    }
    for id in res_ids {
        if pdf
            .get_dictionary(id)
            .is_ok_and(|d| dict_has_image_xobject(pdf, d, 0))
        {
            return true;
        }
    }
    false
}

fn is_standard_pdf_punct(c: char) -> bool {
    matches!(
        c,
        '.' | ','
            | ';'
            | ':'
            | '-'
            | '_'
            | '/'
            | '('
            | ')'
            | '['
            | ']'
            | '\''
            | '"'
            | '?'
            | '!'
            | '。'
            | '，'
            | '、'
            | '；'
            | '：'
            | '？'
            | '！'
            | '（'
            | '）'
            | '【'
            | '】'
            | '《'
            | '》'
            | '“'
            | '”'
            | '‘'
            | '’'
            | '—'
            | '–'
            | '…'
            | '·'
    )
}

fn is_pdf_page_quarantined(text: &str, has_image_xobject: bool) -> bool {
    let mut non_ws_chars = 0usize;
    let mut garble_chars = 0usize;
    let mut valid_chars = 0usize;

    for ch in text.chars().filter(|c| !c.is_whitespace()) {
        non_ws_chars += 1;
        let is_garble =
            ch == '\u{FFFD}' || ('\u{E000}'..='\u{F8FF}').contains(&ch) || ch.is_control();
        if is_garble {
            garble_chars += 1;
        } else if ch.is_alphabetic() || ch.is_numeric() || is_standard_pdf_punct(ch) {
            valid_chars += 1;
        }
    }

    let garble_ratio = if non_ws_chars == 0 {
        0.0
    } else {
        garble_chars as f64 / non_ws_chars as f64
    };
    let valid_script_ratio = if non_ws_chars == 0 {
        1.0
    } else {
        valid_chars as f64 / non_ws_chars as f64
    };

    garble_ratio > 0.08
        || (non_ws_chars >= 20 && valid_script_ratio < 0.55)
        || (has_image_xobject && non_ws_chars < 40)
}

fn pdf(bytes: &[u8]) -> Result<Document> {
    let mut pdf = lopdf::Document::load_mem(bytes)?;
    if pdf.is_encrypted() && pdf.decrypt("").is_err() {
        return Err(IngestError::Invalid("encrypted PDF requires decryption"));
    }
    let pages = pdf.get_pages();
    if pages.len() > 2000 {
        return Err(IngestError::Invalid("PDF page limit exceeded"));
    }
    let mut extracted_pages: Vec<(u32, String, bool)> = Vec::with_capacity(pages.len());
    let mut total = 0usize;
    for (page_num, page_id) in &pages {
        let raw = pdf.extract_text(&[*page_num])?;
        let folded = fold_pdf_ligatures(&raw);
        total = total
            .checked_add(folded.len())
            .ok_or(IngestError::Limit(MAX_INPUT_BYTES))?;
        if total > MAX_INPUT_BYTES {
            return Err(IngestError::Limit(MAX_INPUT_BYTES));
        }
        let has_image = page_has_image_xobject(&pdf, *page_id);
        extracted_pages.push((*page_num, folded, has_image));
    }

    strip_repeating_pdf_headers_footers(&mut extracted_pages);

    let mut chunks = Vec::new();
    let mut quarantined_pages = 0usize;
    for (page_num, body, has_image) in extracted_pages {
        if is_pdf_page_quarantined(&body, has_image) {
            quarantined_pages += 1;
            continue;
        }
        if body.trim().is_empty() {
            continue;
        }
        let ranges = text::split_prose_ranges(
            &body,
            0..body.len(),
            text::TARGET_CHUNK_TOKENS,
            text::HARD_MAX_CHUNK_TOKENS,
        );
        for r in ranges {
            chunks.push(ChunkInput {
                heading: format!("Page {page_num}"),
                body: body[r].trim().to_owned(),
                anchor: AnchorSpec {
                    page: Some(page_num),
                    ..AnchorSpec::default()
                },
            });
        }
    }
    Ok(Document {
        media_type: "application/pdf",
        needs_ocr: quarantined_pages > 0 || chunks.is_empty(),
        chunks,
    })
}

pub fn add_file(
    store: &mut Store,
    path: &Path,
    options: &FunnelOptions,
    extractor: Option<&dyn Extractor>,
) -> Result<AddedDocument> {
    let bytes = crate::read_file(path)?;
    let document = parse(path, &bytes)?;
    let uri = path.to_string_lossy().into_owned();
    let key = format!("source:{uri}:{}", ContentHash::digest(&bytes)).parse()?;
    let mut inputs = Vec::new();
    if let Some(extractor) = extractor {
        for chunk in &document.chunks {
            for extracted in extractor.extract(&chunk.body, &uri)? {
                let mut input = base_fact(
                    store,
                    Memory {
                        subject: extracted.subject,
                        predicate: extracted.predicate,
                        value: extracted.value,
                        kind: extracted.kind,
                        logical_key: None,
                        tags: extracted.tags,
                        entities: extracted.entities,
                        valid: ValidityWindow {
                            valid_from: options.at,
                            valid_until: kp_model::EpochMs::OPEN_END,
                        },
                    },
                    options,
                )?;
                input.fact.source_anchor = Some(chunk.anchor.clone());
                inputs.push(input);
            }
        }
    }
    let chunks = document.chunks.len();
    let raw_input = RawSourceInput {
        uri: uri.clone(),
        media_type: document.media_type.into(),
        content: bytes,
        readable_by: options.audience.clone(),
        captured_at: options.at,
        chunks: document.chunks,
        self_echo_filtered: false,
    };
    let lib_id = store.library_id().clone();
    let (source, extracted) = if store.status()?.embedding_dims == 0 {
        store.ingest_funnel(
            &lib_id,
            &key,
            &raw_input,
            &kp_store::FtsOnlyEmbedder,
            &mut inputs,
        )?
    } else {
        store.ingest_funnel(
            &lib_id,
            &key,
            &raw_input,
            &kp_store::HashEmbedder,
            &mut inputs,
        )?
    };
    Ok(AddedDocument {
        uri,
        source,
        extracted,
        chunks,
        needs_ocr: document.needs_ocr,
    })
}
