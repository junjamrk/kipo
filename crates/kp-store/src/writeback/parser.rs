use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use kp_model::{
    ActorId, AudienceSpec, ContentHash, DerivationView, EpochMs, Fact, FactId, FactKind, KpError,
    LibraryId, ModelRoutePolicy, ProjectionPageMeta, SourceId, SourceScopeId,
};
use rusqlite::{Connection, OptionalExtension};
use serde_json::Value;

use super::merge_route_policy;
use crate::export::{
    compute_source_summary_excerpt, format_kp_ref, format_source_markdown_bullet,
    sanitize_md_inline,
};
use crate::facts::source_effective_route_policy_in_tx;
use crate::slots::{read_slot, validate_value};
use crate::snapshot::{lookup_scope_label, strip_turn_tags_and_assistant_lines};
use crate::Result;

#[derive(Clone, Debug)]
pub(super) struct ParsedFrontmatter {
    pub(super) page_slug: String,
    pub(super) library_id: LibraryId,
    pub(super) viewer: ActorId,
    pub(super) base_seq: i64,
    pub(super) exported_at: EpochMs,
}

#[derive(Clone, Debug)]
pub(super) struct ParsedEntryBlock {
    pub(super) fact_id_raw: String,
    pub(super) raw_block: String,
    pub(super) content_lines: Vec<String>,
}

#[derive(Clone, Debug)]
pub(super) struct FreeformCandidate {
    pub(super) source_file: String,
    pub(super) ordinal: usize,
    pub(super) raw_markdown: String,
    pub(super) route_policy: Option<ModelRoutePolicy>,
}

#[derive(Clone, Debug)]
struct ParsedSourceEntryBlock {
    source_id_raw: String,
    raw_block: String,
    content_lines: Vec<String>,
}

#[derive(Clone, Debug)]
struct ParsedSourcesDocument {
    entries: Vec<ParsedSourceEntryBlock>,
    freeform_blocks: Vec<String>,
}

#[derive(Clone, Debug)]
pub(super) struct ParsedIndexDocument {
    pub(super) entries: Vec<ParsedEntryBlock>,
    pub(super) freeform_blocks: Vec<String>,
}

#[derive(Clone, Debug)]
pub(super) struct ParsedStandardBullet {
    pub(super) kp_ref: String,
    pub(super) claim: String,
}

pub(super) fn parse_frontmatter(raw: &str) -> Result<(ParsedFrontmatter, &str)> {
    let rest = raw
        .strip_prefix("---\n")
        .or_else(|| raw.strip_prefix("---\r\n"))
        .ok_or_else(|| KpError::invalid("frontmatter", "missing opening YAML frontmatter `---`"))?;

    let (fm_text, body) = find_frontmatter_end(rest)
        .ok_or_else(|| KpError::invalid("frontmatter", "missing closing YAML frontmatter `---`"))?;

    let mut page_slug: Option<String> = None;
    let mut library_id: Option<LibraryId> = None;
    let mut viewer: Option<ActorId> = None;
    let mut base_seq: Option<i64> = None;
    let mut exported_at: Option<EpochMs> = None;

    for raw_line in fm_text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once(':') else {
            return Err(KpError::invalid("frontmatter", "malformed frontmatter line").into());
        };
        let key = k.trim();
        let val = v.trim().trim_matches('"').trim_matches('\'');
        match key {
            "page_slug" => page_slug = Some(val.to_owned()),
            "library_id" => library_id = Some(val.parse()?),
            "viewer" => viewer = Some(val.parse()?),
            "base_seq" => {
                let parsed: i64 = val
                    .parse()
                    .map_err(|_| KpError::invalid("base_seq", "invalid integer base_seq"))?;
                if parsed < 0 {
                    return Err(
                        KpError::invalid("base_seq", "base_seq must be non-negative").into(),
                    );
                }
                base_seq = Some(parsed);
            }
            "exported_at" => {
                let parsed: i64 = val
                    .parse()
                    .map_err(|_| KpError::invalid("exported_at", "invalid integer exported_at"))?;
                exported_at = Some(EpochMs(parsed));
            }
            _ => {}
        }
    }

    Ok((
        ParsedFrontmatter {
            page_slug: page_slug
                .ok_or_else(|| KpError::invalid("page_slug", "missing page_slug in frontmatter"))?,
            library_id: library_id.ok_or_else(|| {
                KpError::invalid("library_id", "missing library_id in frontmatter")
            })?,
            viewer: viewer
                .ok_or_else(|| KpError::invalid("viewer", "missing viewer in frontmatter"))?,
            base_seq: base_seq
                .ok_or_else(|| KpError::invalid("base_seq", "missing base_seq in frontmatter"))?,
            exported_at: exported_at.ok_or_else(|| {
                KpError::invalid("exported_at", "missing exported_at in frontmatter")
            })?,
        },
        body,
    ))
}

