use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::Path;

use kp_model::{
    ActorId, EpochMs, Fact, FactId, FactKind, KpError, MemberRole, SkillPackage, SkillSlug,
};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::Serialize;

use crate::{Result, Store, StoreError};

#[derive(Clone, Debug, Serialize)]
pub struct ActiveSkill {
    pub fact_id: FactId,
    #[serde(flatten)]
    pub package: SkillPackage,
    pub notice: Option<&'static str>,
}

impl Store {
    pub fn active_skills(&self, actor: &ActorId, at: EpochMs) -> Result<Vec<ActiveSkill>> {
        let has_any_skill: bool = self
            .db
            .prepare_cached(
                "SELECT EXISTS(
                    SELECT 1 FROM facts
                    WHERE kind = 'skill_version' AND status = 'active' AND expired_at IS NULL
                )",
            )?
            .query_row([], |r| r.get(0))?;
        if !has_any_skill {
            return Ok(Vec::new());
        }
        let tx = self.db.unchecked_transaction()?;
        let skills = active(&tx, actor, at)?;
        tx.commit()?;
        Ok(skills)
    }

    pub fn approve_skill(&mut self, slug: &SkillSlug, actor: &ActorId, at: EpochMs) -> Result<()> {
        if self.member_role(actor)? != Some(MemberRole::Owner) || self.owner_actor != *actor {
            return Err(StoreError::PermissionDenied);
        }
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let skill = active(&tx, actor, at)?
            .into_iter()
            .find(|s| &s.package.name == slug)
            .ok_or(StoreError::NotFound)?;
        let graph = crate::facts::read_graph(&tx)?;
        let fact = graph
            .facts
            .get(&skill.fact_id)
            .ok_or(StoreError::NotFound)?;
        tx.execute("INSERT INTO skill_approvals VALUES (?1,?2,1,?3,?4) ON CONFLICT(fact_id) DO UPDATE SET content_sha256=excluded.content_sha256,exec_approved=1,approved_by=excluded.approved_by,approved_at=excluded.approved_at",params![fact.id.as_str(),fact.strong_ref().content_sha256.as_str(),actor.as_str(),at.0])?;
        tx.commit()?;
        self.sync_skills_now()
    }

    /// The directory is a disposable owner-visible projection, never an authority or sync input.
    /// Holding the write lock prevents an older projector from reinstalling a retracted skill.
    pub fn sync_skills_now(&mut self) -> Result<()> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let duration = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| KpError::invalid("clock", "before Unix epoch"))?;
        let at = EpochMs(
            i64::try_from(duration.as_millis())
                .map_err(|_| KpError::invalid("clock", "out of range"))?,
        );
        let owner: String =
            tx.query_row("SELECT owner_actor FROM library_meta", [], |r| r.get(0))?;
        let skills = active(&tx, &owner.parse()?, at)?;
        let root = self.path.join("skills");
        if skills.is_empty() && !root.try_exists()? {
            tx.commit()?;
            return Ok(());
        }
        directory(&root)?;
        let names: BTreeMap<_, _> = skills
            .iter()
            .map(|s| (s.package.name.as_str(), s))
            .collect();
        for entry in fs::read_dir(&root)? {
            let entry = entry?;
            if !entry
                .file_name()
                .to_str()
                .is_some_and(|name| names.contains_key(name))
            {
                remove(&entry.path())?;
            }
        }
        for skill in skills {
            let dir = root.join(skill.package.name.as_str());
            directory(&dir)?;
            let path = dir.join("SKILL.md");
            let text = skill.package.render(skill.package.exec_approved)?;
            skill
                .package
                .validate_markdown(&text, skill.package.exec_approved)?;
            if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
                fs::remove_file(&path)?;
            }
            if fs::read(&path).is_ok_and(|bytes| bytes == text.as_bytes()) {
                continue;
            }
            let temporary = dir.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
            let result = (|| -> std::io::Result<()> {
                let mut file = fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&temporary)?;
                file.write_all(text.as_bytes())?;
                file.sync_all()?;
                fs::rename(&temporary, &path)
            })();
            if result.is_err() {
                let _ = fs::remove_file(&temporary);
            }
            result?;
        }
        tx.commit()?;
        Ok(())
    }
}

pub(crate) fn active(db: &Connection, actor: &ActorId, at: EpochMs) -> Result<Vec<ActiveSkill>> {
    let has_member: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM members_tokens WHERE actor_id=?1 AND revoked_at IS NULL)",
        [actor.as_str()],
        |r| r.get(0),
    )?;
    if !has_member {
        return Ok(Vec::new());
    }
    let has_any_skill: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM facts WHERE kind='skill_version' AND status='active' AND expired_at IS NULL)",
        [],
        |r| r.get(0),
    )?;
    if !has_any_skill {
        return Ok(Vec::new());
    }
    let graph = crate::facts::read_graph(db)?;
    let visible = graph.evaluate(actor, at);
    let local: String = db.query_row("SELECT library_id FROM library_meta", [], |r| r.get(0))?;
    let mut by_name = BTreeMap::new();
    let mut facts = graph
        .facts
        .values()
        .filter(|f| f.kind == FactKind::SkillVersion && visible.facts.contains(&f.id))
        .collect::<Vec<_>>();
    // A remote slug collision cannot displace a locally authored package.
    facts.sort_by_key(|f| {
        (
            f.origin_library.as_str() == local,
            f.object
                .get("version")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
            f.access.recorded_at(),
            &f.id,
        )
    });
    for fact in facts {
        let mut package: SkillPackage = serde_json::from_value(fact.object.clone())?;
        package.validate()?;
        package.exec_approved = locally_approved(db, fact)?;
        let notice = (!package.exec_approved).then_some(kp_model::READ_ONLY_SKILL);
        by_name.insert(
            package.name.clone(),
            ActiveSkill {
                fact_id: fact.id.clone(),
                package,
                notice,
            },
        );
    }
    Ok(by_name.into_values().collect())
}

pub(crate) fn locally_approved(db: &Connection, fact: &Fact) -> Result<bool> {
    let approved: Option<bool> = db
        .query_row(
            "SELECT exec_approved FROM skill_approvals WHERE fact_id=?1 AND content_sha256=?2",
            params![fact.id.as_str(), fact.strong_ref().content_sha256.as_str()],
            |r| r.get(0),
        )
        .optional()?;
    Ok(approved.unwrap_or(false))
}

fn directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            Err(KpError::invalid("skills path", "requires a directory without symlinks").into())
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path)?;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

fn remove(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}
