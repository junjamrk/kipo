//! Connector-to-Public-Ingest-Contract conversion helpers (`Rev 5 §0.1.2 & §0.1.3`).
//!
//! Public wire structs, validation, and [`parse_public_jsonl_str`] live in [`kp_model::ingest`] and
//! are re-exported here for `kp_connect` callers. This module converts normalized connector batches
//! ([`IngestedSourceEvent`], [`SourceScopeRoster`], [`PollBatch`]) into those public JSONL lines.

use std::collections::BTreeMap;
use std::io::Write;

pub use kp_model::{
    parse_public_jsonl_str, PublicAudience, PublicContainerRoster, PublicIngestLine,
    PublicIngestOp, PublicIngestRecord, PublicIngestSource, PublicSpeaker,
};
use kp_model::{ActorId, ModelRoutePolicy};

use crate::{
    ConnectorKind, IngestedSourceEvent, PollBatch, Result, SourceEventOp, SourceScopeRoster,
};

impl From<&SourceEventOp> for PublicIngestOp {
    fn from(op: &SourceEventOp) -> Self {
        match op {
            SourceEventOp::Upsert => Self::Upsert,
            SourceEventOp::Supersede {
                previous_external_key,
            } => Self::Supersede {
                previous_external_id: previous_external_key.clone(),
            },
            SourceEventOp::Withdraw => Self::Withdraw,
        }
    }
}

impl From<SourceEventOp> for PublicIngestOp {
    fn from(op: SourceEventOp) -> Self {
        Self::from(&op)
    }
}

fn format_external_identity(provider: &str, external_id: &str) -> String {
    let prefix_colon = format!("{provider}:");
    let alt_underscore = format!("{}:", provider.replace('-', "_"));
    if let Some(rest) = external_id.strip_prefix(&prefix_colon) {
        format!("{provider}:{rest}")
    } else if let Some(rest) = external_id.strip_prefix(&alt_underscore) {
        format!("{provider}:{rest}")
    } else {
        format!("{provider}:{external_id}")
    }
}

impl IngestedSourceEvent {
    /// Converts this normalized connector event into the public JSONL [`PublicIngestRecord`] contract.
    ///
    /// When `confirmed_identities` resolves `(provider, speaker_external_id)` to a library
    /// [`ActorId`], `speaker.id` carries that `actor_id`; otherwise it carries the namespaced
    /// external identifier `"{provider}:{external_id}"` (or `ingested_by` for local agent sessions).
    pub fn to_public_ingest_record(
        &self,
        connector_kind: ConnectorKind,
        confirmed_identities: &BTreeMap<(String, String), ActorId>,
    ) -> PublicIngestRecord {
        let provider = connector_kind.as_str();
        let speaker_id = if let Some(actor) =
            confirmed_identities.get(&(provider.to_owned(), self.speaker_external_id.clone()))
        {
            actor.as_str().to_owned()
        } else if connector_kind == ConnectorKind::AgentSession {
            self.ingested_by.as_str().to_owned()
        } else {
            format_external_identity(provider, &self.speaker_external_id)
        };

        let model_policy = if self.model_policy == ModelRoutePolicy::HostCli {
            None
        } else {
            Some(self.model_policy.clone())
        };

        PublicIngestRecord {
            source: PublicIngestSource {
                kind: provider.to_owned(),
                uri: self.uri.clone(),
                external_id: self.external_key.clone(),
                container_id: Some(self.source_scope_id.as_str().to_owned()),
            },
            speaker: PublicSpeaker::new(speaker_id, Some(self.speaker_display_name.clone())),
            audience: PublicAudience::ContainerMembers,
            occurred_at: self.occurred_at,
            content: self.content.clone(),
            op: PublicIngestOp::from(&self.event_op),
            ingested_by: Some(self.ingested_by.clone()),
            media_type: Some(self.media_type.clone()),
            model_policy,
        }
    }
}

impl SourceScopeRoster {
    /// Converts this live scope roster into a public [`PublicContainerRoster`] JSONL record.
    pub fn to_public_container_roster(
        &self,
        confirmed_identities: &BTreeMap<(String, String), ActorId>,
    ) -> PublicContainerRoster {
        let members = self
            .external_member_ids
            .iter()
            .map(|ext_id| {
                if let Some(actor) =
                    confirmed_identities.get(&(self.provider.clone(), ext_id.clone()))
                {
                    actor.as_str().to_owned()
                } else {
                    format_external_identity(&self.provider, ext_id)
                }
            })
            .collect();

        PublicContainerRoster {
            container_id: self.scope_id.as_str().to_owned(),
            label: self.label.clone(),
            members,
            all_library_members: self.all_library_members,
            synced_at: self.synced_at,
            provider: Some(self.provider.clone()),
            upstream_container_id: Some(self.external_container_id.clone()),
        }
    }
}