fn find_frontmatter_end(rest: &str) -> Option<(&str, &str)> {
    let mut offset = 0;
    for segment in rest.split_inclusive('\n') {
        let trimmed = segment.trim_end_matches(['\r', '\n']);
        if trimmed == "---" {
            let fm = &rest[..offset];
            let after = &rest[offset + segment.len()..];
            return Some((fm, after));
        }
        offset += segment.len();
    }
    if rest.trim_end_matches('\r').ends_with("\n---") || rest.trim() == "---" {
        if let Some(idx) = rest.rfind("---") {
            return Some((&rest[..idx], ""));
        }
    }
    None
}

pub(super) fn parse_kp_entry_marker(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    let inner = trimmed
        .strip_prefix("<!--")?
        .strip_suffix("-->")?
        .trim()
        .strip_prefix("kp-entry:")?
        .trim();
    (!inner.is_empty()).then_some(inner)
}

fn is_standard_index_boilerplate_line(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.is_empty()
        || trimmed == "_None._"
        || trimmed.starts_with("# Kipo Projection (")
        || matches!(
            trimmed,
            "## Rules & Constraints"
                | "## Decisions"
                | "## Warnings & Derived Insights"
                | "## Facts & Events"
        )
}

pub(super) fn is_standard_sub_bullet(line: &str) -> bool {
    line.starts_with("  - Quote: ") || line.starts_with("  - Premise ")
}

pub(super) fn parse_index_body(body: &str) -> ParsedIndexDocument {
    let lines: Vec<&str> = body.lines().collect();
    let mut entries = Vec::new();
    let mut freeform_blocks = Vec::new();
    let mut cur_freeform: Vec<&str> = Vec::new();

    let flush_freeform = |buf: &mut Vec<&str>, out: &mut Vec<String>| {
        if !buf.is_empty() {
            let joined = buf.join("\n").trim().to_owned();
            if !joined.is_empty() {
                out.push(joined);
            }
            buf.clear();
        }
    };

    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if let Some(fact_id_raw) = parse_kp_entry_marker(line) {
            flush_freeform(&mut cur_freeform, &mut freeform_blocks);
            let mut raw_lines = vec![line];
            let mut content_lines = Vec::new();
            i += 1;

            // Collect primary bullet/line and any indented sub-lines belonging to this entry.
            let mut saw_primary = false;
            while i < lines.len() {
                let next = lines[i];
                let next_trimmed = next.trim();
                if next_trimmed.is_empty()
                    || parse_kp_entry_marker(next).is_some()
                    || next_trimmed.starts_with('#')
                    || next_trimmed == "_None._"
                {
                    break;
                }
                if !saw_primary {
                    saw_primary = true;
                    raw_lines.push(next);
                    content_lines.push(next.to_owned());
                    i += 1;
                } else if next.starts_with("  ") || next.starts_with('\t') {
                    raw_lines.push(next);
                    content_lines.push(next.to_owned());
                    i += 1;
                } else {
                    // An unindented second bullet or paragraph after the primary entry line is a
                    // separate user-added freeform block.
                    break;
                }
            }

            entries.push(ParsedEntryBlock {
                fact_id_raw: fact_id_raw.to_owned(),
                raw_block: raw_lines.join("\n"),
                content_lines,
            });
            continue;
        }

        if line.trim().is_empty() {
            let only_headings = !cur_freeform.is_empty()
                && cur_freeform
                    .iter()
                    .all(|l| l.trim().is_empty() || l.trim().starts_with('#'));
            if only_headings {
                if cur_freeform.last().is_none_or(|last| !last.is_empty()) {
                    cur_freeform.push("");
                }
            } else {
                flush_freeform(&mut cur_freeform, &mut freeform_blocks);
            }
        } else if is_standard_index_boilerplate_line(line) {
            flush_freeform(&mut cur_freeform, &mut freeform_blocks);
        } else {
            cur_freeform.push(line);
        }
        i += 1;
    }
    flush_freeform(&mut cur_freeform, &mut freeform_blocks);

    ParsedIndexDocument {
        entries,
        freeform_blocks,
    }
}

