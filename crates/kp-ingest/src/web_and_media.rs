use std::net::IpAddr;
use std::path::Path;
use std::sync::LazyLock;
use std::time::Duration;

use kp_derive::{base_fact, Extractor, FunnelOptions, Memory};
use kp_model::{AnchorSpec, ContentHash, TrustTier, ValidityWindow};
use kp_store::{ChunkInput, RawSourceInput, Store};
use regex::Regex;

use crate::doc_router::{AddedDocument, Document};
use crate::{subtitles, text, IngestError, Result, MAX_INPUT_BYTES};

static STRIP_BLOCK_RE: LazyLock<std::result::Result<Regex, regex::Error>> = LazyLock::new(|| {
    Regex::new(
        r"(?is)<!--.*?-->|<(script|style|noscript|svg|template|title)\b[^>]*>.*?</(script|style|noscript|svg|template|title)>",
    )
});
static HEADING_TAG_RE: LazyLock<std::result::Result<Regex, regex::Error>> =
    LazyLock::new(|| Regex::new(r"(?is)<h([1-6])\b[^>]*>(.*?)</h[1-6]>"));
static TITLE_TAG_RE: LazyLock<std::result::Result<Regex, regex::Error>> =
    LazyLock::new(|| Regex::new(r"(?is)<title\b[^>]*>(.*?)</title>"));
static CODE_TAG_RE: LazyLock<std::result::Result<Regex, regex::Error>> =
    LazyLock::new(|| Regex::new(r"(?is)<code\b[^>]*>(.*?)</code>"));
static BLOCK_BREAK_RE: LazyLock<std::result::Result<Regex, regex::Error>> = LazyLock::new(|| {
    Regex::new(
        r"(?i)</?(p|div|section|article|main|header|footer|aside|li|ul|ol|tr|table|blockquote|pre|br|hr)\b[^>]*>",
    )
});
static ANY_TAG_RE: LazyLock<std::result::Result<Regex, regex::Error>> =
    LazyLock::new(|| Regex::new(r"(?s)<[^>]+>"));

pub(crate) fn is_media_extension(ext: &str) -> bool {
    matches!(
        ext,
        "mp4" | "mov" | "mkv" | "webm" | "avi" | "mp3" | "m4a" | "wav" | "flac" | "ogg" | "aac"
    )
}

pub(crate) fn media_mime_type(ext: &str) -> &'static str {
    match ext {
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "avi" => "video/x-msvideo",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "ogg" => "audio/ogg",
        "aac" => "audio/aac",
        _ => "application/octet-stream",
    }
}

pub(crate) fn parse_html(source: &str) -> Result<Vec<ChunkInput>> {
    let strip_re = STRIP_BLOCK_RE.as_ref().map_err(Clone::clone)?;
    let heading_re = HEADING_TAG_RE.as_ref().map_err(Clone::clone)?;
    let title_re = TITLE_TAG_RE.as_ref().map_err(Clone::clone)?;
    let code_re = CODE_TAG_RE.as_ref().map_err(Clone::clone)?;
    let block_re = BLOCK_BREAK_RE.as_ref().map_err(Clone::clone)?;
    let tag_re = ANY_TAG_RE.as_ref().map_err(Clone::clone)?;

    let doc_title = title_re
        .captures(source)
        .and_then(|c| c.get(1))
        .map(|m| clean_html_fragment(m.as_str(), code_re, block_re, tag_re))
        .unwrap_or_default();

    let cleaned_html = strip_re.replace_all(source, "\n");
    let mut splits: Vec<(usize, usize, String)> = Vec::new();
    for cap in heading_re.captures_iter(&cleaned_html) {
        let Some(full) = cap.get(0) else {
            continue;
        };
        let heading_txt = cap
            .get(2)
            .map(|m| clean_html_fragment(m.as_str(), code_re, block_re, tag_re))
            .unwrap_or_default();
        splits.push((full.start(), full.end(), heading_txt));
    }

    let mut chunks = Vec::new();
    if splits.is_empty() {
        let body = clean_html_fragment(&cleaned_html, code_re, block_re, tag_re);
        if !body.is_empty() {
            chunks.push(ChunkInput {
                heading: doc_title,
                body,
                anchor: AnchorSpec::default(),
            });
        }
        return Ok(chunks);
    }

    if splits[0].0 > 0 {
        let lead = clean_html_fragment(&cleaned_html[..splits[0].0], code_re, block_re, tag_re);
        if !lead.is_empty() {
            chunks.push(ChunkInput {
                heading: doc_title.clone(),
                body: lead,
                anchor: AnchorSpec::default(),
            });
        }
    }

    for (idx, (_, h_end, h_text)) in splits.iter().enumerate() {
        let next_start = splits
            .get(idx + 1)
            .map(|s| s.0)
            .unwrap_or(cleaned_html.len());
        let section_body =
            clean_html_fragment(&cleaned_html[*h_end..next_start], code_re, block_re, tag_re);
        let combined = match (h_text.is_empty(), section_body.is_empty()) {
            (false, false) => format!("{h_text}\n{section_body}"),
            (false, true) => h_text.clone(),
            (true, false) => section_body,
            (true, true) => continue,
        };
        let heading = if h_text.is_empty() {
            doc_title.clone()
        } else {
            h_text.clone()
        };
        chunks.push(ChunkInput {
            heading,
            body: combined,
            anchor: AnchorSpec::default(),
        });
    }

    Ok(chunks)
}

