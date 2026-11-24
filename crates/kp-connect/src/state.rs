//! Connector-owned cursor and scope state persistence (`Rev 5 §0.1.4 & §0.1.5`).
//!
//! Decouples connector cursors completely from Core SQLite: cursors and connector
//! scope configurations live in a user-private JSON state file (default `~/.kp/connectors/state.json`,
//! overridable via `KP_CONNECTOR_STATE_PATH`) written atomically via same-directory `tempfile` +
//! `0600` file permissions + `sync_all()` + POSIX rename.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use kp_model::{ActorId, ConnectorId, SourceScopeId};
use serde::{Deserialize, Serialize};

use crate::{
    ConnectError, Connector, ConnectorCursor, ConnectorKind, ConnectorScope, PollBatch, Result,
};

/// Default relative path under `$HOME` for connector-owned state (`~/.kp/connectors/state.json`).
pub const DEFAULT_STATE_REL_PATH: &str = ".kp/connectors/state.json";

/// Environment variable overriding the connector state file path.
pub const ENV_CONNECTOR_STATE_PATH: &str = "KP_CONNECTOR_STATE_PATH";

/// Schema version for [`ConnectorStateSnapshot`].
pub const STATE_SCHEMA_VERSION: u32 = 1;

fn default_schema_version() -> u32 {
    STATE_SCHEMA_VERSION
}

/// Deterministic composite lookup key `"{connector_id}::{source_scope_id}"` inside `state.json`.
pub fn connector_scope_state_key(
    connector_id: &ConnectorId,
    source_scope_id: &SourceScopeId,
) -> String {
    format!("{connector_id}::{source_scope_id}")
}

/// Serializable connector state persisted in `~/.kp/connectors/state.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorStateSnapshot {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub scopes: BTreeMap<String, ConnectorScope>,
    #[serde(default)]
    pub cursors: BTreeMap<String, ConnectorCursor>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub scope_kinds: BTreeMap<String, ConnectorKind>,
}

impl Default for ConnectorStateSnapshot {
    fn default() -> Self {
        Self {
            schema_version: STATE_SCHEMA_VERSION,
            scopes: BTreeMap::new(),
            cursors: BTreeMap::new(),
            scope_kinds: BTreeMap::new(),
        }
    }
}

/// Atomic JSON file store for connector cursors and configured scopes.
#[derive(Clone, Debug)]
pub struct ConnectorStateStore {
    path: PathBuf,
}