pub(super) fn parse_standard_entry_bullet(
    line: &str,
    expected_source_label: Option<&str>,
) -> Option<ParsedStandardBullet> {
    let rest = line.strip_prefix("- **[kp-ref:")?;
    let (kp_ref, after_ref) = rest.split_once("]** (`")?;
    if kp_ref.trim().is_empty() {
        return None;
    }
    let (_kind, after_kind) = after_ref.split_once("` \u{00B7} `")?;
    let (_author, after_author) = after_kind.split_once("` @ `")?;

    let after_meta = if let Some(src_lbl) = expected_source_label {
        let prefix = format!("{src_lbl}`)");
        if let Some(rem) = after_author.strip_prefix(&prefix) {
            rem
        } else {
            let (_src, rem) = after_author.split_once("`)")?;
            rem
        }
    } else {
        let (_src, rem) = after_author.split_once("`)")?;
        rem
    };

    let claim = after_meta.trim();
    if claim.is_empty() {
        return None;
    }
    Some(ParsedStandardBullet {
        kp_ref: kp_ref.trim().to_owned(),
        claim: claim.to_owned(),
    })
}

pub(super) fn kp_ref_matches_fact_id(kp_ref: &str, fact_id: &FactId) -> bool {
    let expected = format_kp_ref(fact_id);
    kp_ref == expected || kp_ref == fact_id.as_str()
}

pub(super) struct ScannedMarkdownDir {
    pub(super) index_bytes: Vec<u8>,
    pub(super) sources_bytes: Option<Vec<u8>>,
    pub(super) extra_md_files: Vec<(String, Vec<u8>)>,
}

pub(super) fn scan_markdown_directory(dir: &Path) -> Result<ScannedMarkdownDir> {
    let dir_meta = fs::symlink_metadata(dir)?;
    if dir_meta.file_type().is_symlink() {
        return Err(KpError::invalid(
            "dir",
            "symlinks are not permitted for markdown projection directories",
        )
        .into());
    }
    if !dir_meta.is_dir() {
        return Err(KpError::invalid("dir", "projection path is not a directory").into());
    }

    let mut index_bytes: Option<Vec<u8>> = None;
    let mut sources_bytes: Option<Vec<u8>> = None;
    let mut extra_md_files: Vec<(String, Vec<u8>)> = Vec::new();

    collect_markdown_files_rec(
        dir,
        dir,
        &mut index_bytes,
        &mut sources_bytes,
        &mut extra_md_files,
    )?;

    let index = index_bytes
        .ok_or_else(|| KpError::invalid("index.md", "missing `index.md` in export directory"))?;
    extra_md_files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(ScannedMarkdownDir {
        index_bytes: index,
        sources_bytes,
        extra_md_files,
    })
}

