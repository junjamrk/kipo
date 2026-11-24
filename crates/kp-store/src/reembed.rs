//! Batch vector migration and stale vector recomputation (`kp embed --stale`).

use kp_model::{Fact, KpError};
use rusqlite::{params, Connection, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::embedding::{
    encode_vector_le_bytes, guard_and_lock_tx_embedding_dims, sanitize_and_validate_embedding,
    validate_vector,
};
use crate::facts::{fact_default_passage, is_fact_raw_only, is_source_marked_raw_only, read_fact};
use crate::tombstone::tombstone_id;
use crate::{Embedder, Result, Store, StoreError};

/// Summary returned by [`Store::reembed_stale`] (`kp embed --stale [--to <model> --dim <N>]`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReembedReport {
    pub model_id: String,
    pub dims: usize,
    pub chunks_reembedded: usize,
    pub facts_reembedded: usize,
    pub chunks_up_to_date: usize,
    pub facts_up_to_date: usize,
    pub skipped_raw_only: usize,
}

struct StaleChunkItem {
    chunk_id: String,
    text: String,
}

struct StaleFactItem {
    fact_id: String,
    text: String,
}

fn current_migration_seq(db: &Connection) -> Result<i64> {
    Ok(db.query_row(
        "SELECT COALESCE(MAX(seq), 0) FROM change_log WHERE op = 'embedding_migration'",
        [],
        |r| r.get(0),
    )?)
}