fn clean_html_fragment(raw: &str, code_re: &Regex, block_re: &Regex, tag_re: &Regex) -> String {
    let with_code = code_re.replace_all(raw, "`$1`");
    let with_breaks = block_re.replace_all(&with_code, "\n");
    let stripped = tag_re.replace_all(&with_breaks, " ");
    let decoded = decode_html_entities(&stripped);
    decoded
        .lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn decode_html_entities(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp + 1..];
        if let Some(semi) = after.bytes().take(12).position(|b| b == b';') {
            let entity = &after[..semi];
            let decoded = match entity {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" | "#39" => Some('\''),
                "nbsp" => Some(' '),
                s if s.starts_with("#x") || s.starts_with("#X") => u32::from_str_radix(&s[2..], 16)
                    .ok()
                    .and_then(char::from_u32),
                s if s.starts_with('#') => s[1..].parse::<u32>().ok().and_then(char::from_u32),
                _ => None,
            };
            if let Some(ch) = decoded {
                out.push(ch);
                rest = &after[semi + 1..];
                continue;
            }
        }
        out.push('&');
        rest = after;
    }
    out.push_str(rest);
    out
}

pub(crate) fn parse_media_sidecar(path: &Path, ext: &str) -> Result<(Document, Option<Vec<u8>>)> {
    let media_type = media_mime_type(ext);
    let Some(parent) = path.parent() else {
        return Ok((
            Document {
                media_type,
                chunks: Vec::new(),
                needs_ocr: true,
            },
            None,
        ));
    };
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let fname = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    let candidates = [
        format!("{stem}.vtt"),
        format!("{stem}.srt"),
        format!("{stem}.lrc"),
        format!("{stem}.whisper.json"),
        format!("{stem}.transcript.json"),
        format!("{stem}.json"),
        format!("{stem}.md"),
        format!("{stem}.txt"),
        format!("{fname}.vtt"),
        format!("{fname}.srt"),
        format!("{fname}.lrc"),
    ];

    for cand_name in candidates {
        if stem.is_empty() && !cand_name.starts_with(fname) {
            continue;
        }
        let sidecar = parent.join(&cand_name);
        let Ok(meta) = std::fs::symlink_metadata(&sidecar) else {
            continue;
        };
        if meta.file_type().is_symlink() || !meta.is_file() {
            continue;
        }
        let bytes = crate::read_file(&sidecar)?;
        let side_ext = sidecar
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let chunks = match side_ext.as_str() {
            "vtt" | "srt" => subtitles::parse(&text::decode(&bytes)?)?,
            "lrc" => subtitles::parse_lrc(&text::decode(&bytes)?)?,
            "json" => match subtitles::try_parse_asr_json(&text::decode(&bytes)?) {
                Some(c) => c,
                None => continue,
            },
            "md" => text::markdown(&text::decode(&bytes)?)?,
            "txt" => {
                let decoded = text::decode(&bytes)?;
                if decoded.trim().is_empty() {
                    continue;
                }
                vec![ChunkInput {
                    heading: String::new(),
                    body: decoded.trim().to_owned(),
                    anchor: AnchorSpec::default(),
                }]
            }
            _ => continue,
        };
        if chunks.is_empty() {
            continue;
        }
        let media_path_str = path.to_string_lossy().into_owned();
        let mut anchored = chunks;
        for ch in &mut anchored {
            ch.anchor.file_path = Some(media_path_str.clone());
        }
        return Ok((
            Document {
                media_type,
                chunks: anchored,
                needs_ocr: false,
            },
            Some(bytes),
        ));
    }

    Ok((
        Document {
            media_type,
            chunks: Vec::new(),
            needs_ocr: true,
        },
        None,
    ))
}

fn is_disallowed_ip(ip: IpAddr, allow_loopback: bool) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            if v4.is_loopback() {
                return !allow_loopback;
            }
            v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_documentation()
                || v4.is_unspecified()
                || v4.octets()[0] == 100 && (v4.octets()[1] & 0b1100_0000) == 64
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback() {
                return !allow_loopback;
            }
            v6.is_unspecified()
                || v6.is_unique_local()
                || v6.is_unicast_link_local()
                || v6
                    .to_ipv4_mapped()
                    .is_some_and(|v4| is_disallowed_ip(IpAddr::V4(v4), allow_loopback))
        }
    }
}