fn collect_markdown_files_rec(
    root: &Path,
    cur: &Path,
    index_bytes: &mut Option<Vec<u8>>,
    sources_bytes: &mut Option<Vec<u8>>,
    extra_md_files: &mut Vec<(String, Vec<u8>)>,
) -> Result<()> {
    let mut entries: Vec<PathBuf> = Vec::new();
    for entry_res in fs::read_dir(cur)? {
        let entry = entry_res?;
        entries.push(entry.path());
    }
    entries.sort();

    for path in entries {
        let Some(name_os) = path.file_name() else {
            continue;
        };
        let name = name_os.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        let meta = fs::symlink_metadata(&path)?;
        if meta.file_type().is_symlink() {
            return Err(KpError::invalid(
                "path",
                "symlinks are not permitted inside markdown projection directory",
            )
            .into());
        }
        if meta.is_dir() {
            collect_markdown_files_rec(root, &path, index_bytes, sources_bytes, extra_md_files)?;
            continue;
        }
        if !meta.is_file() {
            continue;
        }
        let is_md = path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("md"));
        if !is_md {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let bytes = fs::read(&path)?;
        match rel.as_str() {
            "index.md" => *index_bytes = Some(bytes),
            "sources.md" => *sources_bytes = Some(bytes),
            _ => extra_md_files.push((rel, bytes)),
        }
    }
    Ok(())
}

fn parse_kp_source_marker(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    let inner = trimmed
        .strip_prefix("<!--")?
        .strip_suffix("-->")?
        .trim()
        .strip_prefix("kp-source:")?
        .trim();
    (!inner.is_empty()).then_some(inner)
}

fn is_standard_sources_boilerplate_line(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.is_empty() || trimmed == "_None._" || trimmed.starts_with("# Readable Sources (")
}

fn parse_sources_body(body: &str) -> ParsedSourcesDocument {
    let lines: Vec<&str> = body.lines().collect();
    let mut entries = Vec::new();
    let mut freeform_blocks = Vec::new();
    let mut cur_freeform: Vec<&str> = Vec::new();

    let flush_freeform = |buf: &mut Vec<&str>, out: &mut Vec<String>| {
        if !buf.is_empty() {
            let joined = buf.join("\n").trim().to_owned();
            if !joined.is_empty() {
                out.push(joined);
            }
            buf.clear();
        }
    };

    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if let Some(source_id_raw) = parse_kp_source_marker(line) {
            flush_freeform(&mut cur_freeform, &mut freeform_blocks);
            let mut raw_lines = vec![line];
            let mut content_lines = Vec::new();
            i += 1;

            let mut saw_primary = false;
            while i < lines.len() {
                let next = lines[i];
                let next_trimmed = next.trim();
                if next_trimmed.is_empty()
                    || parse_kp_source_marker(next).is_some()
                    || next_trimmed.starts_with('#')
                    || next_trimmed == "_None._"
                {
                    break;
                }
                if !saw_primary {
                    saw_primary = true;
                    raw_lines.push(next);
                    content_lines.push(next.to_owned());
                    i += 1;
                } else if next.starts_with("  ") || next.starts_with('\t') {
                    raw_lines.push(next);
                    content_lines.push(next.to_owned());
                    i += 1;
                } else {
                    break;
                }
            }

            entries.push(ParsedSourceEntryBlock {
                source_id_raw: source_id_raw.to_owned(),
                raw_block: raw_lines.join("\n"),
                content_lines,
            });
            continue;
        }

        if line.trim().is_empty() {
            let only_headings = !cur_freeform.is_empty()
                && cur_freeform
                    .iter()
                    .all(|l| l.trim().is_empty() || l.trim().starts_with('#'));
            if only_headings {
                if cur_freeform.last().is_none_or(|last| !last.is_empty()) {
                    cur_freeform.push("");
                }
            } else {
                flush_freeform(&mut cur_freeform, &mut freeform_blocks);
            }
        } else if is_standard_sources_boilerplate_line(line) {
            flush_freeform(&mut cur_freeform, &mut freeform_blocks);
        } else {
            cur_freeform.push(line);
        }
        i += 1;
    }
    flush_freeform(&mut cur_freeform, &mut freeform_blocks);

    ParsedSourcesDocument {
        entries,
        freeform_blocks,
    }
}