fn verify_reembed_generation_tx(
    tx: &Connection,
    expected_model: &str,
    expected_dims: usize,
    expected_migration_seq: i64,
) -> Result<()> {
    let cur_seq = current_migration_seq(tx)?;
    if cur_seq != expected_migration_seq {
        return Err(StoreError::Conflict);
    }
    let (cur_model, cur_dims_i64): (String, i64) = tx.query_row(
        "SELECT embedding_model, embedding_dims FROM library_meta LIMIT 1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let cur_dims = usize::try_from(cur_dims_i64)
        .map_err(|_| KpError::invalid("embedding_dims", "negative stored embedding_dims"))?;
    if cur_model != expected_model || cur_dims != expected_dims {
        return Err(StoreError::Conflict);
    }
    Ok(())
}

impl Store {
    /// Recomputes missing or stale (`model_id` / `dims` mismatch) rows in `chunk_vectors` and
    /// `fact_vectors`, optionally migrating `library_meta` to `(target_model, target_dims)` first.
    ///
    /// When `embedder` is an external/cloud service (`!embedder.allows_raw_only()`), any chunk or
    /// fact anchored on a `RawOnly` source is never transmitted externally and is counted in
    /// `skipped_raw_only`.
    pub fn reembed_stale(
        &mut self,
        embedder: &dyn Embedder,
        migrate_to: Option<(&str, usize)>,
    ) -> Result<ReembedReport> {
        if let Some((raw_target_model, target_dims)) = migrate_to {
            let target_model = raw_target_model.trim();
            if target_model.is_empty() {
                return Err(KpError::invalid(
                    "embedding_model",
                    "target embedding model must be nonempty",
                )
                .into());
            }
            if target_model.eq_ignore_ascii_case("none") {
                if target_dims != 0 {
                    return Err(
                        KpError::invalid("embedding_dims", "`none` requires 0 dimensions").into(),
                    );
                }
                let tx = self
                    .db
                    .transaction_with_behavior(TransactionBehavior::Immediate)?;
                tx.execute(
                    "UPDATE library_meta SET embedding_model = 'none', embedding_dims = 0",
                    [],
                )?;
                tx.execute("DELETE FROM chunk_vectors", [])?;
                tx.execute("DELETE FROM fact_vectors", [])?;
                tx.execute(
                    "INSERT INTO change_log (op, target_id, payload_json, created_at)
                     VALUES ('embedding_migration', 'library_meta', ?1, 0)",
                    [serde_json::to_string(
                        &json!({ "model": "none", "dims": 0 }),
                    )?],
                )?;
                tx.commit()?;
                return Ok(ReembedReport {
                    model_id: "none".to_owned(),
                    dims: 0,
                    ..ReembedReport::default()
                });
            }
            if !embedder.is_enabled() {
                return Err(StoreError::Embedding(format!(
                    "cannot migrate to `{target_model}` because the target embedder is not ready"
                )));
            }
            if embedder.model_id() != "custom" && embedder.model_id() != target_model {
                return Err(StoreError::Embedding(format!(
                    "embedder model `{}` does not match requested target `{target_model}`",
                    embedder.model_id()
                )));
            }
            let dims_i64 = i64::try_from(target_dims)
                .map_err(|_| KpError::invalid("embedding_dims", "dimension overflow"))?;
            let tx = self
                .db
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute(
                "UPDATE library_meta SET embedding_model = ?1, embedding_dims = ?2",
                params![target_model, dims_i64],
            )?;
            if target_dims > 0 {
                tx.execute(
                    "DELETE FROM chunk_vectors WHERE dims != ?1 OR model_id != ?2",
                    params![dims_i64, target_model],
                )?;
                tx.execute(
                    "DELETE FROM fact_vectors WHERE dims != ?1 OR model_id != ?2",
                    params![dims_i64, target_model],
                )?;
            } else {
                tx.execute(
                    "DELETE FROM chunk_vectors WHERE model_id != ?1",
                    [target_model],
                )?;
                tx.execute(
                    "DELETE FROM fact_vectors WHERE model_id != ?1",
                    [target_model],
                )?;
            }
            tx.execute(
                "INSERT INTO change_log (op, target_id, payload_json, created_at)
                 VALUES ('embedding_migration', 'library_meta', ?1, 0)",
                [serde_json::to_string(&json!({
                    "model": target_model,
                    "dims": target_dims,
                }))?],
            )?;
            tx.commit()?;
        }

        let (meta_model, mut snapshot_dims) = self.embedding_config()?;
        let mut snapshot_migration_seq = current_migration_seq(&self.db)?;
        if meta_model == "none" || !embedder.is_enabled() {
            return Ok(ReembedReport {
                model_id: meta_model,
                dims: snapshot_dims,
                ..ReembedReport::default()
            });
        }

        if embedder.model_id() != "custom" && embedder.model_id() != meta_model {
            let hint_dims = embedder.expected_dims().unwrap_or(snapshot_dims);
            return Err(StoreError::Embedding(format!(
                "embedding model mismatch: library locks `{meta_model}` ({snapshot_dims}d), \
                 but active embedder is `{}`; \
                 run `kp embed --stale --to {} --dim {hint_dims}`",
                embedder.model_id(),
                embedder.model_id()
            )));
        }

        let target_model = if embedder.model_id() == "custom" {
            meta_model.clone()
        } else {
            embedder.model_id().to_owned()
        };
        let is_real_model = target_model != "hash:v1" && embedder.model_id() != "custom";
        let expected_dims = embedder
            .expected_dims()
            .filter(|&d| d > 0)
            .or_else(|| (snapshot_dims > 0).then_some(snapshot_dims));

        let mut report = ReembedReport {
            model_id: target_model.clone(),
            dims: snapshot_dims,
            ..ReembedReport::default()
        };

        let mut stale_chunks = Vec::<StaleChunkItem>::new();
        let mut raw_only_chunk_deletes = Vec::<String>::new();
        {
            let mut stmt = self.db.prepare(
                "SELECT c.chunk_id, c.source_id, c.heading, c.body, v.dims, v.model_id
                 FROM chunks c
                 JOIN raw_sources s ON s.source_id = c.source_id AND s.content_sha256 = c.source_sha256
                 LEFT JOIN chunk_vectors v ON v.chunk_id = c.chunk_id
                 WHERE s.superseded_by_source IS NULL
                   AND NOT EXISTS (SELECT 1 FROM tombstones t WHERE t.tombstone_id = c.source_id)
                 ORDER BY c.chunk_id",
            )?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let chunk_id: String = row.get(0)?;
                let source_id: String = row.get(1)?;
                let heading: String = row.get(2)?;
                let body: String = row.get(3)?;
                let v_dims: Option<usize> = row.get(4)?;
                let v_model: Option<String> = row.get(5)?;

                if !embedder.allows_raw_only() && is_source_marked_raw_only(&self.db, &source_id)? {
                    if v_dims.is_some() {
                        raw_only_chunk_deletes.push(chunk_id);
                    }
                    report.skipped_raw_only += 1;
                    continue;
                }
                let up_to_date = match (v_dims, v_model.as_deref()) {
                    (Some(d), Some(m)) => {
                        m == target_model && expected_dims.is_none_or(|exp| exp == d)
                    }
                    _ => false,
                };
                if up_to_date {
                    report.chunks_up_to_date += 1;
                    continue;
                }
                let combined = format!("{heading}\n{body}");
                if is_real_model && combined.trim().is_empty() {
                    continue;
                }
                stale_chunks.push(StaleChunkItem {
                    chunk_id,
                    text: combined,
                });
            }
        }

        let mut stale_facts = Vec::<StaleFactItem>::new();
        let mut raw_only_fact_deletes = Vec::<String>::new();
        {
            let tombstones: std::collections::BTreeSet<String> = {
                let mut t_stmt = self.db.prepare("SELECT tombstone_id FROM tombstones")?;
                let rows = t_stmt.query_map([], |r| r.get::<_, String>(0))?;
                rows.collect::<std::result::Result<_, _>>()?
            };
            let all_facts: Vec<Fact> = {
                let mut f_stmt = self.db.prepare(
                    "SELECT * FROM facts
                     WHERE status IN ('active', 'superseded') AND expired_at IS NULL
                     ORDER BY fact_id",
                )?;
                let mut rows = f_stmt.query([])?;
                let mut out = Vec::new();
                while let Some(row) = rows.next()? {
                    let fact = read_fact(row)?;
                    let is_tomb = if tombstones.is_empty() {
                        false
                    } else {
                        let opaque = tombstone_id(
                            fact.origin_library.as_str().as_bytes(),
                            &fact.origin_library,
                            &fact.logical_key,
                        )?;
                        tombstones.contains(opaque.as_str())
                    };
                    if !is_tomb {
                        out.push(fact);
                    }
                }
                out
            };

            let mut fv_stmt = self
                .db
                .prepare("SELECT dims, model_id FROM fact_vectors WHERE fact_id = ?1")?;
            for fact in all_facts {
                let existing: Option<(usize, String)> = rusqlite::OptionalExtension::optional(
                    fv_stmt.query_row([fact.id.as_str()], |r| Ok((r.get(0)?, r.get(1)?))),
                )?;
                if !embedder.allows_raw_only() && is_fact_raw_only(&self.db, &fact)? {
                    if existing.is_some() {
                        raw_only_fact_deletes.push(fact.id.to_string());
                    }
                    report.skipped_raw_only += 1;
                    continue;
                }
                let up_to_date = match &existing {
                    Some((d, m)) => m == &target_model && expected_dims.is_none_or(|exp| exp == *d),
                    None => false,
                };
                if up_to_date {
                    report.facts_up_to_date += 1;
                    continue;
                }
                let passage = fact_default_passage(&fact);
                if is_real_model && passage.trim().is_empty() {
                    continue;
                }
                stale_facts.push(StaleFactItem {
                    fact_id: fact.id.to_string(),
                    text: passage,
                });
            }
        }

        if !raw_only_chunk_deletes.is_empty() || !raw_only_fact_deletes.is_empty() {
            let tx = self
                .db
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            verify_reembed_generation_tx(&tx, &meta_model, snapshot_dims, snapshot_migration_seq)?;
            for cid in &raw_only_chunk_deletes {
                tx.execute("DELETE FROM chunk_vectors WHERE chunk_id = ?1", [cid])?;
            }
            for fid in &raw_only_fact_deletes {
                tx.execute("DELETE FROM fact_vectors WHERE fact_id = ?1", [fid])?;
            }
            tx.commit()?;
        }

        let guard_model_arg = if embedder.model_id() == "custom" {
            "custom"
        } else {
            target_model.as_str()
        };

        const BATCH_SIZE: usize = 64;
        for batch in stale_chunks.chunks(BATCH_SIZE) {
            let texts: Vec<String> = batch.iter().map(|it| it.text.clone()).collect();
            let raw_vecs = embedder.embed_batch(&texts)?;
            if raw_vecs.len() != batch.len() {
                return Err(KpError::invalid(
                    "embedding",
                    "embed_batch returned mismatched vector count during chunk re-embed",
                )
                .into());
            }
            let validated_vecs: Vec<Vec<f32>> = raw_vecs
                .into_iter()
                .map(|v| {
                    if is_real_model {
                        sanitize_and_validate_embedding(&v)
                    } else {
                        validate_vector(&v)?;
                        Ok(v)
                    }
                })
                .collect::<Result<Vec<_>>>()?;

            let tx = self
                .db
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            verify_reembed_generation_tx(&tx, &meta_model, snapshot_dims, snapshot_migration_seq)?;
            for (item, vec) in batch.iter().zip(validated_vecs) {
                let was_unlocked = snapshot_dims == 0 && guard_model_arg != "custom";
                guard_and_lock_tx_embedding_dims(&tx, guard_model_arg, vec.len())?;
                if was_unlocked {
                    snapshot_dims = vec.len();
                    snapshot_migration_seq = current_migration_seq(&tx)?;
                }
                let blob = encode_vector_le_bytes(&vec);
                tx.execute(
                    "INSERT INTO chunk_vectors (chunk_id, dims, vec_blob, model_id)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(chunk_id) DO UPDATE SET
                        dims = excluded.dims,
                        vec_blob = excluded.vec_blob,
                        model_id = excluded.model_id",
                    params![&item.chunk_id, vec.len(), blob, &target_model],
                )?;
                report.chunks_reembedded += 1;
            }
            tx.commit()?;
        }

        for batch in stale_facts.chunks(BATCH_SIZE) {
            let texts: Vec<String> = batch.iter().map(|it| it.text.clone()).collect();
            let raw_vecs = embedder.embed_batch(&texts)?;
            if raw_vecs.len() != batch.len() {
                return Err(KpError::invalid(
                    "embedding",
                    "embed_batch returned mismatched vector count during fact re-embed",
                )
                .into());
            }
            let validated_vecs: Vec<Vec<f32>> = raw_vecs
                .into_iter()
                .map(|v| {
                    if is_real_model {
                        sanitize_and_validate_embedding(&v)
                    } else {
                        validate_vector(&v)?;
                        Ok(v)
                    }
                })
                .collect::<Result<Vec<_>>>()?;

            let tx = self
                .db
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            verify_reembed_generation_tx(&tx, &meta_model, snapshot_dims, snapshot_migration_seq)?;
            for (item, vec) in batch.iter().zip(validated_vecs) {
                let was_unlocked = snapshot_dims == 0 && guard_model_arg != "custom";
                guard_and_lock_tx_embedding_dims(&tx, guard_model_arg, vec.len())?;
                if was_unlocked {
                    snapshot_dims = vec.len();
                    snapshot_migration_seq = current_migration_seq(&tx)?;
                }
                let blob = encode_vector_le_bytes(&vec);
                tx.execute(
                    "INSERT INTO fact_vectors (fact_id, dims, vec_blob, model_id)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(fact_id) DO UPDATE SET
                        dims = excluded.dims,
                        vec_blob = excluded.vec_blob,
                        model_id = excluded.model_id",
                    params![&item.fact_id, vec.len(), blob, &target_model],
                )?;
                report.facts_reembedded += 1;
            }
            tx.commit()?;
        }

        let (final_model, final_dims) = self.embedding_config()?;
        report.model_id = final_model;
        report.dims = final_dims;
        Ok(report)
    }
}