impl ConnectorStateStore {
    /// Creates a state store backed by the given JSON file path.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Resolves the default user-level state file (`$KP_CONNECTOR_STATE_PATH` or `~/.kp/connectors/state.json`).
    pub fn default_user_path() -> Result<PathBuf> {
        if let Some(override_path) = std::env::var_os(ENV_CONNECTOR_STATE_PATH) {
            if !override_path.is_empty() {
                return Ok(PathBuf::from(override_path));
            }
        }
        let home = std::env::var_os("HOME").ok_or_else(|| {
            ConnectError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "neither KP_CONNECTOR_STATE_PATH nor HOME is set",
            ))
        })?;
        Ok(PathBuf::from(home).join(DEFAULT_STATE_REL_PATH))
    }

    /// Opens a state store at [`Self::default_user_path`].
    pub fn open_default() -> Result<Self> {
        Ok(Self::new(Self::default_user_path()?))
    }

    /// Returns the underlying state file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Loads the persisted snapshot, returning an empty [`ConnectorStateSnapshot::default`] when
    /// the state file has not been created yet.
    pub fn load(&self) -> Result<ConnectorStateSnapshot> {
        if !self.path.exists() {
            return Ok(ConnectorStateSnapshot::default());
        }
        let bytes = fs::read(&self.path)?;
        let snapshot: ConnectorStateSnapshot = serde_json::from_slice(&bytes)?;
        Ok(snapshot)
    }

    /// Atomically writes `snapshot` to `self.path` using a sibling temporary file, `0700` parent
    /// directory permissions, `0600` file permissions on Unix, `sync_all()`, and atomic rename.
    pub fn save(&self, snapshot: &ConnectorStateSnapshot) -> Result<()> {
        let parent = self
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        ensure_private_dir(parent)?;

        let mut payload = serde_json::to_vec_pretty(snapshot)?;
        payload.push(b'\n');

        let mut temp = tempfile::Builder::new()
            .prefix(".kp-connect-state.")
            .suffix(".tmp")
            .tempfile_in(parent)?;
        set_private_file_permissions(temp.path())?;
        temp.write_all(&payload)?;
        temp.flush()?;
        temp.as_file().sync_all()?;
        temp.persist(&self.path)
            .map_err(|err| ConnectError::Io(err.error))?;
        Ok(())
    }

    /// Reads the saved cursor for `(connector_id, source_scope_id)`, if present.
    pub fn get_cursor(
        &self,
        connector_id: &ConnectorId,
        source_scope_id: &SourceScopeId,
    ) -> Result<Option<ConnectorCursor>> {
        let key = connector_scope_state_key(connector_id, source_scope_id);
        Ok(self.load()?.cursors.remove(&key))
    }

    /// Reads the saved cursor for `(connector_id, source_scope_id)` or [`ConnectorCursor::default`].
    pub fn get_cursor_or_default(
        &self,
        connector_id: &ConnectorId,
        source_scope_id: &SourceScopeId,
    ) -> Result<ConnectorCursor> {
        Ok(self
            .get_cursor(connector_id, source_scope_id)?
            .unwrap_or_default())
    }

    /// Atomically updates the cursor for `(connector_id, source_scope_id)`.
    pub fn put_cursor(
        &self,
        connector_id: &ConnectorId,
        source_scope_id: &SourceScopeId,
        cursor: ConnectorCursor,
    ) -> Result<()> {
        let mut snapshot = self.load()?;
        let key = connector_scope_state_key(connector_id, source_scope_id);
        snapshot.cursors.insert(key, cursor);
        self.save(&snapshot)
    }

    /// Removes a stored cursor for `(connector_id, source_scope_id)`, returning `true` if one existed.
    pub fn remove_cursor(
        &self,
        connector_id: &ConnectorId,
        source_scope_id: &SourceScopeId,
    ) -> Result<bool> {
        let mut snapshot = self.load()?;
        let key = connector_scope_state_key(connector_id, source_scope_id);
        let existed = snapshot.cursors.remove(&key).is_some();
        if existed {
            self.save(&snapshot)?;
        }
        Ok(existed)
    }

    /// Registers or updates a [`ConnectorScope`] configuration in the state file.
    pub fn upsert_scope(&self, scope: ConnectorScope) -> Result<()> {
        let mut snapshot = self.load()?;
        let key = connector_scope_state_key(&scope.connector_id, &scope.source_scope_id);
        snapshot.scopes.insert(key, scope);
        self.save(&snapshot)
    }

    /// Looks up a configured [`ConnectorScope`] by `(connector_id, source_scope_id)`.
    pub fn get_scope(
        &self,
        connector_id: &ConnectorId,
        source_scope_id: &SourceScopeId,
    ) -> Result<Option<ConnectorScope>> {
        let key = connector_scope_state_key(connector_id, source_scope_id);
        Ok(self.load()?.scopes.remove(&key))
    }

    /// Lists all configured [`ConnectorScope`]s in deterministic key order.
    pub fn list_scopes(&self) -> Result<Vec<ConnectorScope>> {
        Ok(self.load()?.scopes.into_values().collect())
    }

    /// Loads the cursor for `scope`, runs [`Connector::poll_incremental`], atomically persists
    /// `scope` and `batch.next_cursor` to `self.path`, and returns the resulting [`PollBatch`].
    pub async fn poll_and_persist<C: Connector + ?Sized>(
        &self,
        connector: &C,
        scope: &ConnectorScope,
        ingested_by: &ActorId,
    ) -> Result<PollBatch> {
        let cursor = self.get_cursor_or_default(&scope.connector_id, &scope.source_scope_id)?;
        let batch = connector
            .poll_incremental(scope, &cursor, ingested_by)
            .await?;
        let mut snapshot = self.load()?;
        let key = connector_scope_state_key(&scope.connector_id, &scope.source_scope_id);
        snapshot.scopes.insert(key.clone(), scope.clone());
        snapshot.cursors.insert(key, batch.next_cursor.clone());
        self.save(&snapshot)?;
        Ok(batch)
    }
}

fn ensure_private_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        if !dir.exists() {
            let mut builder = fs::DirBuilder::new();
            builder.recursive(true).mode(0o700);
            builder.create(dir)?;
        } else if dir != Path::new(".") {
            let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
        }
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(dir)?;
    }
    Ok(())
}

fn set_private_file_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use kp_model::{EpochMs, ModelRoutePolicy};

    use super::*;

    #[test]
    fn state_store_roundtrips_cursors_and_scopes_with_private_permissions() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let state_path = temp.path().join("nested/connectors/state.json");
        let store = ConnectorStateStore::new(&state_path);

        let initial = store.load()?;
        assert_eq!(initial.schema_version, STATE_SCHEMA_VERSION);
        assert!(initial.cursors.is_empty());
        assert!(initial.scopes.is_empty());

        let connector_id: ConnectorId = "feishu_main".parse()?;
        let scope_id: SourceScopeId = "feishu.chat.oc_cutover".parse()?;
        let scope = ConnectorScope {
            connector_id: connector_id.clone(),
            source_scope_id: scope_id.clone(),
            upstream_container_id: "chat:oc_cutover".into(),
            label: "运维切换群".into(),
            enabled: true,
            model_policy: ModelRoutePolicy::HostCli,
        };
        store.upsert_scope(scope.clone())?;

        let cursor = ConnectorCursor {
            cursor_token: Some("{\"v\":1}".into()),
            high_water_ms: Some(EpochMs(1_795_245_000_000)),
            updated_at: EpochMs(1_795_245_380_000),
        };
        store.put_cursor(&connector_id, &scope_id, cursor.clone())?;

        assert_eq!(store.get_cursor(&connector_id, &scope_id)?, Some(cursor));
        assert_eq!(store.get_scope(&connector_id, &scope_id)?, Some(scope));
        assert_eq!(store.list_scopes()?.len(), 1);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let file_mode = fs::metadata(&state_path)?.permissions().mode() & 0o777;
            assert_eq!(file_mode, 0o600);
            let dir_mode = fs::metadata(state_path.parent().unwrap_or(temp.path()))?
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(dir_mode, 0o700);
        }

        assert!(store.remove_cursor(&connector_id, &scope_id)?);
        assert_eq!(store.get_cursor(&connector_id, &scope_id)?, None);
        Ok(())
    }
}
