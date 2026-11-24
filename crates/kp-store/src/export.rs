//! Viewer-scoped deterministic Markdown directory projection (`kp export`) with `projection_pages`
//! baseline tracking for M3 lossless writeback (`kp apply`).

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use kp_model::{
    ActorId, ContentHash, EpochMs, Fact, FactAccess, FactId, FactKind, FactStatus, LibraryId,
    ProjectionPageMeta,
};
use rusqlite::{params, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};

use crate::facts::read_graph;
use crate::snapshot::{
    lookup_scope_label, render_fact_claim_text, render_premise_human_quote,
    resolve_fact_source_label, strip_turn_tags_and_assistant_lines,
};
use crate::{Result, Store, StoreError};

/// Summary returned by [`Store::export_markdown_directory`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportReport {
    pub library_id: LibraryId,
    pub viewer: ActorId,
    pub base_seq: i64,
    pub exported_at: EpochMs,
    pub pages: Vec<ProjectionPageMeta>,
    pub visible_facts: usize,
    pub visible_sources: usize,
}

pub(crate) struct ExportDirLock {
    lock_path: PathBuf,
}

impl ExportDirLock {
    pub(crate) fn acquire(out_dir: &Path) -> Result<Self> {
        let lock_path = out_dir.join(".kp-export.lock");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock_path)
            {
                Ok(mut f) => {
                    let _ = writeln!(f, "pid={}", std::process::id());
                    let _ = f.sync_all();
                    return Ok(Self { lock_path });
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    if Instant::now() >= deadline {
                        return Err(StoreError::Conflict);
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(err) => return Err(err.into()),
            }
        }
    }
}

impl Drop for ExportDirLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.lock_path);
    }
}

fn restore_persisted_pages(out_dir: &Path, backups: &[(PathBuf, Option<Vec<u8>>)]) {
    for (file_path, prev_bytes) in backups {
        match prev_bytes {
            Some(bytes) => {
                if let Ok(mut tmp) = tempfile::Builder::new()
                    .prefix(".kp-restore.")
                    .suffix(".tmp")
                    .tempfile_in(out_dir)
                {
                    if tmp.write_all(bytes).is_ok() && tmp.as_file().sync_all().is_ok() {
                        let _ = tmp.persist(file_path);
                    }
                }
            }
            None => {
                let _ = fs::remove_file(file_path);
            }
        }
    }
}

struct ReadableSourceRow {
    source_id: String,
    uri: String,
    media_type: String,
    captured_at: i64,
    scope_label: Option<String>,
    chunk_count: usize,
    summary_excerpt: String,
}

pub(crate) fn format_kp_ref(fact_id: &FactId) -> String {
    let raw = fact_id.as_str();
    if raw.starts_with("fact_") {
        raw.to_owned()
    } else {
        format!("fact_{raw}")
    }
}

