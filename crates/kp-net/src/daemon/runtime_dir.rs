use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::{Path, PathBuf};

use kp_model::ContentHash;
use tokio::net::UnixStream;

use crate::{NetError, Result};

/// Safe upper bound (`< 90` bytes) well below macOS `sockaddr_un.sun_path` (`104` bytes).
pub const MAX_UDS_PATH_LEN: usize = 90;

/// Returns the current process effective UID using 100% safe Rust (`unsafe_code = "forbid"`)
/// with zero filesystem I/O via `rustix::process::geteuid()`.
pub fn current_process_uid() -> Result<u32> {
    Ok(rustix::process::geteuid().as_raw())
}

pub(super) fn candidate_runtime_dirs(
    is_macos: bool,
    is_linux: bool,
    tmpdir: Option<&OsStr>,
    xdg_runtime: Option<&OsStr>,
    home: Option<&OsStr>,
    uid: u32,
) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    fn nonempty(v: Option<&OsStr>) -> Option<&OsStr> {
        v.filter(|s| !s.as_bytes().is_empty())
    }

    if is_macos {
        if let Some(t) = nonempty(tmpdir) {
            dirs.push(PathBuf::from(t).join(format!("kp-{uid}")));
        }
        if let Some(h) = nonempty(home) {
            dirs.push(PathBuf::from(h).join(".kp").join("run"));
        }
    } else if is_linux {
        if let Some(x) = nonempty(xdg_runtime) {
            dirs.push(PathBuf::from(x).join("kp"));
        }
        if let Some(h) = nonempty(home) {
            dirs.push(PathBuf::from(h).join(".kp").join("run"));
        }
    } else {
        if let Some(x) = nonempty(xdg_runtime) {
            dirs.push(PathBuf::from(x).join("kp"));
        }
        if let Some(t) = nonempty(tmpdir) {
            dirs.push(PathBuf::from(t).join(format!("kp-{uid}")));
        }
        if let Some(h) = nonempty(home) {
            dirs.push(PathBuf::from(h).join(".kp").join("run"));
        }
    }
    dirs
}