struct SourceRenderInfo {
    rendered_bullet: String,
    uri_inline: String,
    media_inline: String,
    chunk_count: usize,
    captured_at: i64,
    summary_excerpt: String,
    live_scope_allows: bool,
}

fn lookup_source_render_info_tx(
    tx: &Connection,
    graph: &DerivationView,
    source_id: &str,
    viewer: &ActorId,
) -> Result<Option<SourceRenderInfo>> {
    struct RawSrcMeta {
        uri: String,
        media_type: String,
        captured_at: i64,
        scope_id: Option<String>,
        is_tombstoned: bool,
    }
    let meta_opt: Option<RawSrcMeta> = tx
        .query_row(
            "SELECT s.uri,
                    s.media_type,
                    s.captured_at,
                    s.source_scope_id,
                    EXISTS(SELECT 1 FROM tombstones t WHERE t.tombstone_id = s.source_id)
             FROM raw_sources s
             WHERE s.source_id = ?1",
            [source_id],
            |r| {
                Ok(RawSrcMeta {
                    uri: r.get(0)?,
                    media_type: r.get(1)?,
                    captured_at: r.get(2)?,
                    scope_id: r.get(3)?,
                    is_tombstoned: r.get(4)?,
                })
            },
        )
        .optional()?;
    let Some(meta) = meta_opt else {
        return Ok(None);
    };
    if meta.is_tombstoned {
        return Ok(None);
    }

    let mut chunk_stmt = tx.prepare_cached(
        "SELECT heading, body, readable_by_json
         FROM chunks
         WHERE source_id = ?1
         ORDER BY ordinal ASC",
    )?;
    let mut rows = chunk_stmt.query([source_id])?;
    let mut visible_chunks = Vec::<(String, String)>::new();
    while let Some(r) = rows.next()? {
        let heading: String = r.get(0)?;
        let body: String = r.get(1)?;
        let aud_raw: String = r.get(2)?;
        if let Ok(aud) = serde_json::from_str::<AudienceSpec>(&aud_raw) {
            if aud.allows(viewer, &graph.members) {
                visible_chunks.push((heading, body));
            }
        }
    }
    if visible_chunks.is_empty() {
        return Ok(None);
    }

    let live_scope_allows = match &meta.scope_id {
        None => true,
        Some(sid_str) => sid_str
            .parse::<SourceScopeId>()
            .ok()
            .and_then(|sid| graph.source_scopes.get(&sid))
            .is_some_and(|aud| aud.allows(viewer, &graph.members)),
    };
    let scope_label = match meta.scope_id.as_deref() {
        Some(sid) => lookup_scope_label(tx, sid)?,
        None => None,
    };

    let (first_heading, first_body) = &visible_chunks[0];
    let summary_excerpt = compute_source_summary_excerpt(first_heading, first_body);
    let rendered_bullet = format_source_markdown_bullet(
        &meta.uri,
        source_id,
        &meta.media_type,
        visible_chunks.len(),
        meta.captured_at,
        scope_label.as_deref(),
        &summary_excerpt,
    );

    Ok(Some(SourceRenderInfo {
        rendered_bullet,
        uri_inline: sanitize_md_inline(&meta.uri),
        media_inline: sanitize_md_inline(&meta.media_type),
        chunk_count: visible_chunks.len(),
        captured_at: meta.captured_at,
        summary_excerpt,
        live_scope_allows,
    }))
}

fn is_same_source_bullet_ignoring_container_rename(
    line: &str,
    info: &SourceRenderInfo,
    source_id: &str,
) -> bool {
    let expected_prefix = format!(
        "- **`{}`** (`{source_id}` · `{}` · {} chunk(s) · captured `{}`",
        info.uri_inline, info.media_inline, info.chunk_count, info.captured_at
    );
    let Some(rest) = line.strip_prefix(&expected_prefix) else {
        return false;
    };
    let after_close = if let Some(rem) = rest.strip_prefix("): ") {
        rem
    } else if let Some(after_container) = rest.strip_prefix(" · container `") {
        let Some((_lbl, rem)) = after_container.split_once("`): ") else {
            return false;
        };
        rem
    } else {
        return false;
    };
    after_close == info.summary_excerpt
}