pub async fn add_url(
    store: &mut Store,
    raw_url: &str,
    options: &FunnelOptions,
    trusted: bool,
    extractor: Option<&dyn Extractor>,
) -> Result<AddedDocument> {
    let parsed = reqwest::Url::parse(raw_url.trim())
        .map_err(|_| IngestError::Invalid("invalid HTTP/HTTPS URL"))?;
    if !matches!(parsed.scheme(), "http" | "https")
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.fragment().is_some()
    {
        return Err(IngestError::Invalid(
            "URL must be http or https without credentials or fragment",
        ));
    }
    let host = parsed
        .host_str()
        .ok_or(IngestError::Invalid("URL missing host"))?;
    let allow_local = std::env::var_os("KP_ALLOW_LOCAL_URL_INGEST").is_some();
    if host.eq_ignore_ascii_case("localhost") || host.ends_with(".local") {
        if !allow_local {
            return Err(IngestError::Invalid(
                "loopback or .local URL blocked by SSRF guard",
            ));
        }
    } else if let Ok(ip) = host.trim_matches(['[', ']']).parse::<IpAddr>() {
        if is_disallowed_ip(ip, allow_local) {
            return Err(IngestError::Invalid(
                "private or loopback IP URL blocked by SSRF guard",
            ));
        }
    } else {
        let port = parsed.port_or_known_default().unwrap_or(443);
        let addrs: Vec<_> = tokio::net::lookup_host((host, port))
            .await
            .map_err(|_| IngestError::Invalid("failed to resolve URL host"))?
            .collect();
        if addrs.is_empty()
            || addrs
                .iter()
                .any(|sa| is_disallowed_ip(sa.ip(), allow_local))
        {
            return Err(IngestError::Invalid(
                "URL resolved to private or disallowed IP",
            ));
        }
    }

    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .user_agent("kipo-ingest/0.1")
        .build()
        .map_err(|_| IngestError::Invalid("HTTP client initialization failed"))?;

    let mut resp = client
        .get(parsed.clone())
        .send()
        .await
        .map_err(|_| IngestError::Invalid("URL fetch failed"))?;
    if !resp.status().is_success() {
        return Err(IngestError::Invalid("URL returned non-success HTTP status"));
    }
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("text/html")
        .split(';')
        .next()
        .unwrap_or("text/html")
        .trim()
        .to_ascii_lowercase();

    let mut bytes = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|_| IngestError::Invalid("URL stream read failed"))?
    {
        if bytes.len().saturating_add(chunk.len()) > MAX_INPUT_BYTES {
            return Err(IngestError::Limit(MAX_INPUT_BYTES));
        }
        bytes.extend_from_slice(&chunk);
    }

    let path_lower = parsed.path().to_ascii_lowercase();
    let (media_type, mut chunks, needs_ocr) =
        if content_type == "application/pdf" || path_lower.ends_with(".pdf") {
            let doc = crate::doc_router::parse(Path::new("remote.pdf"), &bytes)?;
            (doc.media_type, doc.chunks, doc.needs_ocr)
        } else if content_type == "text/vtt" || path_lower.ends_with(".vtt") {
            ("text/vtt", subtitles::parse(&text::decode(&bytes)?)?, false)
        } else if content_type == "application/x-subrip" || path_lower.ends_with(".srt") {
            (
                "application/x-subrip",
                subtitles::parse(&text::decode(&bytes)?)?,
                false,
            )
        } else if content_type == "text/markdown"
            || path_lower.ends_with(".md")
            || path_lower.ends_with(".markdown")
        {
            (
                "text/markdown",
                text::markdown(&text::decode(&bytes)?)?,
                false,
            )
        } else if content_type.contains("json") || path_lower.ends_with(".json") {
            let decoded = text::decode(&bytes)?;
            if let Some(asr) = subtitles::try_parse_asr_json(&decoded) {
                ("application/x-whisper+json", asr, false)
            } else {
                (
                    "application/json",
                    vec![ChunkInput {
                        heading: String::new(),
                        body: decoded.into_owned(),
                        anchor: AnchorSpec::default(),
                    }],
                    false,
                )
            }
        } else if content_type.contains("html")
            || path_lower.ends_with(".html")
            || path_lower.ends_with(".htm")
        {
            ("text/html", parse_html(&text::decode(&bytes)?)?, false)
        } else {
            let decoded = text::decode(&bytes)?;
            (
                "text/plain",
                vec![ChunkInput {
                    heading: String::new(),
                    body: decoded.into_owned(),
                    anchor: AnchorSpec::default(),
                }],
                false,
            )
        };

    let uri = parsed.as_str().to_owned();
    for ch in &mut chunks {
        ch.anchor.file_path = Some(uri.clone());
    }
    let key = format!("source:{uri}:{}", ContentHash::digest(&bytes)).parse()?;
    let mut inputs = Vec::new();
    if let Some(extractor) = extractor {
        for chunk in &chunks {
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
                if !trusted {
                    input.fact.trust_tier = TrustTier::ExternalUntrusted;
                }
                input.fact.source_anchor = Some(chunk.anchor.clone());
                inputs.push(input);
            }
        }
    }
    let chunk_count = chunks.len();
    let raw_input = RawSourceInput {
        uri: uri.clone(),
        media_type: media_type.into(),
        content: bytes,
        readable_by: options.audience.clone(),
        captured_at: options.at,
        chunks,
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
        chunks: chunk_count,
        needs_ocr,
    })
}
