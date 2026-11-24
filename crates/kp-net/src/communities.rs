use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use kp_model::{EpochMs, LibraryId};
use serde::{Deserialize, Serialize};

use crate::{CommunityClient, NetError, Result};

pub const COMMUNITIES_CONFIG_FILE: &str = "communities.json";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JoinedCommunity {
    pub url: String,
    pub token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub library_id: Option<LibraryId>,
    #[cfg(unix)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unix_socket: Option<PathBuf>,
    pub joined_at: EpochMs,
}

impl JoinedCommunity {
    pub fn to_client(&self) -> Result<CommunityClient> {
        #[cfg(unix)]
        if let Some(ref sock) = self.unix_socket {
            return CommunityClient::with_unix_socket(&self.url, &self.token, sock);
        }
        CommunityClient::new(&self.url, &self.token)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct CommunitiesFile {
    #[serde(default)]
    communities: Vec<JoinedCommunity>,
}

pub fn communities_config_path(library_dir: &Path) -> PathBuf {
    library_dir.join(COMMUNITIES_CONFIG_FILE)
}

#[cfg(unix)]
fn verify_communities_file_owner(meta: &fs::Metadata) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let expected_uid = crate::daemon::current_process_uid()?;
    if meta.uid() != expected_uid {
        return Err(NetError::Forbidden);
    }
    Ok(())
}

#[cfg(not(unix))]
fn verify_communities_file_owner(_meta: &fs::Metadata) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn open_private_temp_file(tmp_path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(tmp_path)
}

#[cfg(not(unix))]
fn open_private_temp_file(tmp_path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(tmp_path)
}

#[cfg(unix)]
fn finalize_private_temp_permissions(tmp_path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(tmp_path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn finalize_private_temp_permissions(_tmp_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Loads `<library_dir>/communities.json`, enforcing non-symlink and same-UID ownership checks on Unix.
pub fn load_joined_communities(library_dir: &Path) -> Result<Vec<JoinedCommunity>> {
    let path = communities_config_path(library_dir);
    let meta = match fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(NetError::Io(err)),
    };
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err(NetError::Invalid(
            "communities.json must be a regular non-symlink file",
        ));
    }
    verify_communities_file_owner(&meta)?;
    let bytes = fs::read(&path)?;
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(Vec::new());
    }
    let parsed: CommunitiesFile = serde_json::from_slice(&bytes)?;
    Ok(parsed.communities)
}

/// Atomically writes `<library_dir>/communities.json` with strict `0600` permissions on Unix.
pub fn save_joined_communities(library_dir: &Path, communities: &[JoinedCommunity]) -> Result<()> {
    let target = communities_config_path(library_dir);
    if let Ok(meta) = fs::symlink_metadata(&target) {
        if meta.file_type().is_symlink() {
            return Err(NetError::Invalid("refusing to overwrite symlink"));
        }
    }
    let tmp_name = format!(
        ".communities.json.tmp.{}.{}",
        std::process::id(),
        uuid_nonce()
    );
    let tmp_path = library_dir.join(tmp_name);
    let payload = serde_json::to_vec_pretty(&CommunitiesFile {
        communities: communities.to_vec(),
    })?;

    let write_res = (|| -> Result<()> {
        let mut file = open_private_temp_file(&tmp_path)?;
        file.write_all(&payload)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        drop(file);
        finalize_private_temp_permissions(&tmp_path)?;
        fs::rename(&tmp_path, &target)?;
        Ok(())
    })();

    if write_res.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    write_res
}

pub fn upsert_joined_community(
    library_dir: &Path,
    mut entry: JoinedCommunity,
) -> Result<Vec<JoinedCommunity>> {
    let canonical_url = entry.to_client()?.url().to_owned();
    entry.url = canonical_url;
    let mut items = load_joined_communities(library_dir)?;
    if let Some(existing) = items.iter_mut().find(|c| c.url == entry.url) {
        *existing = entry;
    } else {
        items.push(entry);
    }
    save_joined_communities(library_dir, &items)?;
    Ok(items)
}

pub fn remove_joined_community(library_dir: &Path, url: &str) -> Result<bool> {
    let normalized = CommunityClient::new(url, "")
        .map(|c| c.url().to_owned())
        .unwrap_or_else(|_| url.trim().to_owned());
    let mut items = load_joined_communities(library_dir)?;
    let before = items.len();
    items.retain(|c| {
        c.url != normalized && c.url.trim_end_matches('/') != url.trim_end_matches('/')
    });
    let removed = items.len() != before;
    if removed {
        save_joined_communities(library_dir, &items)?;
    }
    Ok(removed)
}

fn uuid_nonce() -> u128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}