pub(super) struct SourcesMdExtractCtx<'a> {
    pub(super) graph: &'a DerivationView,
    pub(super) sources_bytes: &'a [u8],
    pub(super) sources_meta: Option<&'a ProjectionPageMeta>,
    pub(super) expected_library_id: &'a LibraryId,
    pub(super) expected_viewer: &'a ActorId,
    pub(super) default_policy: &'a ModelRoutePolicy,
    pub(super) redactor: &'a kp_model::SecretRedactor,
}

pub(super) fn extract_sources_md_candidates_tx(
    tx: &Connection,
    ctx: &SourcesMdExtractCtx<'_>,
) -> Result<Vec<FreeformCandidate>> {
    let SourcesMdExtractCtx {
        graph,
        sources_bytes,
        sources_meta,
        expected_library_id,
        expected_viewer,
        default_policy,
        redactor,
    } = *ctx;
    let sources_sha = ContentHash::digest(sources_bytes);
    if sources_meta.is_some_and(|m| m.page_sha256 == sources_sha) {
        return Ok(Vec::new());
    }
    let raw_str = String::from_utf8(sources_bytes.to_vec())
        .map_err(|_| KpError::invalid("sources.md", "`sources.md` is not valid UTF-8"))?;

    let Ok((fm, body)) = parse_frontmatter(&raw_str) else {
        let redacted = redactor.redact(raw_str.trim());
        return Ok((!redacted.trim().is_empty())
            .then(|| FreeformCandidate {
                source_file: "sources.md".to_owned(),
                ordinal: 0,
                raw_markdown: redacted,
                route_policy: Some(default_policy.clone()),
            })
            .into_iter()
            .collect());
    };
    if fm.page_slug != "sources"
        || fm.library_id != *expected_library_id
        || fm.viewer != *expected_viewer
    {
        let redacted = redactor.redact(raw_str.trim());
        return Ok((!redacted.trim().is_empty())
            .then(|| FreeformCandidate {
                source_file: "sources.md".to_owned(),
                ordinal: 0,
                raw_markdown: redacted,
                route_policy: Some(default_policy.clone()),
            })
            .into_iter()
            .collect());
    }

    let existed_sources_at_base: BTreeSet<String> = {
        let mut stmt = tx.prepare(
            "SELECT DISTINCT target_id FROM change_log
             WHERE op = 'ingest_source' AND seq <= ?1 AND target_id IS NOT NULL",
        )?;
        let mut rows = stmt.query([fm.base_seq])?;
        let mut out = BTreeSet::new();
        while let Some(r) = rows.next()? {
            out.insert(r.get::<_, String>(0)?);
        }
        out
    };

    let parsed = parse_sources_body(body);
    let mut out = Vec::new();

    for (entry_idx, entry) in parsed.entries.into_iter().enumerate() {
        if entry.content_lines.is_empty() {
            continue;
        }
        let sid_str = entry.source_id_raw.trim();
        let redacted_raw = redactor.redact(&entry.raw_block);
        if redacted_raw.trim().is_empty() {
            continue;
        }

        if let Some(info) = lookup_source_render_info_tx(tx, graph, sid_str, expected_viewer)? {
            if info.live_scope_allows || existed_sources_at_base.contains(sid_str) {
                let unchanged = entry.content_lines.len() == 1
                    && (entry.content_lines[0] == info.rendered_bullet
                        || is_same_source_bullet_ignoring_container_rename(
                            &entry.content_lines[0],
                            &info,
                            sid_str,
                        ));
                if unchanged {
                    continue;
                }
                let src_pol = match sid_str.parse::<SourceId>() {
                    Ok(sid) => Some(source_effective_route_policy_in_tx(tx, &sid)?),
                    Err(_) => None,
                };
                let eff_pol = merge_route_policy(src_pol.as_ref(), default_policy);
                out.push(FreeformCandidate {
                    source_file: "sources.md".to_owned(),
                    ordinal: entry_idx,
                    raw_markdown: redacted_raw,
                    route_policy: Some(eff_pol),
                });
                continue;
            }
        }

        // Unknown or never-readable `source_id`: do not inspect source policy (zero side-channel).
        out.push(FreeformCandidate {
            source_file: "sources.md".to_owned(),
            ordinal: entry_idx,
            raw_markdown: redacted_raw,
            route_policy: Some(default_policy.clone()),
        });
    }

    for (fb_idx, block) in parsed.freeform_blocks.into_iter().enumerate() {
        let redacted = redactor.redact(&block);
        if !redacted.trim().is_empty() {
            out.push(FreeformCandidate {
                source_file: "sources.md".to_owned(),
                ordinal: 10_000 + fb_idx,
                raw_markdown: redacted,
                route_policy: Some(default_policy.clone()),
            });
        }
    }

    Ok(out)
}

