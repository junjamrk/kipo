use kp_model::{ActorId, AnchorSpec, ContentHash, EpochMs, FactId, VerifyStatus};
use rusqlite::{params, TransactionBehavior};
use serde::{Deserialize, Serialize};

use crate::{FactInput, Remembered, Result, Store};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CodeLocus {
    pub repo_root: String,
    pub file_path: String,
    pub language: String,
    pub symbol_name: String,
    pub sig_hash: ContentHash,
    pub body_hash: ContentHash,
    pub start_line: u32,
    pub end_line: u32,
}

impl CodeLocus {
    pub fn anchor(&self) -> AnchorSpec {
        AnchorSpec {
            file_path: Some(self.file_path.clone()),
            symbol: Some(self.symbol_name.clone()),
            sig_hash: Some(self.sig_hash.clone()),
            body_hash: Some(self.body_hash.clone()),
            start_line: Some(self.start_line),
            end_line: Some(self.end_line),
            ..AnchorSpec::default()
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct CodeAnchor {
    pub anchor_id: String,
    pub fact_id: FactId,
    #[serde(flatten)]
    pub locus: CodeLocus,
    pub verify_status: VerifyStatus,
    pub verified_at: EpochMs,
}

impl Store {
    /// The fact and its original signature become durable together.
    pub fn remember_anchored(
        &mut self,
        mut input: FactInput,
        locus: &CodeLocus,
    ) -> Result<Remembered> {
        input.fact.source_anchor = Some(locus.anchor());
        let inputs = [input];
        let fact = &inputs[0].fact;
        let embedder = self.embedder.clone();
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result =
            crate::slots::remember_facts_indexed(&tx, &inputs, &self.tokenizer, embedder.as_ref())?;
        tx.execute("INSERT INTO code_anchors (anchor_id,fact_id,repo_root,file_path,language,symbol_name,sig_hash,body_hash,start_line,end_line,verify_status,verified_at)
            VALUES (?1,?1,?2,?3,?4,?5,?6,?7,?8,?9,'verified',?10)", params![
            fact.id.as_str(), locus.repo_root, locus.file_path, locus.language, locus.symbol_name,
            locus.sig_hash.as_str(), locus.body_hash.as_str(), locus.start_line, locus.end_line, fact.access.recorded_at().0])?;
        tx.commit()?;
        self.register_entities(&inputs);
        Ok(result)
    }

    /// Resolve visibility before exposing paths or touching the repository filesystem.
    pub fn code_anchors(&self, viewer: &ActorId, at: EpochMs) -> Result<Vec<CodeAnchor>> {
        let raw_rows = {
            let mut stmt = self.db.prepare(
                "SELECT anchor_id,fact_id,repo_root,file_path,language,symbol_name,sig_hash,body_hash,start_line,end_line,verify_status,verified_at
                 FROM code_anchors WHERE repo_root IS NOT NULL ORDER BY anchor_id",
            )?;
            let mut rows = stmt.query([])?;
            let mut list = Vec::new();
            while let Some(row) = rows.next()? {
                list.push(CodeAnchor {
                    anchor_id: row.get(0)?,
                    fact_id: row.get::<_, String>(1)?.parse()?,
                    locus: CodeLocus {
                        repo_root: row.get(2)?,
                        file_path: row.get(3)?,
                        language: row.get(4)?,
                        symbol_name: row.get(5)?,
                        sig_hash: row.get::<_, String>(6)?.parse()?,
                        body_hash: row.get::<_, String>(7)?.parse()?,
                        start_line: row.get(8)?,
                        end_line: row.get(9)?,
                    },
                    verify_status: row.get::<_, String>(10)?.parse()?,
                    verified_at: EpochMs(row.get(11)?),
                });
            }
            list
        };
        if raw_rows.is_empty() {
            return Ok(Vec::new());
        }
        let candidate_ids: Vec<FactId> = raw_rows.iter().map(|a| a.fact_id.clone()).collect();
        let visible = self.filter_visible_fact_ids(viewer, at, &candidate_ids)?;
        Ok(raw_rows
            .into_iter()
            .filter(|a| visible.contains(&a.fact_id))
            .collect())
    }

    /// Hashes remain the original contract; observing a change must not silently accept it.
    pub fn update_code_anchors(&mut self, anchors: &[CodeAnchor]) -> Result<()> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for anchor in anchors {
            tx.execute("UPDATE code_anchors SET verify_status=?1,verified_at=?2,start_line=?3,end_line=?4 WHERE anchor_id=?5", params![
                anchor.verify_status.as_str(), anchor.verified_at.0, anchor.locus.start_line, anchor.locus.end_line, anchor.anchor_id])?;
            tx.execute("UPDATE facts SET source_anchor_json=json_set(source_anchor_json,'$.start_line',?1,'$.end_line',?2) WHERE fact_id=?3", params![anchor.locus.start_line,anchor.locus.end_line,anchor.fact_id.as_str()])?;
        }
        tx.commit()?;
        Ok(())
    }
}
