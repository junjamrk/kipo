use rusqlite::{params, Connection};

use crate::{Result, RetrievalSnapshot, Tokenizer};

/// A transient contentless FTS5 index whose BM25 corpus is exactly the authorized slice.
/// SQL WHERE filters on the persistent index would still leak its global corpus statistics.
pub struct ScopedIndex {
    db: Connection,
}

impl ScopedIndex {
    pub fn new(snapshot: &RetrievalSnapshot, tokenizer: &Tokenizer) -> Result<Self> {
        let mut db = Connection::open_in_memory()?;
        db.execute_batch("CREATE VIRTUAL TABLE chunks_fts USING fts5(heading, segmented_body, content='', contentless_delete=1)")?;
        let tx = db.transaction()?;
        for (index, doc) in snapshot.documents().iter().enumerate() {
            tx.execute(
                "INSERT INTO chunks_fts(rowid,heading,segmented_body) VALUES (?1,?2,?3)",
                params![
                    index + 1,
                    tokenizer.segment_text(&doc.heading),
                    tokenizer.segment_text(&doc.passage)
                ],
            )?;
        }
        tx.commit()?;
        Ok(Self { db })
    }

    /// Tokens are always quoted data. Phrase mode preserves multiword name bridges.
    pub fn search(
        &self,
        terms: &[String],
        phrase: bool,
        limit: usize,
    ) -> Result<Vec<(usize, f64)>> {
        if terms.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let quoted = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
        let query = if phrase {
            quoted(&terms.join(" "))
        } else {
            terms
                .iter()
                .map(|s| quoted(s))
                .collect::<Vec<_>>()
                .join(" OR ")
        };
        let mut stmt = self.db.prepare(
            "SELECT rowid,bm25(chunks_fts) FROM chunks_fts
            WHERE chunks_fts MATCH ?1 ORDER BY bm25(chunks_fts),rowid LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![query, limit], |r| {
            Ok((r.get::<_, usize>(0)? - 1, -r.get::<_, f64>(1)?))
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }
}