pub(super) fn build_updated_fact_object(
    tx: &Connection,
    old_fact: &Fact,
    redacted_claim: &str,
    orig_policy: &ModelRoutePolicy,
) -> Result<Option<Value>> {
    if matches!(old_fact.kind, FactKind::Membership | FactKind::SkillVersion) {
        return Ok(None);
    }

    let normalized_for_slot = if !matches!(
        old_fact.kind,
        FactKind::Constraint
            | FactKind::Decision
            | FactKind::Event
            | FactKind::Observation
            | FactKind::Membership
    ) {
        let clean_subj =
            sanitize_md_inline(&strip_turn_tags_and_assistant_lines(&old_fact.subject));
        let prefix = format!("{clean_subj}: ");
        if !clean_subj.is_empty() {
            if let Some(stripped) = redacted_claim.strip_prefix(&prefix) {
                let trimmed = stripped.trim();
                if !trimmed.is_empty() {
                    trimmed
                } else {
                    redacted_claim
                }
            } else {
                redacted_claim
            }
        } else {
            redacted_claim
        }
    } else {
        redacted_claim
    };

    let candidate_obj = match &old_fact.object {
        Value::Object(old_map) => {
            let mut map = old_map.clone();
            let text_keys = [
                "summary",
                "claim_text",
                "advice",
                "claim",
                "reason",
                "rule",
                "message",
                "text",
                "description",
            ];
            let mut updated_any = false;
            for key in text_keys {
                if map.get(key).and_then(Value::as_str).is_some() {
                    map.insert(
                        key.to_owned(),
                        Value::String(normalized_for_slot.to_owned()),
                    );
                    updated_any = true;
                }
            }
            if !updated_any {
                map.insert(
                    "claim_text".to_owned(),
                    Value::String(normalized_for_slot.to_owned()),
                );
            }
            if map.contains_key("event") && map.contains_key("constraint") && !updated_any {
                map.insert(
                    "summary".to_owned(),
                    Value::String(normalized_for_slot.to_owned()),
                );
            }
            if !matches!(orig_policy, ModelRoutePolicy::HostCli) || map.contains_key("route_policy")
            {
                map.insert(
                    "route_policy".to_owned(),
                    serde_json::to_value(orig_policy)?,
                );
            }
            Value::Object(map)
        }
        Value::String(_) => Value::String(normalized_for_slot.to_owned()),
        _ => {
            if let Ok(parsed_val) = serde_json::from_str::<Value>(normalized_for_slot) {
                parsed_val
            } else {
                Value::String(normalized_for_slot.to_owned())
            }
        }
    };

    if let Some(slot) = read_slot(tx, &old_fact.predicate)? {
        if validate_value(&slot, &candidate_obj).is_err() {
            return Ok(None);
        }
    }
    Ok(Some(candidate_obj))
}