pub(crate) fn sanitize_md_inline(raw: &str) -> String {
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(crate) fn compute_source_summary_excerpt(first_heading: &str, first_body: &str) -> String {
    let clean_body = sanitize_md_inline(&strip_turn_tags_and_assistant_lines(first_body));
    let clean_heading = sanitize_md_inline(&strip_turn_tags_and_assistant_lines(first_heading));
    let combined = if clean_heading.is_empty() {
        clean_body
    } else if clean_body.is_empty() {
        clean_heading
    } else {
        format!("{clean_heading}: {clean_body}")
    };
    let chars: Vec<char> = combined.chars().collect();
    if chars.len() > 160 {
        format!("{}…", chars[..159].iter().collect::<String>().trim_end())
    } else {
        combined
    }
}

pub(crate) fn format_source_markdown_bullet(
    uri: &str,
    source_id: &str,
    media_type: &str,
    chunk_count: usize,
    captured_at: i64,
    scope_label: Option<&str>,
    summary_excerpt: &str,
) -> String {
    let container_suffix = scope_label
        .filter(|s| !s.trim().is_empty())
        .map(|lbl| format!(" · container `{}`", sanitize_md_inline(lbl)))
        .unwrap_or_default();
    format!(
        "- **`{}`** (`{}` · `{}` · {} chunk(s) · captured `{}`{container_suffix}): {}",
        sanitize_md_inline(uri),
        source_id,
        sanitize_md_inline(media_type),
        chunk_count,
        captured_at,
        summary_excerpt
    )
}

fn render_frontmatter(
    page_slug: &str,
    library_id: &LibraryId,
    viewer: &ActorId,
    base_seq: i64,
    exported_at: EpochMs,
) -> String {
    format!(
        "---\n\
         page_slug: {page_slug}\n\
         library_id: {library_id}\n\
         viewer: {viewer}\n\
         base_seq: {base_seq}\n\
         exported_at: {}\n\
         ---\n\n",
        exported_at.0
    )
}

impl Store {
    /// Reads a recorded [`ProjectionPageMeta`] baseline from `projection_pages`.
    pub fn projection_page(
        &self,
        page_slug: &str,
        viewer: &ActorId,
    ) -> Result<Option<ProjectionPageMeta>> {
        self.db
            .query_row(
                "SELECT page_slug, viewer_actor, base_seq, page_sha256, rendered_at
                 FROM projection_pages
                 WHERE page_slug = ?1 AND viewer_actor = ?2",
                params![page_slug.trim(), viewer.as_str()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                },
            )
            .optional()?
            .map(|(slug, actor_str, base_seq, sha_str, rendered_at)| {
                Ok(ProjectionPageMeta {
                    page_slug: slug,
                    viewer: actor_str.parse()?,
                    base_seq,
                    page_sha256: sha_str.parse()?,
                    rendered_at: EpochMs(rendered_at),
                })
            })
            .transpose()
    }

    /// Exports viewer-visible active facts, rules, decisions, derived warnings (with verbatim
    /// quotes and `kp-ref:`), and readable source summaries into deterministic Markdown files in
    /// `out_dir`, recording `(page_slug, viewer_actor, base_seq, page_sha256, rendered_at)` in
    /// `projection_pages`.
    pub fn export_markdown_directory(
        &mut self,
        out_dir: &Path,
        viewer: &ActorId,
        at: EpochMs,
    ) -> Result<ExportReport> {
        let pre_active: bool = self.db.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM members_tokens WHERE actor_id = ?1 AND revoked_at IS NULL
             )",
            [viewer.as_str()],
            |row| row.get(0),
        )?;
        if !pre_active {
            return Err(StoreError::PermissionDenied);
        }

        fs::create_dir_all(out_dir)?;
        let _dir_lock = ExportDirLock::acquire(out_dir)?;

        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let is_active_member: bool = tx.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM members_tokens WHERE actor_id = ?1 AND revoked_at IS NULL
             )",
            [viewer.as_str()],
            |row| row.get(0),
        )?;
        if !is_active_member {
            return Err(StoreError::PermissionDenied);
        }

        let base_seq: i64 =
            tx.query_row("SELECT COALESCE(MAX(seq), 0) FROM change_log", [], |row| {
                row.get(0)
            })?;

        let mut graph = read_graph(&tx)?;
        let historical: BTreeSet<_> = graph
            .facts
            .values()
            .filter(|fact| {
                fact.status == FactStatus::Superseded
                    && fact.access.expired_at().is_none()
                    && matches!(&fact.access, FactAccess::Base { stamp, .. } if stamp.valid.contains(at))
                    && fact
                        .superseded_by
                        .as_ref()
                        .and_then(|id| graph.facts.get(id))
                        .is_some_and(|next| {
                            matches!(&next.access, FactAccess::Base { stamp, .. } if at < stamp.valid.valid_from)
                        })
            })
            .map(|f| f.id.clone())
            .collect();
        for id in historical {
            if let Some(fact) = graph.facts.get_mut(&id) {
                fact.status = FactStatus::Active;
            }
        }

        let evaluated = graph.evaluate(viewer, at);
        let mut visible_facts: Vec<&Fact> = evaluated
            .facts
            .iter()
            .filter_map(|fid| graph.facts.get(fid))
            .collect();
        visible_facts.sort_by(|a, b| {
            a.subject
                .cmp(&b.subject)
                .then_with(|| a.predicate.cmp(&b.predicate))
                .then_with(|| a.id.cmp(&b.id))
        });

        let mut rules = Vec::new();
        let mut decisions = Vec::new();
        let mut warnings = Vec::new();
        let mut facts_and_events = Vec::new();

        for fact in &visible_facts {
            match fact.kind {
                FactKind::Constraint | FactKind::ClosedNegative => rules.push(*fact),
                FactKind::Decision => decisions.push(*fact),
                FactKind::CollisionWarning
                | FactKind::DerivedFact
                | FactKind::Synthesis
                | FactKind::Conflict => warnings.push(*fact),
                _ => facts_and_events.push(*fact),
            }
        }

        let mut index_md = render_frontmatter("index", &self.library_id, viewer, base_seq, at);
        let _ = writeln!(
            index_md,
            "# Kipo Projection (`{}` · viewer `{}`)\n",
            self.library_id, viewer
        );

        let render_fact_entry = |buf: &mut String, fact: &Fact| -> Result<()> {
            let claim = sanitize_md_inline(&render_fact_claim_text(&tx, fact)?);
            let source_label =
                sanitize_md_inline(&resolve_fact_source_label(&tx, &self.library_id, fact)?);
            let kp_ref = format_kp_ref(&fact.id);
            let quote = fact
                .source_anchor
                .as_ref()
                .and_then(|a| a.verbatim_quote.as_deref())
                .map(strip_turn_tags_and_assistant_lines)
                .map(|q| sanitize_md_inline(&q))
                .filter(|q| !q.is_empty());

            let _ = writeln!(buf, "<!-- kp-entry: {} -->", fact.id);
            let _ = writeln!(
                buf,
                "- **[kp-ref:{kp_ref}]** (`{}` · `{}` @ `{source_label}`) {claim}",
                fact.kind.as_str(),
                fact.author
            );
            if let Some(q) = quote {
                if !claim.contains(&q) {
                    let _ = writeln!(buf, "  - Quote: \"{q}\"");
                }
            }

            if let Some(just_ids) = evaluated.justifications.get(&fact.id) {
                let mut prem_ids = BTreeSet::<FactId>::new();
                for jid in just_ids {
                    if let Some(just) = graph.justifications.get(jid) {
                        for prem in &just.premises {
                            if evaluated.facts.contains(&prem.fact_id) {
                                prem_ids.insert(prem.fact_id.clone());
                            }
                        }
                    }
                }
                for pid in prem_ids {
                    if let Some(pfact) = graph.facts.get(&pid) {
                        let p_quote = sanitize_md_inline(&render_premise_human_quote(&tx, pfact)?);
                        let p_src = sanitize_md_inline(&resolve_fact_source_label(
                            &tx,
                            &self.library_id,
                            pfact,
                        )?);
                        let p_ref = format_kp_ref(&pfact.id);
                        let _ = writeln!(
                            buf,
                            "  - Premise `[kp-ref:{p_ref}]` (`{}` @ `{p_src}`): {p_quote}",
                            pfact.author
                        );
                    }
                }
            }
            Ok(())
        };

        let _ = writeln!(index_md, "## Rules & Constraints\n");
        if rules.is_empty() {
            let _ = writeln!(index_md, "_None._\n");
        } else {
            for fact in &rules {
                render_fact_entry(&mut index_md, fact)?;
            }
            index_md.push('\n');
        }

        let _ = writeln!(index_md, "## Decisions\n");
        if decisions.is_empty() {
            let _ = writeln!(index_md, "_None._\n");
        } else {
            for fact in &decisions {
                render_fact_entry(&mut index_md, fact)?;
            }
            index_md.push('\n');
        }

        let _ = writeln!(index_md, "## Warnings & Derived Insights\n");
        if warnings.is_empty() {
            let _ = writeln!(index_md, "_None._\n");
        } else {
            for fact in &warnings {
                render_fact_entry(&mut index_md, fact)?;
            }
            index_md.push('\n');
        }

        let _ = writeln!(index_md, "## Facts & Events\n");
        if facts_and_events.is_empty() {
            let _ = writeln!(index_md, "_None._\n");
        } else {
            for fact in &facts_and_events {
                render_fact_entry(&mut index_md, fact)?;
            }
            index_md.push('\n');
        }

        let readable_sources = {
            let mut stmt = tx.prepare(
                "SELECT s.source_id, s.uri, s.media_type, s.captured_at, s.source_scope_id
                 FROM raw_sources s
                 WHERE s.superseded_by_source IS NULL
                   AND NOT EXISTS (SELECT 1 FROM tombstones t WHERE t.tombstone_id = s.source_id)
                   AND (s.source_scope_id IS NULL OR EXISTS (
                       SELECT 1 FROM source_scopes sc, json_each(sc.current_audience_json) sa
                       WHERE sc.scope_id = s.source_scope_id
                         AND (sa.value = ?1
                              OR (sa.value = '*' AND EXISTS (
                                  SELECT 1 FROM members_tokens m WHERE m.actor_id = ?1 AND m.revoked_at IS NULL
                              )))
                   ))
                 ORDER BY s.captured_at ASC, s.source_id ASC",
            )?;
            let mut chunk_stmt = tx.prepare(
                "SELECT c.heading, c.body
                 FROM chunks c
                 WHERE c.source_id = ?1
                   AND EXISTS (
                       SELECT 1 FROM json_each(c.readable_by_json) a
                       WHERE a.value = ?2
                          OR (a.value = '*' AND EXISTS (
                              SELECT 1 FROM members_tokens m WHERE m.actor_id = ?2 AND m.revoked_at IS NULL
                          ))
                   )
                 ORDER BY c.ordinal ASC",
            )?;
            let source_rows: Vec<(String, String, String, i64, Option<String>)> = stmt
                .query_map([viewer.as_str()], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                })?
                .collect::<std::result::Result<_, _>>()?;

            let mut collected = Vec::new();
            for (source_id, uri, media_type, captured_at, scope_id) in source_rows {
                let visible_chunks: Vec<(String, String)> = chunk_stmt
                    .query_map(params![&source_id, viewer.as_str()], |r| {
                        Ok((r.get(0)?, r.get(1)?))
                    })?
                    .collect::<std::result::Result<_, _>>()?;
                if visible_chunks.is_empty() {
                    continue;
                }
                let scope_label = match scope_id {
                    Some(ref sid) => lookup_scope_label(&tx, sid)?,
                    None => None,
                };
                let (first_heading, first_body) = &visible_chunks[0];
                let summary_excerpt = compute_source_summary_excerpt(first_heading, first_body);
                collected.push(ReadableSourceRow {
                    source_id,
                    uri,
                    media_type,
                    captured_at,
                    scope_label,
                    chunk_count: visible_chunks.len(),
                    summary_excerpt,
                });
            }
            collected
        };

        let mut sources_md = render_frontmatter("sources", &self.library_id, viewer, base_seq, at);
        let _ = writeln!(
            sources_md,
            "# Readable Sources (`{}` · viewer `{}`)\n",
            self.library_id, viewer
        );
        if readable_sources.is_empty() {
            let _ = writeln!(sources_md, "_None._\n");
        } else {
            for src in &readable_sources {
                let _ = writeln!(sources_md, "<!-- kp-source: {} -->", src.source_id);
                let _ = writeln!(
                    sources_md,
                    "{}",
                    format_source_markdown_bullet(
                        &src.uri,
                        &src.source_id,
                        &src.media_type,
                        src.chunk_count,
                        src.captured_at,
                        src.scope_label.as_deref(),
                        &src.summary_excerpt,
                    )
                );
            }
            sources_md.push('\n');
        }

        let rendered_pages = [("index", index_md), ("sources", sources_md)];
        // Phase 1: Read all existing page backups BEFORE creating or modifying any file on disk.
        let mut backups: Vec<(PathBuf, Option<Vec<u8>>)> = Vec::with_capacity(rendered_pages.len());
        for (slug, _) in &rendered_pages {
            let file_path = out_dir.join(format!("{slug}.md"));
            let prev_bytes = match fs::symlink_metadata(&file_path) {
                Ok(_) => Some(fs::read(&file_path)?),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
                Err(err) => return Err(err.into()),
            };
            backups.push((file_path, prev_bytes));
        }

        // Phase 2: Stage and atomically persist all pages; restore prior backups on any failure.
        for (persisted_count, ((slug, content), (file_path, _))) in
            rendered_pages.iter().zip(backups.iter()).enumerate()
        {
            let persist_res: Result<()> = (|| {
                let mut tmp = tempfile::Builder::new()
                    .prefix(&format!(".{slug}.md."))
                    .suffix(".tmp")
                    .tempfile_in(out_dir)?;
                tmp.write_all(content.as_bytes())?;
                tmp.as_file().sync_all()?;
                tmp.persist(file_path).map_err(|e| e.error)?;
                Ok(())
            })();
            if let Err(err) = persist_res {
                restore_persisted_pages(out_dir, &backups[..persisted_count]);
                return Err(err);
            }
        }
        if let Ok(dir_handle) = fs::File::open(out_dir) {
            let _ = dir_handle.sync_all();
        }

        // Phase 3: Commit `projection_pages`; restore ALL files if commit fails.
        let mut page_metas = Vec::with_capacity(rendered_pages.len());
        let db_res: Result<()> = (|| {
            for (slug, content) in &rendered_pages {
                let page_sha256 = ContentHash::digest(content.as_bytes());
                tx.execute(
                    "INSERT INTO projection_pages (page_slug, viewer_actor, base_seq, page_sha256, rendered_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(page_slug, viewer_actor) DO UPDATE SET
                        base_seq = excluded.base_seq,
                        page_sha256 = excluded.page_sha256,
                        rendered_at = excluded.rendered_at",
                    params![slug, viewer.as_str(), base_seq, page_sha256.as_str(), at.0],
                )?;
                page_metas.push(ProjectionPageMeta {
                    page_slug: (*slug).to_owned(),
                    viewer: viewer.clone(),
                    base_seq,
                    page_sha256,
                    rendered_at: at,
                });
            }
            tx.commit()?;
            Ok(())
        })();
        if let Err(err) = db_res {
            restore_persisted_pages(out_dir, &backups);
            return Err(err);
        }

        Ok(ExportReport {
            library_id: self.library_id.clone(),
            viewer: viewer.clone(),
            base_seq,
            exported_at: at,
            pages: page_metas,
            visible_facts: visible_facts.len(),
            visible_sources: readable_sources.len(),
        })
    }
}