impl PollBatch {
    /// Converts all events in this batch into [`PublicIngestRecord`]s.
    pub fn to_public_ingest_records(
        &self,
        connector_kind: ConnectorKind,
        confirmed_identities: &BTreeMap<(String, String), ActorId>,
    ) -> Vec<PublicIngestRecord> {
        self.events
            .iter()
            .map(|ev| ev.to_public_ingest_record(connector_kind, confirmed_identities))
            .collect()
    }

    /// Converts all container membership updates in this batch into [`PublicContainerRoster`]s.
    pub fn to_public_container_rosters(
        &self,
        confirmed_identities: &BTreeMap<(String, String), ActorId>,
    ) -> Vec<PublicContainerRoster> {
        self.scope_rosters
            .iter()
            .map(|r| r.to_public_container_roster(confirmed_identities))
            .collect()
    }

    /// Produces ordered [`PublicIngestLine`] items (container rosters first, then event records).
    pub fn to_public_jsonl_lines(
        &self,
        connector_kind: ConnectorKind,
        confirmed_identities: &BTreeMap<(String, String), ActorId>,
    ) -> Vec<PublicIngestLine> {
        let mut lines = Vec::with_capacity(self.scope_rosters.len() + self.events.len());
        for roster in &self.scope_rosters {
            lines.push(PublicIngestLine::ContainerRoster(
                roster.to_public_container_roster(confirmed_identities),
            ));
        }
        for event in &self.events {
            lines.push(PublicIngestLine::Record(Box::new(
                event.to_public_ingest_record(connector_kind, confirmed_identities),
            )));
        }
        lines
    }

    /// Writes this batch as newline-delimited JSON (`JSONL`) to `writer`, returning the line count.
    pub fn write_public_jsonl<W: Write>(
        &self,
        mut writer: W,
        connector_kind: ConnectorKind,
        confirmed_identities: &BTreeMap<(String, String), ActorId>,
    ) -> Result<usize> {
        let lines = self.to_public_jsonl_lines(connector_kind, confirmed_identities);
        for item in &lines {
            item.validate()?;
            serde_json::to_writer(&mut writer, item)?;
            writer.write_all(b"\n")?;
        }
        writer.flush()?;
        Ok(lines.len())
    }

    /// Formats this batch as a UTF-8 JSONL string ready for `kp ingest`.
    pub fn to_public_jsonl_string(
        &self,
        connector_kind: ConnectorKind,
        confirmed_identities: &BTreeMap<(String, String), ActorId>,
    ) -> Result<String> {
        let mut buf = Vec::new();
        self.write_public_jsonl(&mut buf, connector_kind, confirmed_identities)?;
        String::from_utf8(buf)
            .map_err(|e| crate::ConnectError::Io(std::io::Error::other(e.to_string())))
    }
}

#[cfg(test)]
mod tests {
    use kp_model::{EpochMs, SourceScopeId};

    use super::*;
    use crate::ConnectorCursor;

    #[test]
    fn poll_batch_converts_connector_rosters_and_events_into_kp_model_ingest_lines() -> Result<()> {
        let alice: ActorId = "eng_alice".parse()?;
        let scope_id: SourceScopeId = "feishu.chat.oc_cutover".parse()?;
        let confirmed = BTreeMap::from([(("feishu".into(), "ou_alice".into()), alice.clone())]);

        let batch = PollBatch {
            scope_rosters: vec![SourceScopeRoster {
                scope_id: scope_id.clone(),
                provider: "feishu".into(),
                external_container_id: "oc_cutover".into(),
                label: "运维切换群".into(),
                external_member_ids: vec!["ou_alice".into(), "ou_pm_li".into()],
                all_library_members: false,
                synced_at: EpochMs(1_795_245_000_000),
            }],
            events: vec![IngestedSourceEvent {
                external_key: "om_1001".into(),
                uri: "feishu://chat/oc_cutover/om_1001".into(),
                media_type: "text/plain".into(),
                speaker_external_id: "ou_pm_li".into(),
                speaker_display_name: "李四".into(),
                ingested_by: alice,
                source_scope_id: scope_id,
                content: "下周四凌晨两点数据库主从切换".into(),
                event_op: SourceEventOp::Upsert,
                occurred_at: EpochMs(1_795_245_000_000),
                model_policy: ModelRoutePolicy::HostCli,
            }],
            identity_proposals: Vec::new(),
            next_cursor: ConnectorCursor::default(),
            skipped_low_signal: 0,
        };

        let jsonl = batch.to_public_jsonl_string(ConnectorKind::Feishu, &confirmed)?;
        let parsed = kp_model::parse_public_jsonl_str(&jsonl)?;
        assert_eq!(parsed.len(), 2);
        assert!(matches!(
            &parsed[0],
            PublicIngestLine::ContainerRoster(r)
                if r.members == vec!["eng_alice".to_owned(), "feishu:ou_pm_li".to_owned()]
        ));
        assert!(matches!(
            &parsed[1],
            PublicIngestLine::Record(rec)
                if rec.speaker.id == "feishu:ou_pm_li"
                    && rec.audience == PublicAudience::ContainerMembers
                    && rec.op == PublicIngestOp::Upsert
                    && rec.model_policy.is_none()
        ));
        Ok(())
    }
}