fn create_private_dir_0700(dir: &Path) -> Result<()> {
    if std::fs::symlink_metadata(dir).is_err() {
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(0o700);
        if let Some(parent) = dir.parent() {
            if dir.ends_with(".kp/run") && std::fs::symlink_metadata(parent).is_err() {
                builder.create(parent)?;
                std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
            }
        }
        builder.create(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Ensures the parent directory of `socket_path` exists with `0o700` (if newly created) and passes
/// strict owner UID (`== expected_uid`), non-symlink, and `mode & 0o077 == 0` checks.
pub(crate) fn ensure_socket_parent_dir_with_uid(
    socket_path: &Path,
    expected_uid: u32,
) -> Result<()> {
    let parent = socket_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or(NetError::Invalid(
            "daemon socket path must have a parent directory",
        ))?;
    create_private_dir_0700(parent)?;
    verify_socket_parent_dir_with_uid(socket_path, expected_uid)
}

/// Verifies that `socket_path`'s parent directory is a real non-symlink directory owned by
/// `expected_uid` with zero group/other permission bits (`mode & 0o077 == 0`).
pub fn verify_socket_parent_dir_with_uid(socket_path: &Path, expected_uid: u32) -> Result<()> {
    let parent = socket_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or(NetError::Invalid(
            "daemon socket path must have a parent directory",
        ))?;
    let meta = std::fs::symlink_metadata(parent).map_err(|_| NetError::Unavailable)?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(NetError::Forbidden);
    }
    if meta.uid() != expected_uid {
        return Err(NetError::Forbidden);
    }
    if (meta.mode() & 0o077) != 0 {
        return Err(NetError::Forbidden);
    }
    Ok(())
}

/// Verifies that the peer of an async `UnixStream` matches `expected_uid` via `peer_cred()`.
pub fn verify_stream_peer_uid(stream: &UnixStream, expected_uid: u32) -> Result<()> {
    let cred = stream.peer_cred().map_err(|_| NetError::Forbidden)?;
    if cred.uid() != expected_uid {
        return Err(NetError::Forbidden);
    }
    Ok(())
}

/// Verifies that the peer of a blocking `std::os::unix::net::UnixStream` matches `expected_uid`
/// via `peer_cred()` in 100% safe Rust (`unsafe_code = "forbid"`), restoring blocking mode before return.
pub fn verify_std_stream_peer_uid(stream: &StdUnixStream, expected_uid: u32) -> Result<()> {
    static PEER_CRED_IO_RT: std::sync::OnceLock<std::io::Result<tokio::runtime::Runtime>> =
        std::sync::OnceLock::new();

    let rt = PEER_CRED_IO_RT
        .get_or_init(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_io()
                .build()
        })
        .as_ref()
        .map_err(|_| NetError::Forbidden)?;

    let _enter = rt.enter();
    let cloned = stream.try_clone().map_err(|_| NetError::Forbidden)?;
    if cloned.set_nonblocking(true).is_err() {
        let _ = stream.set_nonblocking(false);
        return Err(NetError::Forbidden);
    }
    let peer_uid_res = match UnixStream::from_std(cloned) {
        Ok(tok_stream) => {
            let cred_res = tok_stream.peer_cred().map(|c| c.uid());
            if let Ok(back_std) = tok_stream.into_std() {
                let _ = back_std.set_nonblocking(false);
            }
            cred_res
        }
        Err(err) => Err(err),
    };
    let _ = stream.set_nonblocking(false);

    let peer_uid = peer_uid_res.map_err(|_| NetError::Forbidden)?;
    if peer_uid != expected_uid {
        return Err(NetError::Forbidden);
    }
    Ok(())
}

/// Resolves and initializes the user-private `0700` runtime socket directory:
/// `$TMPDIR/kp-<uid>` on macOS, `$XDG_RUNTIME_DIR/kp` on Linux, fallback `~/.kp/run/`.
pub fn daemon_runtime_dir() -> Result<PathBuf> {
    let uid = current_process_uid()?;
    let tmpdir = std::env::var_os("TMPDIR");
    let xdg = std::env::var_os("XDG_RUNTIME_DIR");
    let home = std::env::var_os("HOME");
    let candidates = candidate_runtime_dirs(
        cfg!(target_os = "macos"),
        cfg!(target_os = "linux"),
        tmpdir.as_deref(),
        xdg.as_deref(),
        home.as_deref(),
        uid,
    );

    // Leave at least 12 bytes (`/<8hex>.s`) inside `MAX_UDS_PATH_LEN` (`< 90` bytes).
    for dir in candidates {
        if dir.as_os_str().as_bytes().len() + 12 >= MAX_UDS_PATH_LEN {
            continue;
        }
        create_private_dir_0700(&dir)?;
        let probe_sock = dir.join("p.s");
        verify_socket_parent_dir_with_uid(&probe_sock, uid)?;
        return Ok(dir);
    }

    Err(NetError::Invalid(
        "unable to resolve a user-owned 0700 runtime directory shorter than 90 bytes",
    ))
}

/// Computes a deterministic Unix Domain Socket path inside the user's `0700` runtime directory,
/// hashing the filename if needed so the full path always stays `< 90` bytes (`SUN_PATH`).
pub fn daemon_socket_path(library_dir: &Path) -> Result<PathBuf> {
    let runtime_dir = daemon_runtime_dir()?;
    socket_path_in_dir(&runtime_dir, library_dir)
}

pub(super) fn socket_path_in_dir(runtime_dir: &Path, library_dir: &Path) -> Result<PathBuf> {
    let canonical = library_dir
        .canonicalize()
        .unwrap_or_else(|_| library_dir.to_path_buf());
    let digest = ContentHash::digest(canonical.as_os_str().as_bytes());
    let hex = digest.as_str();
    for name in [
        format!("kp-{}.sock", &hex[..16]),
        format!("{}.sock", &hex[..12]),
        format!("{}.s", &hex[..10]),
        format!("{}.s", &hex[..8]),
    ] {
        let candidate = runtime_dir.join(name);
        if validate_socket_path(&candidate).is_ok() {
            return Ok(candidate);
        }
    }
    Err(NetError::Invalid(
        "daemon socket path must be nonempty and shorter than 90 bytes",
    ))
}

/// Validates that `path` fits cleanly inside macOS/BSD `SUN_PATH` (`< 90` bytes).
pub fn validate_socket_path(path: &Path) -> Result<()> {
    let len = path.as_os_str().as_bytes().len();
    if len == 0 || len >= MAX_UDS_PATH_LEN {
        return Err(NetError::Invalid(
            "daemon socket path must be nonempty and shorter than 90 bytes",
        ));
    }
    Ok(())
}

/// Companion advisory lockfile path (`<socket_path>.lock`) held exclusively for the entire
/// lifecycle of a running `kp daemon` instance.
pub fn daemon_lock_path_for_socket(socket_path: &Path) -> PathBuf {
    let mut os = socket_path.as_os_str().to_os_string();
    os.push(".lock");
    PathBuf::from(os)
}

/// Returns `true` if a live `kp daemon` currently holds the advisory lock for `library_dir`
/// (allowing hook autostart logic to distinguish a busy running daemon from an absent one).
pub fn is_daemon_lock_held(library_dir: &Path) -> bool {
    let Ok(sock) = daemon_socket_path(library_dir) else {
        return false;
    };
    is_socket_lock_held(&sock)
}

/// Non-destructively probes whether `<socket_path>.lock` is currently held by an active process.
pub fn is_socket_lock_held(socket_path: &Path) -> bool {
    let lock_path = daemon_lock_path_for_socket(socket_path);
    let Ok(file) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock_path)
    else {
        return false;
    };
    match file.try_lock() {
        Ok(()) => {
            let _ = file.unlock();
            false
        }
        Err(std::fs::TryLockError::WouldBlock) => true,
        Err(std::fs::TryLockError::Error(_)) => false,
    }
}
