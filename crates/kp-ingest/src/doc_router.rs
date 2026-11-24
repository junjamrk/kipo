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
                chunks.push(plain_chunk(source)?);
            }
            Document {
                media_type: "text/x-source",
                chunks,
                needs_ocr: false,
            }
        }
        "txt" | "csv" | "json" => {
            let source = text::decode(bytes)?;
            let media_type = match extension.as_str() {
                "json" => "application/json",
                "csv" => "text/csv",
                _ => "text/plain",
            };
            Document {
                media_type,
                chunks: vec![plain_chunk(&source)?],
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

fn plain_chunk(text: &str) -> Result<ChunkInput> {
    Ok(ChunkInput {
        heading: String::new(),
        body: text.into(),
        anchor: text::line_anchor(text, 0..text.len())?,
    })
}

fn pdf(bytes: &[u8]) -> Result<Document> {
    let pdf = lopdf::Document::load_mem(bytes)?;
    let pages = pdf.get_pages();
    if pages.len() > 2000 {
        return Err(IngestError::Invalid("PDF page limit exceeded"));
    }
    let mut chunks = Vec::new();
    let mut total = 0usize;
    for page in pages.keys() {
        let body = pdf.extract_text(&[*page])?;
        total = total
            .checked_add(body.len())
            .ok_or(IngestError::Limit(MAX_INPUT_BYTES))?;
        if total > MAX_INPUT_BYTES {
            return Err(IngestError::Limit(MAX_INPUT_BYTES));
        }
        if !body.trim().is_empty() {
            chunks.push(ChunkInput {
                heading: format!("Page {page}"),
                body,
                anchor: AnchorSpec {
                    page: Some(*page),
                    ..AnchorSpec::default()
                },
            });
        }
    }
    Ok(Document {
        media_type: "application/pdf",
        needs_ocr: chunks.is_empty(),
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
