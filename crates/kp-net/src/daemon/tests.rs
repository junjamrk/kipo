use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kp_model::{
    ActorId, AnchorSpec, AudienceSpec, BitemporalStamp, BoxFuture, EpochMs, Fact, FactAccess,
    FactKind, FactRank, FactStatus, LibraryMode, MemberRole, TrustTier, ValidityWindow,
};
use kp_store::{ChunkInput, ExtractionJobStatus, InitOptions, RawSourceInput, Store};
use serde_json::json;
use tokio::sync::Notify;

use super::runtime_dir::{candidate_runtime_dirs, socket_path_in_dir};
use super::*;
use crate::{NetError, Result};

type TestResult<T = ()> = std::result::Result<T, Box<dyn Error>>;

const AT: EpochMs = EpochMs(1_763_683_200_000);
static UNIT_SOCK_SEQ: AtomicU64 = AtomicU64::new(1);

struct NoopDaemonHandler;

impl DaemonHandler for NoopDaemonHandler {}

fn temp_store(id: &str) -> TestResult<(tempfile::TempDir, Store)> {
    let dir = tempfile::tempdir()?;
    let store = Store::init(
        dir.path(),
        InitOptions {
            library_id: id.parse()?,
            name: id.into(),
            mode: LibraryMode::Personal,
            owner: "alice".parse()?,
            created_at: EpochMs(0),
        },
    )?;
    Ok((dir, store))
}

fn unit_socket(tag: &str) -> TestResult<PathBuf> {
    let dir = daemon_runtime_dir()?;
    let seq = UNIT_SOCK_SEQ.fetch_add(1, Ordering::Relaxed);
    Ok(dir.join(format!("u-{tag}-{seq}.s")))
}

fn make_unit_fact(
    id: &str,
    author: &str,
    kind: FactKind,
    actors: &[&str],
    recorded_at: EpochMs,
    object: serde_json::Value,
) -> TestResult<Fact> {
    Ok(Fact {
        id: id.parse()?,
        origin_library: format!("{author}-personal").parse()?,
        logical_key: id.parse()?,
        kind,
        subject: id.into(),
        predicate: "kp.core.constraint".into(),
        object,
        tags: BTreeSet::new(),
        author: author.parse()?,
        ingested_by: None,
        trust_tier: TrustTier::HumanVerbatim,
        access: FactAccess::Base {
            audience: AudienceSpec::Explicit(
                actors
                    .iter()
                    .map(|a| a.parse())
                    .collect::<kp_model::Result<_>>()?,
            ),
            stamp: BitemporalStamp {
                valid: ValidityWindow {
                    valid_from: EpochMs(0),
                    valid_until: EpochMs::OPEN_END,
                },
                recorded_at,
                expired_at: None,
            },
        },
        source_scope_id: None,
        governed_scope: None,
        broader_scope: None,
        status: FactStatus::Active,
        superseded_by: None,
        rank: FactRank::Normal,
        source_anchor: None,
        unverified_gaps: Vec::new(),
        miss_count: 0,
    })
}

#[test]
fn runtime_dir_priority_and_filename_hashing() -> TestResult {
    // 1. macOS prefers `$TMPDIR/kp-<uid>`, Linux prefers `$XDG_RUNTIME_DIR/kp`, both fallback to `~/.kp/run`.
    let mac_dirs = candidate_runtime_dirs(
        true,
        false,
        Some(OsStr::new("/var/folders/ab/cd/T")),
        Some(OsStr::new("/run/user/501")),
        Some(OsStr::new("/srv/acct/alice")),
        501,
    );
    assert_eq!(mac_dirs[0], PathBuf::from("/var/folders/ab/cd/T/kp-501"));
    assert_eq!(mac_dirs[1], PathBuf::from("/srv/acct/alice/.kp/run"));

    let linux_dirs = candidate_runtime_dirs(
        false,
        true,
        Some(OsStr::new("/tmp")),
        Some(OsStr::new("/run/user/1000")),
        Some(OsStr::new("/srv/acct/alice")),
        1000,
    );
    assert_eq!(linux_dirs[0], PathBuf::from("/run/user/1000/kp"));
    assert_eq!(linux_dirs[1], PathBuf::from("/srv/acct/alice/.kp/run"));

    // 2. Long runtime directory hashes filename shorter so total path stays `< 90` bytes.
    let seventy_four_byte_dir = PathBuf::from(format!("/srv/{}", "d".repeat(69)));
    let short_sock = socket_path_in_dir(&seventy_four_byte_dir, Path::new("/srv/very/deep/lib"))?;
    assert!(short_sock.as_os_str().as_bytes().len() < MAX_UDS_PATH_LEN);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn peer_uid_rejection_and_bounded_sync_read_enforced_end_to_end() -> TestResult {
    let uid = current_process_uid()?;
    assert_eq!(uid, rustix::process::geteuid().as_raw());
    let (_dir, store) = temp_store("unit-sec")?;
    let sock = unit_socket("sec")?;

    let parent = sock.parent().ok_or("missing parent")?;
    let meta = std::fs::symlink_metadata(parent)?;
    assert_eq!(meta.uid(), uid);
    assert_eq!(meta.permissions().mode() & 0o077, 0);

    // Parent-dir wrong-UID & `0755` checks reject untrusted endpoints with `Err(NetError::Forbidden)`.
    assert!(matches!(
        verify_socket_parent_dir_with_uid(&sock, uid.wrapping_add(1)),
        Err(NetError::Forbidden)
    ));
    let loose_dir = tempfile::tempdir()?;
    std::fs::set_permissions(loose_dir.path(), std::fs::Permissions::from_mode(0o755))?;
    let loose_sock = loose_dir.path().join("d.s");
    if validate_socket_path(&loose_sock).is_ok() {
        let (_bad_dir, bad_store) = temp_store("unit-bad-dir")?;
        assert!(matches!(
            DaemonServer::spawn_default(
                bad_store,
                DaemonConfig {
                    socket_path: loose_sock.clone(),
                    idle_extract_window: Duration::from_secs(60),
                    idle_shutdown_timeout: None,
                    max_frame_bytes: 64 * 1024,
                    web_inspector: None,
                },
            )
            .await,
            Err(NetError::Forbidden)
        ));
        assert!(matches!(
            DaemonClient::new(&loose_sock)?.ping().await,
            Err(NetError::Forbidden)
        ));
        assert!(matches!(
            DaemonSyncClient::new(&loose_sock)?.request(&DaemonRequest::Ping),
            Err(NetError::Forbidden)
        ));
    }

    let mut daemon = DaemonServer::spawn_default(
        store,
        DaemonConfig {
            socket_path: sock.clone(),
            idle_extract_window: Duration::from_secs(60),
            idle_shutdown_timeout: None,
            max_frame_bytes: 64 * 1024,
            web_inspector: None,
        },
    )
    .await?;

    let status = DaemonClient::new(&sock)?.status().await?;
    assert_eq!(status.owner_actor.as_str(), "alice");

    // Bounded `.take(max_frame_bytes + 1)` rejects oversized responses on `DaemonSyncClient`.
    let bounded_sync = DaemonSyncClient::new(&sock)?.with_max_frame_bytes(16);
    assert!(matches!(
        bounded_sync.request(&DaemonRequest::Status),
        Err(NetError::Invalid("daemon frame exceeds max_frame_bytes"))
    ));

    // `DaemonHandle::wait` + `subscribe_shutdown` wake immediately on `Shutdown` RPC without polling.
    let mut sub = daemon.subscribe_shutdown();
    assert!(!*sub.borrow());
    let shutdown_client = DaemonClient::new(&sock)?;
    tokio::spawn(async move {
        let _ = shutdown_client.shutdown().await;
    });
    tokio::time::timeout(Duration::from_secs(5), daemon.wait()).await?;
    assert!(*sub.borrow_and_update());
    daemon.shutdown().await?;

    // Server-side peer UID check drops client with mismatched UID.
    let (_dir2, store2) = temp_store("unit-srv-uid")?;
    let sock2 = unit_socket("srv")?;
    let strict_srv = DaemonServer::spawn_with_expected_peer_uid_for_test(
        store2,
        DaemonConfig {
            socket_path: sock2.clone(),
            idle_extract_window: Duration::from_secs(60),
            idle_shutdown_timeout: None,
            max_frame_bytes: 64 * 1024,
            web_inspector: None,
        },
        Arc::new(NoopDaemonHandler),
        uid.wrapping_add(1),
    )
    .await?;
    assert!(matches!(
        DaemonClient::new(&sock2)?.ping().await,
        Err(NetError::Forbidden | NetError::Unavailable | NetError::Io(_))
    ));
    assert!(matches!(
        DaemonSyncClient::new(&sock2)?.request(&DaemonRequest::Ping),
        Err(NetError::Forbidden | NetError::Unavailable | NetError::Io(_))
    ));
    strict_srv.shutdown().await?;
    Ok(())
}

struct UnitRetryHandler {
    fail_left: AtomicUsize,
    hold_quota: AtomicBool,
    calls: AtomicUsize,
    quota_hit_notify: Notify,
    done_notify: Notify,
    done: Mutex<Vec<IdleSessionBatch>>,
}

impl UnitRetryHandler {
    fn new(fail_left: usize, hold_quota: bool) -> Self {
        Self {
            fail_left: AtomicUsize::new(fail_left),
            hold_quota: AtomicBool::new(hold_quota),
            calls: AtomicUsize::new(0),
            quota_hit_notify: Notify::new(),
            done_notify: Notify::new(),
            done: Mutex::new(Vec::new()),
        }
    }
}

impl DaemonHandler for UnitRetryHandler {
    fn extract_session_batch<'a>(
        &'a self,
        _library_dir: &'a Path,
        batch: IdleSessionBatch,
    ) -> BoxFuture<'a, Result<Option<u64>>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail_left.load(Ordering::SeqCst) > 0 {
                self.fail_left.fetch_sub(1, Ordering::SeqCst);
                return Err(NetError::Daemon("transient failure".into()));
            }
            if self.hold_quota.load(Ordering::SeqCst) {
                self.quota_hit_notify.notify_waiters();
                return Err(NetError::Daemon("429 quota exhausted".into()));
            }
            if let Ok(mut g) = self.done.lock() {
                g.push(batch.clone());
            }
            self.done_notify.notify_waiters();
            Ok(Some(batch.end_byte_offset))
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unit_retry_backoff_paused_quota_and_startup_cursor_recovery() -> TestResult {
    // (a) 1st attempt fails transiently -> 2nd hits quota gate (`PausedQuota` pinned deterministically)
    //     -> gate released -> post-quota retry succeeds!
    let (dir, store) = temp_store("unit-retry")?;
    let sock = unit_socket("rty")?;
    let handler = Arc::new(UnitRetryHandler::new(1, true));
    let daemon = DaemonServer::spawn(
        store,
        DaemonConfig {
            socket_path: sock.clone(),
            idle_extract_window: Duration::from_millis(25),
            idle_shutdown_timeout: None,
            max_frame_bytes: 64 * 1024,
            web_inspector: None,
        },
        Arc::clone(&handler) as Arc<dyn DaemonHandler>,
    )
    .await?;
    let client = DaemonClient::new(&sock)?;
    let alice: ActorId = "alice".parse()?;

    let quota_notified = handler.quota_hit_notify.notified();
    client
        .notify_cursor(CursorNotification {
            session_id: "u_sess_1".into(),
            client_kind: "codex".into(),
            transcript_path: None,
            byte_offset: 400,
            line_count: 4,
            prefix_sha256: "p1".into(),
            actor: alice.clone(),
            at: AT,
            session_ended: true,
        })
        .await?;

    tokio::time::timeout(Duration::from_secs(5), quota_notified).await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let ro = Store::open_readonly(dir.path())?;
            if let Some((ExtractionJobStatus::PausedQuota, attempts, _)) =
                ro.daemon_session_cursor_retry_state("u_sess_1")?
            {
                assert!(attempts >= 2);
                return Ok::<(), Box<dyn Error>>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;

    let done_notified = handler.done_notify.notified();
    handler.hold_quota.store(false, Ordering::SeqCst);
    tokio::time::timeout(Duration::from_secs(5), done_notified).await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if client.status().await?.pending_extractions == 0 {
                return Ok::<(), Box<dyn Error>>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    assert_eq!(client.status().await?.pending_extractions, 0);
    assert!(handler.calls.load(Ordering::SeqCst) >= 3);
    daemon.shutdown().await?;

    // (b) Restart recovery + external `data_version` periodic scan:
    //     unextracted `session_cursors` row is picked up on startup without `NotifyCursor`,
    //     and a later direct SQLite cursor write on another connection is picked up on `scan_tick`.
    let (dir2, mut store2) = temp_store("unit-restart")?;
    store2.daemon_record_session_cursor(&kp_store::SessionCursorUpdate {
        session_id: "u_sess_restart",
        client_kind: "claude-code",
        transcript_path: None,
        byte_offset: 900,
        line_count: 9,
        prefix_sha256: "pr",
        prev_byte_offset: None,
        prev_prefix_sha256: None,
        actor: Some(&alice),
        session_ended: false,
        at: AT,
    })?;
    let sock2 = unit_socket("rst")?;
    let handler2 = Arc::new(UnitRetryHandler::new(0, false));
    let startup_done = handler2.done_notify.notified();
    let daemon2 = DaemonServer::spawn(
        store2,
        DaemonConfig {
            socket_path: sock2.clone(),
            idle_extract_window: Duration::from_millis(25),
            idle_shutdown_timeout: None,
            max_frame_bytes: 64 * 1024,
            web_inspector: None,
        },
        Arc::clone(&handler2) as Arc<dyn DaemonHandler>,
    )
    .await?;
    let client2 = DaemonClient::new(&sock2)?;
    tokio::time::timeout(Duration::from_secs(5), startup_done).await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if client2.status().await?.pending_extractions == 0 {
                return Ok::<(), Box<dyn Error>>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    assert_eq!(handler2.calls.load(Ordering::SeqCst), 1);

    let ext_done = handler2.done_notify.notified();
    {
        let mut external_store = Store::open(dir2.path())?;
        external_store.daemon_record_session_cursor(&kp_store::SessionCursorUpdate {
            session_id: "u_sess_external_dv",
            client_kind: "codex",
            transcript_path: None,
            byte_offset: 600,
            line_count: 6,
            prefix_sha256: "pe",
            prev_byte_offset: None,
            prev_prefix_sha256: None,
            actor: Some(&alice),
            session_ended: true,
            at: AT,
        })?;
    }
    tokio::time::timeout(Duration::from_secs(5), ext_done).await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if client2.status().await?.pending_extractions == 0 {
                return Ok::<(), Box<dyn Error>>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await??;
    assert_eq!(handler2.calls.load(Ordering::SeqCst), 2);
    daemon2.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn query_context_and_pre_tool_check_claude_ask_vs_codex_deny_allow_once() -> TestResult {
    let (dir, mut store) = temp_store("unit-hooks")?;
    let alice: ActorId = "alice".parse()?;
    let bob: ActorId = "bob".parse()?;
    store.add_member(bob.clone(), MemberRole::Maintainer, AT)?;

    // Timestamp 2025-11-20T22:00:00Z (`1763676000000`): UTC date is `2025-11-20`, `+480` is `2025-11-21`.
    let late_utc = EpochMs(1_763_676_000_000);
    let mut bare_id_fact = make_unit_fact(
        "01950000-0000-7000-8000-000000000001",
        "alice",
        FactKind::Constraint,
        &["alice", "bob"],
        late_utc,
        json!({
            "hard_guard": true,
            "command": "gh pr merge",
            "claim_text": "主从切换演练窗口暂停执行 gh pr merge"
        }),
    )?;
    bare_id_fact.subject = "合并门禁".into();
    bare_id_fact.source_anchor = Some(AnchorSpec {
        verbatim_quote: Some("主从切换演练窗口暂停执行 gh pr merge".into()),
        ..AnchorSpec::default()
    });
    store.insert_fact(&bare_id_fact)?;

    let mut second_guard = make_unit_fact(
        "fact_protect_schema",
        "alice",
        FactKind::Constraint,
        &["alice", "bob"],
        AT,
        json!({
            "file_path": "schema.sql",
            "claim_text": "不要直接修改 schema.sql"
        }),
    )?;
    second_guard.subject = "存储结构".into();
    store.insert_fact(&second_guard)?;

    // Also ingest a raw chunk so `QueryContext` verifies chunk `ref_id`s are NOT misreported as `cited_fact_ids`.
    let lib_id = store.library_id().clone();
    store.ingest_source(
        &lib_id,
        &"raw-note".parse()?,
        &RawSourceInput {
            uri: "doc://runbook.md".into(),
            media_type: "text/markdown".into(),
            content: b"# Runbook\n Canary rollout checklist.\n".to_vec(),
            readable_by: AudienceSpec::Explicit([alice.clone(), bob.clone()].into()),
            captured_at: AT,
            chunks: vec![ChunkInput {
                heading: "Runbook".into(),
                body: "Canary rollout checklist.".into(),
                anchor: AnchorSpec::default(),
            }],
            self_echo_filtered: false,
        },
    )?;

    let sock = unit_socket("hks")?;
    let daemon = DaemonServer::spawn(
        store,
        DaemonConfig {
            socket_path: sock.clone(),
            idle_extract_window: Duration::from_secs(60),
            idle_shutdown_timeout: None,
            max_frame_bytes: 64 * 1024,
            web_inspector: None,
        },
        Arc::new(NoopDaemonHandler),
    )
    .await?;
    let client = DaemonClient::new(&sock)?;

    // 1. `QueryContext`: empty prompt, low-signal ack, and non-member viewer return empty result.
    assert_eq!(
        client
            .query_context(QueryContextRequest {
                prompt: "   ".into(),
                viewer: alice.clone(),
                at: AT,
                max_tokens: 250,
                max_items: 3,
                time_zone: Some("UTC".into()),
            })
            .await?,
        QueryContextResult::default()
    );
    assert_eq!(
        client
            .query_context(QueryContextRequest {
                prompt: "好的".into(),
                viewer: alice.clone(),
                at: AT,
                max_tokens: 250,
                max_items: 3,
                time_zone: Some("UTC".into()),
            })
            .await?,
        QueryContextResult::default()
    );
    assert_eq!(
        client
            .query_context(QueryContextRequest {
                prompt: "主从切换演练窗口".into(),
                viewer: "outsider".parse()?,
                at: AT,
                max_tokens: 250,
                max_items: 3,
                time_zone: Some("UTC".into()),
            })
            .await?,
        QueryContextResult::default()
    );

    // IANA timezone (`Asia/Shanghai` vs `UTC`) and `cited_fact_ids` resolution on bare UUID `FactId`.
    let ctx_utc = client
        .query_context(QueryContextRequest {
            prompt: "主从切换演练窗口 gh pr merge".into(),
            viewer: alice.clone(),
            at: AT,
            max_tokens: 250,
            max_items: 3,
            time_zone: Some("UTC".into()),
        })
        .await?;
    assert!(ctx_utc.additional_context.contains("[2025-11-20 · alice"));
    assert!(ctx_utc
        .additional_context
        .contains("kp-ref:fact_01950000-0000-7000-8000-000000000001]"));
    assert_eq!(ctx_utc.cited_fact_ids, vec![bare_id_fact.id.clone()]);

    let ctx_p8 = client
        .query_context(QueryContextRequest {
            prompt: "主从切换演练窗口 gh pr merge".into(),
            viewer: alice.clone(),
            at: AT,
            max_tokens: 250,
            max_items: 3,
            time_zone: Some("Asia/Shanghai".into()),
        })
        .await?;
    assert!(ctx_p8.additional_context.contains("[2025-11-21 · alice"));

    let chunk_only_ctx = client
        .query_context(QueryContextRequest {
            prompt: "Canary rollout checklist".into(),
            viewer: alice.clone(),
            at: AT,
            max_tokens: 250,
            max_items: 3,
            time_zone: Some("UTC".into()),
        })
        .await?;
    assert!(chunk_only_ctx.item_count >= 1);
    assert!(
        chunk_only_ctx.cited_fact_ids.is_empty(),
        "raw chunk hits must not pollute cited_fact_ids"
    );

    // 2. `PreToolCheck`: non-member and unguarded tools return silent default `{}`.
    assert_eq!(
        client
            .pre_tool_check(PreToolCheckRequest {
                tool_name: "Bash".into(),
                tool_input: json!({"command": "gh pr merge 1"}),
                cwd: None,
                viewer: "outsider".parse()?,
                at: AT,
                client_kind: Some("codex".into()),
                time_zone: Some("UTC".into()),
            })
            .await?,
        PreToolCheckResult::default()
    );
    assert_eq!(
        client
            .pre_tool_check(PreToolCheckRequest {
                tool_name: "Read".into(),
                tool_input: json!({"file_path": "schema.sql"}),
                cwd: None,
                viewer: bob.clone(),
                at: AT,
                client_kind: Some("codex".into()),
                time_zone: Some("UTC".into()),
            })
            .await?,
        PreToolCheckResult::default()
    );

    // `git merge-base` and `stdlib.rs` never false-positive through `kp_retrieve::guard`.
    assert_eq!(
        client
            .pre_tool_check(PreToolCheckRequest {
                tool_name: "Bash".into(),
                tool_input: json!({"command": "git -C /srv/repo merge-base HEAD main"}),
                cwd: None,
                viewer: bob.clone(),
                at: AT,
                client_kind: None,
                time_zone: Some("UTC".into()),
            })
            .await?
            .permission_decision,
        None
    );

    // 3. Codex WITHOUT `kp allow` grant -> `Deny` + `allow_refs` + `kp allow <ref>` prompt!
    let codex_deny = client
        .pre_tool_check(PreToolCheckRequest {
            tool_name: "exec_command".into(),
            tool_input: json!({"cmd": "gh pr merge 77 --squash"}),
            cwd: None,
            viewer: bob.clone(),
            at: AT,
            client_kind: Some("codex".into()),
            time_zone: Some("Asia/Shanghai".into()),
        })
        .await?;
    assert_eq!(
        codex_deny.permission_decision,
        Some(ToolPermissionDecision::Deny)
    );
    assert_eq!(codex_deny.matched_fact_ids, vec![bare_id_fact.id.clone()]);
    assert_eq!(
        codex_deny.allow_refs,
        vec!["fact_01950000-0000-7000-8000-000000000001".to_owned()]
    );
    let deny_reason = codex_deny
        .permission_decision_reason
        .as_deref()
        .unwrap_or("");
    assert!(deny_reason.contains("[2025-11-21 · alice"));
    assert!(
        deny_reason.ends_with(
            "\n先问用户；用户同意后，请用户执行 kp allow fact_01950000-0000-7000-8000-000000000001。"
        ),
        "unexpected codex reason: {deny_reason}"
    );
    let codex_hook_json = codex_deny.to_hook_json();
    assert_eq!(
        codex_hook_json["hookSpecificOutput"]["permissionDecision"],
        "deny"
    );

    // 4. Grant a single-use `allow_guard_once` for `bare_id_fact` via `Store::grant_guard_allow_once`:
    //    - Claude Code (`client_kind: None` and `Some("claude-code")`) returns `Ask` AND DOES NOT consume the grant!
    //    - Codex (`client_kind: Some("codex")`) consumes the grant ONCE (returning `{}`), and blocks on the 2nd attempt!
    {
        let mut admin_store = Store::open(dir.path())?;
        admin_store.grant_guard_allow_once(&alice, &bare_id_fact.id, 600_000, AT)?;
    }

    for claude_kind in [None, Some("claude-code".to_owned())] {
        let claude_ask = client
            .pre_tool_check(PreToolCheckRequest {
                tool_name: "Bash".into(),
                tool_input: json!({"command": "gh pr merge 77 --squash"}),
                cwd: None,
                viewer: bob.clone(),
                at: EpochMs(AT.0 + 1_000),
                client_kind: claude_kind,
                time_zone: Some("Asia/Shanghai".into()),
            })
            .await?;
        assert_eq!(
            claude_ask.permission_decision,
            Some(ToolPermissionDecision::Ask)
        );
        assert_eq!(claude_ask.matched_fact_ids, vec![bare_id_fact.id.clone()]);
        assert_eq!(
            claude_ask.allow_refs,
            vec!["fact_01950000-0000-7000-8000-000000000001".to_owned()]
        );
        assert!(!claude_ask
            .permission_decision_reason
            .as_deref()
            .unwrap_or("")
            .contains("kp allow"));
    }

    // First Codex invocation consumes the still-intact single-use grant and passes silently!
    let codex_allowed_once = client
        .pre_tool_check(PreToolCheckRequest {
            tool_name: "exec_command".into(),
            tool_input: json!({"cmd": "gh pr merge 77 --squash"}),
            cwd: None,
            viewer: bob.clone(),
            at: EpochMs(AT.0 + 2_000),
            client_kind: Some(" Codex ".into()),
            time_zone: Some("Asia/Shanghai".into()),
        })
        .await?;
    assert_eq!(codex_allowed_once, PreToolCheckResult::default());
    assert_eq!(codex_allowed_once.to_hook_json(), json!({}));

    // Second Codex invocation immediately hits `Deny` again because the single-use grant was consumed!
    let codex_denied_again = client
        .pre_tool_check(PreToolCheckRequest {
            tool_name: "exec_command".into(),
            tool_input: json!({"cmd": "gh pr merge 77 --squash"}),
            cwd: None,
            viewer: bob.clone(),
            at: EpochMs(AT.0 + 3_000),
            client_kind: Some("codex".into()),
            time_zone: Some("Asia/Shanghai".into()),
        })
        .await?;
    assert_eq!(
        codex_denied_again.permission_decision,
        Some(ToolPermissionDecision::Deny)
    );

    // 5. All-or-nothing multi-hit grant: command triggers BOTH `bare_id_fact` and `fact_protect_schema`,
    //    with a grant ONLY on `bare_id_fact` -> Codex consumes ZERO grants and prompts ONLY for `fact_protect_schema`,
    //    then once `fact_protect_schema` is ALSO granted, atomically consumes BOTH grants!
    {
        let mut admin_store = Store::open(dir.path())?;
        admin_store.grant_guard_allow_once(
            &alice,
            &bare_id_fact.id,
            600_000,
            EpochMs(AT.0 + 4_000),
        )?;
    }
    let partial_deny = client
        .pre_tool_check(PreToolCheckRequest {
            tool_name: "exec_command".into(),
            tool_input: json!({"cmd": "cat schema.sql && gh pr merge 77"}),
            cwd: None,
            viewer: bob.clone(),
            at: EpochMs(AT.0 + 5_000),
            client_kind: Some("codex".into()),
            time_zone: Some("UTC".into()),
        })
        .await?;
    assert_eq!(
        partial_deny.permission_decision,
        Some(ToolPermissionDecision::Deny)
    );
    assert_eq!(partial_deny.matched_fact_ids, vec![second_guard.id.clone()]);
    assert_eq!(
        partial_deny.allow_refs,
        vec!["fact_protect_schema".to_owned()]
    );
    assert!(partial_deny
        .permission_decision_reason
        .as_deref()
        .unwrap_or("")
        .ends_with("\n先问用户；用户同意后，请用户执行 kp allow fact_protect_schema。"));

    // Repeating the same multi-hit check still sees `bare_id_fact` unconsumed (only `fact_protect_schema` missing)!
    let partial_deny_repeat = client
        .pre_tool_check(PreToolCheckRequest {
            tool_name: "exec_command".into(),
            tool_input: json!({"cmd": "cat schema.sql && gh pr merge 77"}),
            cwd: None,
            viewer: bob.clone(),
            at: EpochMs(AT.0 + 5_500),
            client_kind: Some("codex".into()),
            time_zone: Some("UTC".into()),
        })
        .await?;
    assert_eq!(
        partial_deny_repeat.matched_fact_ids,
        vec![second_guard.id.clone()]
    );

    // Granting ONLY the missing 2nd rule (`second_guard`) now allows the combined command ONCE, atomically consuming both!
    {
        let mut admin_store = Store::open(dir.path())?;
        admin_store.grant_guard_allow_once(
            &alice,
            &second_guard.id,
            600_000,
            EpochMs(AT.0 + 6_000),
        )?;
    }
    let multi_allowed = client
        .pre_tool_check(PreToolCheckRequest {
            tool_name: "exec_command".into(),
            tool_input: json!({"cmd": "cat schema.sql && gh pr merge 77"}),
            cwd: None,
            viewer: bob.clone(),
            at: EpochMs(AT.0 + 7_000),
            client_kind: Some("codex".into()),
            time_zone: Some("UTC".into()),
        })
        .await?;
    assert_eq!(multi_allowed, PreToolCheckResult::default());

    // Subsequent execution finds BOTH single-use grants consumed and denies with both `allow_refs`!
    let multi_denied_after = client
        .pre_tool_check(PreToolCheckRequest {
            tool_name: "exec_command".into(),
            tool_input: json!({"cmd": "cat schema.sql && gh pr merge 77"}),
            cwd: None,
            viewer: bob,
            at: EpochMs(AT.0 + 8_000),
            client_kind: Some("codex".into()),
            time_zone: Some("UTC".into()),
        })
        .await?;
    assert_eq!(
        multi_denied_after.permission_decision,
        Some(ToolPermissionDecision::Deny)
    );
    assert_eq!(multi_denied_after.matched_fact_ids.len(), 2);
    assert_eq!(multi_denied_after.allow_refs.len(), 2);

    daemon.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn advisory_lock_blocks_second_daemon_without_unlinking_live_socket_and_releases_on_shutdown(
) -> TestResult {
    let (dir, store) = temp_store("unit-lock-live")?;
    let cfg = DaemonConfig::for_library(dir.path())?;
    let sock = cfg.socket_path.clone();
    let lock_path = daemon_lock_path_for_socket(&sock);

    assert!(!is_daemon_lock_held(dir.path()));
    assert!(!is_socket_lock_held(&sock));

    let daemon = DaemonServer::spawn_default(store, cfg.clone()).await?;
    assert!(lock_path.exists());
    assert!(is_socket_lock_held(&sock));
    assert!(is_daemon_lock_held(dir.path()));

    let inode_before = std::fs::symlink_metadata(&sock)?.ino();

    // Second `DaemonServer::spawn` on the same socket fails immediately via `.lock` without unlinking `sock`.
    let (_dup_dir, dup_store) = temp_store("unit-lock-dup")?;
    assert!(matches!(
        DaemonServer::spawn_default(dup_store, cfg.clone()).await,
        Err(NetError::Invalid("daemon already running on socket"))
    ));
    let inode_after = std::fs::symlink_metadata(&sock)?.ino();
    assert_eq!(inode_before, inode_after);
    assert_eq!(
        DaemonClient::new(&sock)?.ping().await?.0.as_str(),
        "unit-lock-live"
    );

    // Pre-locked `<sock2>.lock` (simulating another OS process) prevents `DaemonServer::spawn` from unlinking `sock2`.
    let sock2 = unit_socket("extlck")?;
    std::fs::write(&sock2, b"sentinel")?;
    let ext_lock_file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(daemon_lock_path_for_socket(&sock2))?;
    ext_lock_file.try_lock()?;
    let (_d3, store3) = temp_store("unit-lock-ext")?;
    assert!(matches!(
        DaemonServer::spawn_default(
            store3,
            DaemonConfig {
                socket_path: sock2.clone(),
                idle_extract_window: Duration::from_secs(60),
                idle_shutdown_timeout: None,
                max_frame_bytes: 64 * 1024,
                web_inspector: None,
            },
        )
        .await,
        Err(NetError::Invalid("daemon already running on socket"))
    ));
    assert_eq!(std::fs::read(&sock2)?, b"sentinel");
    let _ = ext_lock_file.unlock();
    drop(ext_lock_file);
    let _ = std::fs::remove_file(&sock2);

    // Clean shutdown removes `sock` and releases the advisory `.lock` so a successor daemon can start.
    daemon.shutdown().await?;
    assert!(!sock.exists());
    assert!(!is_socket_lock_held(&sock));
    assert!(!is_daemon_lock_held(dir.path()));

    let reopened = Store::open(dir.path())?;
    let successor = DaemonServer::spawn_default(reopened, cfg).await?;
    assert!(is_daemon_lock_held(dir.path()));
    successor.shutdown().await?;
    Ok(())
}

struct IdleLifecycleTestHandler {
    slow_extract_ms: u64,
    fail_first_extract: AtomicBool,
    extract_calls: AtomicUsize,
    drain_calls: AtomicUsize,
    sync_calls: AtomicUsize,
    extract_done_notify: Notify,
}

impl IdleLifecycleTestHandler {
    fn new(slow_extract_ms: u64, fail_first: bool) -> Self {
        Self {
            slow_extract_ms,
            fail_first_extract: AtomicBool::new(fail_first),
            extract_calls: AtomicUsize::new(0),
            drain_calls: AtomicUsize::new(0),
            sync_calls: AtomicUsize::new(0),
            extract_done_notify: Notify::new(),
        }
    }
}

impl DaemonHandler for IdleLifecycleTestHandler {
    fn extract_session_batch<'a>(
        &'a self,
        _library_dir: &'a Path,
        batch: IdleSessionBatch,
    ) -> BoxFuture<'a, Result<Option<u64>>> {
        Box::pin(async move {
            self.extract_calls.fetch_add(1, Ordering::SeqCst);
            if self.slow_extract_ms > 0 {
                tokio::time::sleep(Duration::from_millis(self.slow_extract_ms)).await;
            }
            if self.fail_first_extract.swap(false, Ordering::SeqCst) {
                return Err(NetError::Daemon("transient slow failure".into()));
            }
            self.extract_done_notify.notify_waiters();
            Ok(Some(batch.end_byte_offset))
        })
    }

    fn drain_queued_jobs<'a>(
        &'a self,
        library_dir: &'a Path,
        at: EpochMs,
    ) -> BoxFuture<'a, Result<bool>> {
        let dir = library_dir.to_path_buf();
        Box::pin(async move {
            self.drain_calls.fetch_add(1, Ordering::SeqCst);
            let mut store = Store::open(&dir)?;
            while let Some(job) = store.lease_next_extraction_job(at, 30_000)? {
                store.complete_extraction_job(&job.job_id)?;
            }
            store.set_needs_alignment(true, at)?;
            Ok(false)
        })
    }

    fn sync_and_derive<'a>(
        &'a self,
        library_dir: &'a Path,
        at: EpochMs,
    ) -> BoxFuture<'a, Result<usize>> {
        let dir = library_dir.to_path_buf();
        Box::pin(async move {
            self.sync_calls.fetch_add(1, Ordering::SeqCst);
            let mut store = Store::open(&dir)?;
            if store.needs_alignment()? {
                store.set_needs_alignment(false, at)?;
                return Ok(1);
            }
            Ok(0)
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_shutdown_timeout_drains_queued_jobs_ignores_periodic_ticks_and_waits_out_slow_inflight_and_backoff(
) -> TestResult {
    // 1. Pre-enqueued `extraction_queue` job (zero `session_cursors`) + `communities.json` periodic ticks:
    //    startup drains the queued job, runs `sync_and_derive` for `needs_alignment`, and exits cleanly
    //    after `idle_shutdown_timeout` despite periodic `scan_tick` (50ms) and `sync_tick` (100ms)!
    let (dir1, mut store1) = temp_store("unit-idle-drain")?;
    let lib_id = store1.library_id().clone();
    let alice: ActorId = "alice".parse()?;
    let stored = store1.ingest_source(
        &lib_id,
        &"fallback-note".parse()?,
        &RawSourceInput {
            uri: "session://fallback-1".into(),
            media_type: "text/plain".into(),
            content: b"fallback transcript chunk\n".to_vec(),
            readable_by: AudienceSpec::Explicit([alice.clone()].into()),
            captured_at: AT,
            chunks: vec![ChunkInput {
                heading: "turn".into(),
                body: "fallback transcript chunk".into(),
                anchor: AnchorSpec::default(),
            }],
            self_echo_filtered: true,
        },
    )?;
    store1.enqueue_extraction_job(&kp_store::ExtractionJobInput {
        source_id: stored.source_id,
        chunk_ids: vec!["chunk_0".into()],
        chunk_batch_sha256: kp_model::ContentHash::digest(b"batch_sha_1"),
        author: alice.clone(),
        model_policy: "inherit".into(),
        priority: 10,
        reference_time: AT,
        created_at: AT,
    })?;
    store1.set_needs_alignment(false, AT)?;
    assert_eq!(store1.daemon_queue_stats()?, (0, 1));
    crate::save_joined_communities(dir1.path(), &[])?;

    let sock1 = unit_socket("idl1")?;
    let handler1 = Arc::new(IdleLifecycleTestHandler::new(0, false));
    let mut daemon1 = DaemonServer::spawn(
        store1,
        DaemonConfig {
            socket_path: sock1.clone(),
            idle_extract_window: Duration::from_millis(25),
            idle_shutdown_timeout: None,
            max_frame_bytes: 64 * 1024,
            web_inspector: None,
        }
        .with_idle_shutdown_timeout(Some(Duration::from_millis(260))),
        Arc::clone(&handler1) as Arc<dyn DaemonHandler>,
    )
    .await?;

    tokio::time::timeout(Duration::from_secs(4), daemon1.wait()).await?;
    daemon1.shutdown().await?;
    assert!(handler1.drain_calls.load(Ordering::SeqCst) >= 1);
    assert!(handler1.sync_calls.load(Ordering::SeqCst) >= 1);
    let check1 = Store::open_readonly(dir1.path())?;
    assert_eq!(check1.daemon_queue_stats()?, (0, 0));
    assert!(!check1.needs_alignment()?);

    // 2. Slow in-flight extraction (`170ms > 120ms` idle timeout) + transient failure + retry backoff:
    //    daemon NEVER exits while extraction or retry queue is active, resets `idle_since` on `Ping`
    //    and on extraction completion, and exits cleanly once truly idle!
    let (_dir2, store2) = temp_store("unit-idle-slow")?;
    let sock2 = unit_socket("idl2")?;
    let handler2 = Arc::new(IdleLifecycleTestHandler::new(170, true));
    let done_notified = handler2.extract_done_notify.notified();
    let mut daemon2 = DaemonServer::spawn(
        store2,
        DaemonConfig {
            socket_path: sock2.clone(),
            idle_extract_window: Duration::from_millis(20),
            idle_shutdown_timeout: None,
            max_frame_bytes: 64 * 1024,
            web_inspector: None,
        }
        .with_idle_shutdown_timeout(Some(Duration::from_millis(120))),
        Arc::clone(&handler2) as Arc<dyn DaemonHandler>,
    )
    .await?;
    let client2 = DaemonClient::new(&sock2)?;
    let t_start = tokio::time::Instant::now();
    client2
        .notify_cursor(CursorNotification {
            session_id: "slow_retry_sess".into(),
            client_kind: "codex".into(),
            transcript_path: None,
            byte_offset: 512,
            line_count: 5,
            prefix_sha256: "pslow".into(),
            actor: alice,
            at: AT,
            session_ended: true,
        })
        .await?;

    tokio::time::timeout(Duration::from_secs(5), done_notified).await?;
    assert_eq!(handler2.extract_calls.load(Ordering::SeqCst), 2);
    assert!(t_start.elapsed() >= Duration::from_millis(340));

    // Send `Ping` 60ms after extraction completed -> resets `idle_since` so daemon is still alive!
    tokio::time::sleep(Duration::from_millis(60)).await;
    let _ = client2.ping().await?;

    // Now stay completely quiet -> daemon exits cleanly after ~120ms despite 50ms periodic `scan_tick`!
    tokio::time::timeout(Duration::from_secs(3), daemon2.wait()).await?;
    daemon2.shutdown().await?;
    assert!(!sock2.exists());
    Ok(())
}

struct RecordingIngestHandler {
    ingest_sleep_ms: u64,
    ingest_calls: AtomicUsize,
    drain_calls: AtomicUsize,
    sync_calls: AtomicUsize,
    seen_requests: Mutex<Vec<IngestJsonlRequest>>,
    drain_done_notify: Notify,
    sync_done_notify: Notify,
}

impl RecordingIngestHandler {
    fn new(ingest_sleep_ms: u64) -> Self {
        Self {
            ingest_sleep_ms,
            ingest_calls: AtomicUsize::new(0),
            drain_calls: AtomicUsize::new(0),
            sync_calls: AtomicUsize::new(0),
            seen_requests: Mutex::new(Vec::new()),
            drain_done_notify: Notify::new(),
            sync_done_notify: Notify::new(),
        }
    }
}

impl DaemonHandler for RecordingIngestHandler {
    fn ingest_jsonl<'a>(
        &'a self,
        _library_dir: &'a Path,
        request: IngestJsonlRequest,
    ) -> BoxFuture<'a, Result<IngestJsonlResult>> {
        Box::pin(async move {
            self.ingest_calls.fetch_add(1, Ordering::SeqCst);
            if self.ingest_sleep_ms > 0 {
                tokio::time::sleep(Duration::from_millis(self.ingest_sleep_ms)).await;
            }
            let parsed = kp_model::parse_public_jsonl_str(&request.jsonl)?;
            if let Ok(mut guard) = self.seen_requests.lock() {
                guard.push(request);
            }
            let mut res = IngestJsonlResult::default();
            for item in parsed {
                match item {
                    kp_model::PublicIngestLine::ContainerRoster(_) => {
                        res.rosters_applied += 1;
                    }
                    kp_model::PublicIngestLine::Record(rec) => match rec.op {
                        kp_model::PublicIngestOp::Upsert => {
                            res.records_upserted += 1;
                            res.jobs_enqueued += 1;
                            res.source_ids
                                .push(format!("src_{}", rec.source.external_id).parse()?);
                        }
                        kp_model::PublicIngestOp::Supersede { .. } => {
                            res.records_superseded += 1;
                            res.jobs_enqueued += 1;
                            res.source_ids
                                .push(format!("src_{}", rec.source.external_id).parse()?);
                        }
                        kp_model::PublicIngestOp::Withdraw => {
                            res.records_withdrawn += 1;
                        }
                    },
                }
            }
            Ok(res)
        })
    }

    fn drain_queued_jobs<'a>(
        &'a self,
        _library_dir: &'a Path,
        _at: EpochMs,
    ) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            self.drain_calls.fetch_add(1, Ordering::SeqCst);
            self.drain_done_notify.notify_waiters();
            Ok(false)
        })
    }

    fn sync_and_derive<'a>(
        &'a self,
        _library_dir: &'a Path,
        _at: EpochMs,
    ) -> BoxFuture<'a, Result<usize>> {
        Box::pin(async move {
            let n = self.sync_calls.fetch_add(1, Ordering::SeqCst) + 1;
            if n >= 1 {
                self.sync_done_notify.notify_waiters();
            }
            Ok(0)
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ingest_jsonl_uds_rpc_batches_multiline_guards_oversize_and_wakes_background_drain_and_sync(
) -> TestResult {
    assert_eq!(MAX_DAEMON_FRAME_BYTES, 8 * 1024 * 1024);

    // Verify `docs/INGEST-SCHEMA.md` has zero trailing whitespace, closed `additionalProperties: false`
    // schemas, nullable `model_policy`, `previous_external_id` gated on `supersede`, and clean examples!
    let doc_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/INGEST-SCHEMA.md");
    let doc_text = std::fs::read_to_string(&doc_path)?;
    for (line_no, raw_line) in doc_text.split('\n').enumerate() {
        assert!(
            !raw_line.ends_with(' ') && !raw_line.ends_with('\t') && !raw_line.ends_with('\r'),
            "trailing whitespace in docs/INGEST-SCHEMA.md at line {}",
            line_no + 1
        );
    }
    for required_doc_term in [
        "SecretRedactor",
        "PermissionDenied",
        "synced_at",
        "Conflict",
        "model_polciy",
    ] {
        assert!(
            doc_text.contains(required_doc_term),
            "docs/INGEST-SCHEMA.md missing normative term {required_doc_term}"
        );
    }
    let schema_start = doc_text.find("```json\n").ok_or("missing schema block")? + 8;
    let schema_end = doc_text[schema_start..]
        .find("\n```")
        .ok_or("unclosed schema block")?
        + schema_start;
    let schema_val: serde_json::Value = serde_json::from_str(&doc_text[schema_start..schema_end])?;
    assert_eq!(
        schema_val["$schema"],
        "https://json-schema.org/draft/2020-12/schema"
    );
    let defs = &schema_val["$defs"];
    assert_eq!(
        defs["PublicContainerRoster"]["additionalProperties"],
        json!(false)
    );
    assert_eq!(
        defs["PublicIngestSource"]["additionalProperties"],
        json!(false)
    );
    assert_eq!(
        defs["PublicSpeaker"]["oneOf"][1]["additionalProperties"],
        json!(false)
    );
    assert_eq!(
        defs["PublicAudience"]["oneOf"][1]["additionalProperties"],
        json!(false)
    );
    let policy_branches = defs["ModelRoutePolicy"]["oneOf"]
        .as_array()
        .ok_or("ModelRoutePolicy.oneOf array")?;
    assert_eq!(policy_branches.len(), 3);
    for branch in policy_branches {
        assert_eq!(branch["additionalProperties"], json!(false));
    }
    assert_eq!(
        defs["PublicIngestRecord"]["additionalProperties"],
        json!(false)
    );
    assert_eq!(
        defs["PublicIngestRecord"]["properties"]["content"]["type"],
        json!(["string", "null"])
    );
    assert_eq!(
        defs["PublicIngestRecord"]["properties"]["model_policy"]["oneOf"],
        json!([
            { "type": "null" },
            { "$ref": "#/$defs/ModelRoutePolicy" }
        ])
    );
    let all_of = defs["PublicIngestRecord"]["allOf"]
        .as_array()
        .ok_or("PublicIngestRecord.allOf array")?;
    assert_eq!(all_of.len(), 3);
    assert_eq!(
        all_of[0]["then"]["properties"]["content"],
        json!({ "type": "string", "minLength": 1, "pattern": "\\S" })
    );
    assert_eq!(
        all_of[0]["then"]["not"],
        json!({ "required": ["previous_external_id"] })
    );
    assert_eq!(
        all_of[1]["then"]["required"],
        json!(["content", "previous_external_id"])
    );
    assert_eq!(
        all_of[1]["then"]["properties"]["content"],
        json!({ "type": "string", "minLength": 1, "pattern": "\\S" })
    );
    assert_eq!(
        all_of[2]["then"]["properties"]["content"],
        json!({ "type": ["string", "null"] })
    );
    assert_eq!(
        all_of[2]["then"]["not"],
        json!({ "required": ["previous_external_id"] })
    );
    assert_eq!(
        kp_model::parse_public_jsonl_str(
            r#"{"source":{"kind":"manual","uri":"stdin://w","external_id":"w_null"},"speaker":"alice","audience":{"explicit":["alice"]},"occurred_at":1763683200000,"content":null,"op":"withdraw"}"#,
        )?
        .len(),
        1
    );
    assert!(kp_model::parse_public_jsonl_str(
        r#"{"source":{"kind":"manual","uri":"stdin://u","external_id":"u_null"},"speaker":"alice","audience":{"explicit":["alice"]},"occurred_at":1763683200000,"content":null,"op":"upsert"}"#,
    )
    .is_err());

    let assert_strict_public_jsonl =
        |raw_jsonl: &str| -> TestResult<Vec<kp_model::PublicIngestLine>> {
            for raw in raw_jsonl.lines().map(str::trim).filter(|l| !l.is_empty()) {
                let v: serde_json::Value = serde_json::from_str(raw)?;
                let obj = v.as_object().ok_or("expected JSON object line")?;
                if obj.contains_key("container_id") && !obj.contains_key("source") {
                    for k in obj.keys() {
                        assert!(
                            matches!(
                                k.as_str(),
                                "container_id"
                                    | "label"
                                    | "members"
                                    | "all_library_members"
                                    | "synced_at"
                                    | "provider"
                                    | "upstream_container_id"
                            ),
                            "unknown roster key {k}"
                        );
                    }
                } else {
                    for k in obj.keys() {
                        assert!(
                            matches!(
                                k.as_str(),
                                "source"
                                    | "speaker"
                                    | "audience"
                                    | "occurred_at"
                                    | "content"
                                    | "op"
                                    | "previous_external_id"
                                    | "ingested_by"
                                    | "media_type"
                                    | "model_policy"
                            ),
                            "unknown record key {k}"
                        );
                    }
                    if obj.get("op").and_then(|x| x.as_str()) != Some("supersede") {
                        assert!(
                            !obj.contains_key("previous_external_id"),
                            "previous_external_id forbidden when op != supersede"
                        );
                    }
                    if let Some(src) = obj.get("source").and_then(|x| x.as_object()) {
                        for k in src.keys() {
                            assert!(
                                matches!(
                                    k.as_str(),
                                    "kind" | "uri" | "external_id" | "container_id"
                                ),
                                "unknown source key {k}"
                            );
                        }
                    }
                    if let Some(spk) = obj.get("speaker").and_then(|x| x.as_object()) {
                        for k in spk.keys() {
                            assert!(
                                matches!(k.as_str(), "id" | "display_name"),
                                "unknown speaker key {k}"
                            );
                        }
                    }
                }
            }
            Ok(kp_model::parse_public_jsonl_str(raw_jsonl)?)
        };

    let bash_marker = "cat <<'EOF' | kp --path ./team-library ingest -\n";
    let bash_start = doc_text.find(bash_marker).ok_or("missing bash example")? + bash_marker.len();
    let bash_end = doc_text[bash_start..].find("\nEOF").ok_or("unclosed EOF")? + bash_start;
    let doc_lines = assert_strict_public_jsonl(&doc_text[bash_start..bash_end])?;
    assert_eq!(doc_lines.len(), 5);
    match &doc_lines[1] {
        kp_model::PublicIngestLine::Record(rec) => assert_eq!(rec.model_policy, None),
        _ => return Err("expected record with model_policy: null at index 1".into()),
    }
    let uds_marker = "Request frame:\n\n```json\n";
    let uds_start = doc_text
        .find(uds_marker)
        .ok_or("missing UDS request frame")?
        + uds_marker.len();
    let uds_end = doc_text[uds_start..]
        .find("\n```")
        .ok_or("unclosed UDS request frame")?
        + uds_start;
    let parsed_uds_req: DaemonRequest = serde_json::from_str(&doc_text[uds_start..uds_end])?;
    match parsed_uds_req {
        DaemonRequest::IngestJsonl(inner) => {
            assert_eq!(assert_strict_public_jsonl(&inner.jsonl)?.len(), 2);
        }
        _ => return Err("expected IngestJsonl in UDS doc example".into()),
    }

    // 0. Wire schema tags match `{"op": "ingest_jsonl"}` and `{"status": "ingested"}`.
    let wire_req = serde_json::to_value(DaemonRequest::IngestJsonl(IngestJsonlRequest {
        jsonl: String::new(),
        actor: None,
        at: Some(AT),
        extract_now: false,
    }))?;
    assert_eq!(wire_req["op"], "ingest_jsonl");
    let wire_resp = serde_json::to_value(DaemonResponse::Ingested(IngestJsonlResult::default()))?;
    assert_eq!(wire_resp["status"], "ingested");

    // 1. Default `NoopDaemonHandler` returns clean unconfigured error without breaking existing impls.
    let (_d0, s0) = temp_store("unit-ingest-noop")?;
    let sock0 = unit_socket("ig0")?;
    let d_noop = DaemonServer::spawn_default(
        s0,
        DaemonConfig {
            socket_path: sock0.clone(),
            idle_extract_window: Duration::from_secs(60),
            idle_shutdown_timeout: None,
            max_frame_bytes: MAX_DAEMON_FRAME_BYTES,
            web_inspector: None,
        },
    )
    .await?;
    assert!(matches!(
        DaemonClient::new(&sock0)?
            .ingest_jsonl(IngestJsonlRequest::default())
            .await,
        Err(NetError::Daemon(_))
    ));
    d_noop.shutdown().await?;

    // 2. Tight `max_frame_bytes = 560` auto-splits a multi-line JSONL stream across multiple UDS frames,
    //    defers `extract_now = true` to the final sub-batch, merges results, and rejects single-line oversize!
    let (_d1, s1) = temp_store("unit-ingest-batch")?;
    let sock1 = unit_socket("ig1")?;
    let handler1 = Arc::new(RecordingIngestHandler::new(0));
    let daemon1 = DaemonServer::spawn(
        s1,
        DaemonConfig {
            socket_path: sock1.clone(),
            idle_extract_window: Duration::from_secs(60),
            idle_shutdown_timeout: None,
            max_frame_bytes: 560,
            web_inspector: None,
        },
        Arc::clone(&handler1) as Arc<dyn DaemonHandler>,
    )
    .await?;

    let mut lines = vec![
        r#"{"container_id":"scope_chat_ops","label":"运维协作组","members":["alice"],"all_library_members":false,"synced_at":1763683200000}"#.to_owned(),
    ];
    for i in 1..=6usize {
        lines.push(format!(
            r#"{{"source":{{"kind":"chat","uri":"chat://ops/m{i}","external_id":"m{i}","container_id":"scope_chat_ops"}},"speaker":"alice","audience":"container_members","occurred_at":1763683200000,"content":"变更窗口步骤记录 #{i}","op":"upsert"}}"#
        ));
    }
    lines.push(r#"{"source":{"kind":"chat","uri":"chat://ops/m1_v2","external_id":"m1_v2","container_id":"scope_chat_ops"},"speaker":"alice","audience":"container_members","occurred_at":1763683201000,"content":"变更窗口步骤记录 #1 修订","op":"supersede","previous_external_id":"m1"}"#.into());
    lines.push(r#"{"source":{"kind":"chat","uri":"chat://ops/m2","external_id":"m2","container_id":"scope_chat_ops"},"speaker":"alice","audience":"container_members","occurred_at":1763683202000,"op":"withdraw"}"#.into());
    let stream_jsonl = lines.join("\n");
    assert!(stream_jsonl.len() > 1_500);

    let async_client1 = DaemonClient::new(&sock1)?.with_max_frame_bytes(560);
    let merged_async = async_client1
        .ingest_jsonl(IngestJsonlRequest {
            jsonl: stream_jsonl.clone(),
            actor: None,
            at: Some(AT),
            extract_now: true,
        })
        .await?;
    assert_eq!(merged_async.rosters_applied, 1);
    assert_eq!(merged_async.records_upserted, 6);
    assert_eq!(merged_async.records_superseded, 1);
    assert_eq!(merged_async.records_withdrawn, 1);
    assert_eq!(merged_async.jobs_enqueued, 7);
    assert_eq!(merged_async.source_ids.len(), 7);

    {
        let seen = handler1.seen_requests.lock().map_err(|_| "lock")?;
        assert!(
            seen.len() >= 4,
            "expected multi-frame batch split, got {}",
            seen.len()
        );
        for (idx, sub) in seen.iter().enumerate() {
            assert_eq!(sub.actor.as_ref().map(ActorId::as_str), Some("alice"));
            let is_last = idx + 1 == seen.len();
            assert_eq!(sub.extract_now, is_last);
        }
    }

    // Sync client also auto-splits and merges identically!
    let sync_client1 = DaemonSyncClient::new(&sock1)?
        .with_max_frame_bytes(560)
        .with_timeout(Duration::from_secs(5));
    let merged_sync = sync_client1.ingest_jsonl(IngestJsonlRequest {
        jsonl: stream_jsonl,
        actor: Some("alice".parse()?),
        at: Some(AT),
        extract_now: false,
    })?;
    assert_eq!(merged_sync, merged_async);

    // Single JSONL line exceeding `max_frame_bytes` is rejected cleanly on both async and sync clients.
    let single_huge_line = format!(
        r#"{{"source":{{"kind":"manual","uri":"stdin://huge","external_id":"h1"}},"speaker":"alice","audience":{{"explicit":["alice"]}},"occurred_at":1763683200000,"content":"{}","op":"upsert"}}"#,
        "x".repeat(700)
    );
    assert!(matches!(
        async_client1
            .ingest_jsonl(IngestJsonlRequest {
                jsonl: single_huge_line.clone(),
                actor: None,
                at: None,
                extract_now: false,
            })
            .await,
        Err(NetError::Invalid("daemon frame exceeds max_frame_bytes"))
    ));
    assert!(matches!(
        sync_client1.ingest_jsonl(IngestJsonlRequest {
            jsonl: single_huge_line.clone(),
            actor: None,
            at: None,
            extract_now: false,
        }),
        Err(NetError::Invalid("daemon frame exceeds max_frame_bytes"))
    ));

    // Server-side `read_bounded_line` also rejects raw unsplit frames > server's `max_frame_bytes` without crashing!
    let raw_oversize_resp = DaemonClient::new(&sock1)?
        .request(&DaemonRequest::IngestJsonl(IngestJsonlRequest {
            jsonl: single_huge_line,
            actor: None,
            at: None,
            extract_now: false,
        }))
        .await?;
    assert!(matches!(raw_oversize_resp, DaemonResponse::Error { .. }));

    // Malformed JSONL and non-member `actor` are rejected before invoking `handler.ingest_jsonl`.
    let calls_before = handler1.ingest_calls.load(Ordering::SeqCst);
    assert!(async_client1
        .ingest_jsonl(IngestJsonlRequest {
            jsonl: "{\"not_valid\": 1}".into(),
            actor: None,
            at: None,
            extract_now: false,
        })
        .await
        .is_err());
    assert!(async_client1
        .ingest_jsonl(IngestJsonlRequest {
            jsonl: String::new(),
            actor: Some("outsider".parse()?),
            at: None,
            extract_now: false,
        })
        .await
        .is_err());
    assert_eq!(handler1.ingest_calls.load(Ordering::SeqCst), calls_before);
    daemon1.shutdown().await?;

    // 3. Active `IngestJsonl` (`180ms > 120ms` idle timeout) suspends idle exit while running,
    //    immediately wakes `drain_queued_jobs` + `sync_and_derive` despite a 60s quiet window,
    //    and exits cleanly once all background work completes!
    let (_d2, s2) = temp_store("unit-ingest-wake")?;
    let sock2 = unit_socket("ig2")?;
    let handler2 = Arc::new(RecordingIngestHandler::new(180));
    let drain_fut = handler2.drain_done_notify.notified();
    let sync_fut = handler2.sync_done_notify.notified();
    let mut daemon2 = DaemonServer::spawn(
        s2,
        DaemonConfig {
            socket_path: sock2.clone(),
            idle_extract_window: Duration::from_secs(60),
            idle_shutdown_timeout: Some(Duration::from_millis(120)),
            max_frame_bytes: MAX_DAEMON_FRAME_BYTES,
            web_inspector: None,
        },
        Arc::clone(&handler2) as Arc<dyn DaemonHandler>,
    )
    .await?;

    let one_line = r#"{"source":{"kind":"manual","uri":"stdin://wake","external_id":"w1"},"speaker":"alice","audience":{"explicit":["alice"]},"occurred_at":"2026-11-21T00:00:00Z","content":"立即唤醒后台抽取与对齐","op":"upsert"}"#;
    let res2 = DaemonClient::new(&sock2)?
        .ingest_jsonl(IngestJsonlRequest {
            jsonl: one_line.into(),
            actor: None,
            at: Some(AT),
            extract_now: false,
        })
        .await?;
    assert_eq!(res2.records_upserted, 1);
    assert_eq!(res2.jobs_enqueued, 1);

    tokio::time::timeout(Duration::from_secs(3), drain_fut).await?;
    tokio::time::timeout(Duration::from_secs(3), sync_fut).await?;
    assert!(handler2.drain_calls.load(Ordering::SeqCst) >= 1);
    assert!(handler2.sync_calls.load(Ordering::SeqCst) >= 1);

    tokio::time::timeout(Duration::from_secs(3), daemon2.wait()).await?;
    daemon2.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_peer_cred_uid_verification_blocks_async_and_sync_before_sending_any_bytes(
) -> TestResult {
    use std::io::{Read, Write};
    use std::os::unix::net::{UnixListener as StdUnixListener, UnixStream as StdUnixStream};

    let uid = current_process_uid()?;
    let wrong_uid = uid.wrapping_add(1);

    // 1. Direct `verify_std_stream_peer_uid` check inside Tokio, on a bare OS thread (no Tokio runtime),
    //    and inside a current-thread Tokio runtime built WITHOUT `.enable_io()`, preserving blocking mode!
    let (mut a, mut b) = StdUnixStream::pair()?;
    verify_std_stream_peer_uid(&a, uid)?;
    assert!(matches!(
        verify_std_stream_peer_uid(&a, wrong_uid),
        Err(NetError::Forbidden)
    ));
    a.write_all(b"ping")?;
    let mut four = [0u8; 4];
    b.read_exact(&mut four)?;
    assert_eq!(&four, b"ping");

    let bare_thread_ok = std::thread::spawn(move || -> std::result::Result<(), String> {
        let (mut c1, mut c2) = StdUnixStream::pair().map_err(|e| e.to_string())?;
        verify_std_stream_peer_uid(&c1, uid).map_err(|e| e.to_string())?;
        if !matches!(
            verify_std_stream_peer_uid(&c1, wrong_uid),
            Err(NetError::Forbidden)
        ) {
            return Err("expected Forbidden on mismatched peer UID".into());
        }
        let non_io_rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .map_err(|e| e.to_string())?;
        non_io_rt.block_on(async {
            verify_std_stream_peer_uid(&c1, uid).map_err(|e| e.to_string())?;
            if !matches!(
                verify_std_stream_peer_uid(&c1, wrong_uid),
                Err(NetError::Forbidden)
            ) {
                return Err("expected Forbidden inside non-IO runtime".into());
            }
            Ok::<(), String>(())
        })?;
        c1.write_all(b"ok").map_err(|e| e.to_string())?;
        let mut two = [0u8; 2];
        c2.read_exact(&mut two).map_err(|e| e.to_string())?;
        if &two != b"ok" {
            return Err("unexpected payload".into());
        }
        Ok(())
    })
    .join()
    .map_err(|_| "bare thread panicked")?;
    bare_thread_ok?;

    // 2. Prove BOTH `DaemonClient` (async) and `DaemonSyncClient` (sync) abort with `NetError::Forbidden`
    //    BEFORE writing a single request byte onto the accepted socket!
    let capture_sock = unit_socket("uidcap")?;
    let _ = std::fs::remove_file(&capture_sock);
    let raw_listener = StdUnixListener::bind(&capture_sock)?;
    std::fs::set_permissions(&capture_sock, std::fs::Permissions::from_mode(0o600))?;
    raw_listener.set_nonblocking(true)?;
    let (pair_tx, mut pair_rx) = tokio::sync::mpsc::unbounded_channel::<StdUnixStream>();
    if let Ok(mut guard) = super::server::daemon_socket_registry().lock() {
        guard.insert(capture_sock.clone(), pair_tx);
    }

    let recv_next_accepted_bytes =
        |listener: &StdUnixListener,
         rx: &mut tokio::sync::mpsc::UnboundedReceiver<StdUnixStream>|
         -> TestResult<Vec<u8>> {
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            let mut srv_stream = loop {
                if let Ok((st, _)) = listener.accept() {
                    let _ = st.set_nonblocking(false);
                    break st;
                }
                if let Ok(st) = rx.try_recv() {
                    let _ = st.set_nonblocking(false);
                    break st;
                }
                if std::time::Instant::now() >= deadline {
                    return Err("timed out waiting for client connection".into());
                }
                std::thread::sleep(Duration::from_millis(5));
            };
            let _ = srv_stream.set_read_timeout(Some(Duration::from_millis(500)));
            let mut bytes = Vec::new();
            let _ = srv_stream.read_to_end(&mut bytes)?;
            Ok(bytes)
        };

    // Async client with wrong expected UID -> Forbidden + 0 bytes written to peer!
    let async_bad = DaemonClient::new(&capture_sock)?
        .with_expected_peer_uid_for_test(wrong_uid)
        .ping()
        .await;
    assert!(matches!(async_bad, Err(NetError::Forbidden)));
    let async_bytes = recv_next_accepted_bytes(&raw_listener, &mut pair_rx)?;
    assert!(
        async_bytes.is_empty(),
        "async client wrote {} bytes despite peer UID mismatch",
        async_bytes.len()
    );

    // Sync client (from a bare non-Tokio thread) with wrong expected UID -> Forbidden + 0 bytes written!
    let cap_clone = capture_sock.clone();
    let sync_bad = std::thread::spawn(move || -> Result<()> {
        let sync_cli = DaemonSyncClient::new(&cap_clone)?
            .with_expected_peer_uid_for_test(wrong_uid)
            .with_timeout(Duration::from_millis(500));
        sync_cli.trigger_sync()
    })
    .join()
    .map_err(|_| "sync client thread panicked")?;
    assert!(matches!(sync_bad, Err(NetError::Forbidden)));
    let sync_bytes = recv_next_accepted_bytes(&raw_listener, &mut pair_rx)?;
    assert!(
        sync_bytes.is_empty(),
        "sync client wrote {} bytes despite peer UID mismatch",
        sync_bytes.len()
    );

    drop(raw_listener);
    if let Ok(mut guard) = super::server::daemon_socket_registry().lock() {
        guard.remove(&capture_sock);
    }
    let _ = std::fs::remove_file(&capture_sock);

    // 3. Against a live `DaemonServer`, matching UID succeeds while mismatched UID fails closed on both clients.
    let (_dir, store) = temp_store("unit-client-uid-live")?;
    let live_sock = unit_socket("uidliv")?;
    let daemon = DaemonServer::spawn_default(
        store,
        DaemonConfig {
            socket_path: live_sock.clone(),
            idle_extract_window: Duration::from_secs(60),
            idle_shutdown_timeout: None,
            max_frame_bytes: 64 * 1024,
            web_inspector: None,
        },
    )
    .await?;

    assert_eq!(
        DaemonClient::new(&live_sock)?.ping().await?.0.as_str(),
        "unit-client-uid-live"
    );
    assert!(DaemonSyncClient::new(&live_sock)?.trigger_sync().is_ok());
    assert!(matches!(
        DaemonClient::new(&live_sock)?
            .with_expected_peer_uid_for_test(wrong_uid)
            .ping()
            .await,
        Err(NetError::Forbidden)
    ));
    assert!(matches!(
        DaemonSyncClient::new(&live_sock)?
            .with_expected_peer_uid_for_test(wrong_uid)
            .trigger_sync(),
        Err(NetError::Forbidden)
    ));

    daemon.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_status_reports_observable_embedder_progress_ready_error_and_default_fallback(
) -> TestResult {
    struct DisabledEmbedder;
    impl kp_store::Embedder for DisabledEmbedder {
        fn embed(&self, _text: &str) -> kp_store::Result<Vec<f32>> {
            Ok(Vec::new())
        }
        fn model_id(&self) -> &str {
            "local:bge-small-zh-v1.5"
        }
        fn is_enabled(&self) -> bool {
            false
        }
    }

    struct ObservableEmbedderHandler {
        current: Mutex<DaemonEmbedderRuntimeStatus>,
    }

    impl DaemonHandler for ObservableEmbedderHandler {
        fn embedder_status(&self) -> DaemonEmbedderRuntimeStatus {
            self.current.lock().map(|g| g.clone()).unwrap_or_default()
        }
    }

    // 1. Default `NoopExtractionHandler` falls back to `Store::status().embedding_model` + `store.embedder().is_enabled()`.
    let (_d0, s0) = temp_store("unit-emb-default")?;
    let sock0 = unit_socket("emb0")?;
    let d0 = DaemonServer::spawn_default(
        s0,
        DaemonConfig {
            socket_path: sock0.clone(),
            idle_extract_window: Duration::from_secs(60),
            idle_shutdown_timeout: None,
            max_frame_bytes: 64 * 1024,
            web_inspector: None,
        },
    )
    .await?;
    let st0 = DaemonClient::new(&sock0)?.status().await?;
    assert_eq!(st0.embedder.model, "hash:v1");
    assert_eq!(st0.embedder.state, "ready");
    assert_eq!(st0.embedder.download_pct, None);
    assert_eq!(st0.embedder.last_error, None);
    d0.shutdown().await?;

    // Fallback when store embedder is not yet enabled -> `"fts_fallback"`.
    let (_d_fb, mut s_fb) = temp_store("unit-emb-fallback")?;
    s_fb.set_embedder(Arc::new(DisabledEmbedder));
    let sock_fb = unit_socket("embfb")?;
    let d_fb = DaemonServer::spawn_default(
        s_fb,
        DaemonConfig {
            socket_path: sock_fb.clone(),
            idle_extract_window: Duration::from_secs(60),
            idle_shutdown_timeout: None,
            max_frame_bytes: 64 * 1024,
            web_inspector: None,
        },
    )
    .await?;
    let st_fb = DaemonClient::new(&sock_fb)?.status().await?;
    assert_eq!(st_fb.embedder.model, "local:bge-small-zh-v1.5");
    assert_eq!(st_fb.embedder.state, "fts_fallback");
    d_fb.shutdown().await?;

    // 2. Custom `DaemonHandler::embedder_status` transitions cleanly through `downloading` -> `ready` -> `fts_fallback` error.
    let (_d1, s1) = temp_store("unit-emb-live")?;
    let sock1 = unit_socket("emb1")?;
    let handler = Arc::new(ObservableEmbedderHandler {
        current: Mutex::new(DaemonEmbedderRuntimeStatus {
            model: "local:bge-small-zh-v1.5".into(),
            state: "downloading".into(),
            download_pct: Some(42),
            last_error: None,
        }),
    });
    let d1 = DaemonServer::spawn(
        s1,
        DaemonConfig {
            socket_path: sock1.clone(),
            idle_extract_window: Duration::from_secs(60),
            idle_shutdown_timeout: None,
            max_frame_bytes: 64 * 1024,
            web_inspector: None,
        },
        Arc::clone(&handler) as Arc<dyn DaemonHandler>,
    )
    .await?;

    let cli1 = DaemonClient::new(&sock1)?;
    let downloading = cli1.status().await?;
    assert_eq!(
        downloading.embedder,
        DaemonEmbedderRuntimeStatus {
            model: "local:bge-small-zh-v1.5".into(),
            state: "downloading".into(),
            download_pct: Some(42),
            last_error: None,
        }
    );

    {
        let mut guard = handler.current.lock().map_err(|_| "lock")?;
        guard.state = "ready".into();
        guard.download_pct = Some(100);
    }
    let ready = cli1.status().await?;
    assert_eq!(ready.embedder.state, "ready");
    assert_eq!(ready.embedder.download_pct, Some(100));
    assert_eq!(ready.embedder.last_error, None);

    {
        let mut guard = handler.current.lock().map_err(|_| "lock")?;
        guard.state = "fts_fallback".into();
        guard.download_pct = None;
        guard.last_error = Some("sha256 verification failed".into());
    }
    let failed = cli1.status().await?;
    assert_eq!(failed.embedder.state, "fts_fallback");
    assert_eq!(failed.embedder.download_pct, None);
    assert_eq!(
        failed.embedder.last_error.as_deref(),
        Some("sha256 verification failed")
    );

    // 3. Backward-compatible serde deserialization when `"embedder"` is absent on wire.
    let mut legacy_json = serde_json::to_value(&ready)?;
    if let Some(obj) = legacy_json.as_object_mut() {
        obj.remove("embedder");
    }
    let decoded_legacy: DaemonStatus = serde_json::from_value(legacy_json)?;
    assert_eq!(
        decoded_legacy.embedder,
        DaemonEmbedderRuntimeStatus::default()
    );

    d1.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_rpcs_refresh_store_embedder_across_external_a_to_b_to_c_migrations() -> TestResult {
    #[derive(Clone)]
    struct AxisEmbedder {
        id: &'static str,
        axis: usize,
        enabled: bool,
    }

    impl kp_store::Embedder for AxisEmbedder {
        fn embed(&self, text: &str) -> kp_store::Result<Vec<f32>> {
            let mut v = vec![0.0_f32; 8];
            if text.contains("发票核销") || text.contains("费用报销") {
                v[self.axis] = 1.0;
            } else {
                v[7] = 1.0;
            }
            Ok(v)
        }

        fn model_id(&self) -> &str {
            self.id
        }

        fn expected_dims(&self) -> Option<usize> {
            Some(8)
        }

        fn is_enabled(&self) -> bool {
            self.enabled
        }
    }

    struct RefreshingEmbedderHandler {
        b_ready: AtomicBool,
        refresh_calls: AtomicUsize,
    }

    impl DaemonHandler for RefreshingEmbedderHandler {
        fn refresh_store_embedder(&self, _library_dir: &Path, store: &mut Store) {
            self.refresh_calls.fetch_add(1, Ordering::SeqCst);
            let Ok((meta_model, _)) = store.embedding_config() else {
                return;
            };
            let next: Arc<dyn kp_store::Embedder + Send + Sync> = match meta_model.as_str() {
                "local:test-a" => Arc::new(AxisEmbedder {
                    id: "local:test-a",
                    axis: 0,
                    enabled: true,
                }),
                "local:test-b" => Arc::new(AxisEmbedder {
                    id: "local:test-b",
                    axis: 1,
                    enabled: self.b_ready.load(Ordering::SeqCst),
                }),
                "local:test-c" => Arc::new(AxisEmbedder {
                    id: "local:test-c",
                    axis: 2,
                    enabled: true,
                }),
                _ => return,
            };
            store.set_embedder(next);
        }
    }

    let dir = tempfile::tempdir()?;
    let mut store = Store::init_with_embedding(
        dir.path(),
        InitOptions {
            library_id: "unit-refresh-abc".parse()?,
            name: "unit-refresh-abc".into(),
            mode: LibraryMode::Personal,
            owner: "alice".parse()?,
            created_at: EpochMs(0),
        },
        "local:test-a",
        8,
    )?;
    let emb_a = AxisEmbedder {
        id: "local:test-a",
        axis: 0,
        enabled: true,
    };
    store.set_embedder(Arc::new(emb_a));

    // Insert a source + fact whose text shares ZERO lexical overlap with the query prompt so only
    // matching `model_id` vector similarity can retrieve it!
    let lib_id = store.library_id().clone();
    let stored_src = store.ingest_source(
        &lib_id,
        &"src_reimburse_doc".parse()?,
        &RawSourceInput {
            uri: "doc://finance/reimburse".into(),
            media_type: "text/plain".into(),
            content: "差旅住宿发票核销单".as_bytes().to_vec(),
            readable_by: AudienceSpec::Explicit(BTreeSet::from(["alice".parse()?])),
            captured_at: AT,
            chunks: vec![ChunkInput {
                heading: String::new(),
                body: "差旅住宿发票核销单".into(),
                anchor: AnchorSpec::default(),
            }],
            self_echo_filtered: false,
        },
    )?;
    let mut semantic_fact = make_unit_fact(
        "fact_reimburse_rule",
        "alice",
        FactKind::Observation,
        &["alice"],
        AT,
        json!("差旅住宿发票核销单"),
    )?;
    semantic_fact.subject = "住宿制度".into();
    semantic_fact.predicate = "kp.core.note".into();
    semantic_fact.source_anchor = Some(AnchorSpec {
        source_id: Some(stored_src.source_id),
        start_byte: Some(0),
        end_byte: Some(27),
        start_line: Some(1),
        end_line: Some(1),
        verbatim_quote: Some("差旅住宿发票核销单".into()),
        ..AnchorSpec::default()
    });
    store.insert_fact(&semantic_fact)?;

    let sock = unit_socket("rfabc")?;
    let handler = Arc::new(RefreshingEmbedderHandler {
        b_ready: AtomicBool::new(false),
        refresh_calls: AtomicUsize::new(0),
    });
    let daemon = DaemonServer::spawn(
        store,
        DaemonConfig {
            socket_path: sock.clone(),
            idle_extract_window: Duration::from_secs(60),
            idle_shutdown_timeout: None,
            max_frame_bytes: 64 * 1024,
            web_inspector: None,
        },
        Arc::clone(&handler) as Arc<dyn DaemonHandler>,
    )
    .await?;
    let client = DaemonClient::new(&sock)?;

    let query_req = QueryContextRequest {
        prompt: "出差酒店费用报销申请".into(),
        viewer: "alice".parse()?,
        at: AT,
        max_tokens: 250,
        max_items: 3,
        time_zone: Some("UTC".into()),
    };

    // Stage A: initial `local:test-a` hits via vector similarity.
    let ctx_a = client.query_context(query_req.clone()).await?;
    assert_eq!(ctx_a.cited_fact_ids, vec![semantic_fact.id.clone()]);
    assert_eq!(client.status().await?.embedder.model, "local:test-a");

    // External `kp embed --stale --to local:test-b` migrates DB vectors while daemon stays running.
    {
        let mut ext_store = Store::open(dir.path())?;
        let emb_b = AxisEmbedder {
            id: "local:test-b",
            axis: 1,
            enabled: true,
        };
        let rep_b = ext_store.reembed_stale(&emb_b, Some(("local:test-b", 8)))?;
        assert_eq!(rep_b.model_id, "local:test-b");
        assert!(rep_b.facts_reembedded >= 1);
    }

    // While `b_ready == false` (background download in progress), Status reports `fts_fallback` and
    // QueryContext degrades cleanly to FTS5 (0 lexical hits, no error).
    let st_b_dl = client.status().await?;
    assert_eq!(st_b_dl.embedder.model, "local:test-b");
    assert_eq!(st_b_dl.embedder.state, "fts_fallback");
    let ctx_b_dl = client.query_context(query_req.clone()).await?;
    assert!(ctx_b_dl.cited_fact_ids.is_empty());

    // Once `b_ready` flips to `true`, read-side `refresh_store_embedder` immediately switches to `local:test-b`!
    handler.b_ready.store(true, Ordering::SeqCst);
    let st_b_ok = client.status().await?;
    assert_eq!(st_b_ok.embedder.model, "local:test-b");
    assert_eq!(st_b_ok.embedder.state, "ready");
    let ctx_b_ok = client.query_context(query_req.clone()).await?;
    assert_eq!(ctx_b_ok.cited_fact_ids, vec![semantic_fact.id.clone()]);

    // Second external migration `B -> C` (`local:test-c`) also takes effect on the very next read RPC!
    {
        let mut ext_store = Store::open(dir.path())?;
        let emb_c = AxisEmbedder {
            id: "local:test-c",
            axis: 2,
            enabled: true,
        };
        let rep_c = ext_store.reembed_stale(&emb_c, Some(("local:test-c", 8)))?;
        assert_eq!(rep_c.model_id, "local:test-c");
        assert!(rep_c.facts_reembedded >= 1);
    }

    let calls_before_ptc = handler.refresh_calls.load(Ordering::SeqCst);
    let _ = client
        .pre_tool_check(PreToolCheckRequest {
            tool_name: "exec_command".into(),
            tool_input: json!({"cmd": "ls"}),
            cwd: None,
            viewer: "alice".parse()?,
            at: AT,
            client_kind: Some("claude-code".into()),
            time_zone: Some("UTC".into()),
        })
        .await?;
    assert!(handler.refresh_calls.load(Ordering::SeqCst) > calls_before_ptc);

    let st_c = client.status().await?;
    assert_eq!(st_c.embedder.model, "local:test-c");
    assert_eq!(st_c.embedder.state, "ready");
    let ctx_c = client.query_context(query_req.clone()).await?;
    assert_eq!(ctx_c.cited_fact_ids, vec![semantic_fact.id.clone()]);

    daemon.shutdown().await?;

    // Negative control: a daemon WITHOUT `refresh_store_embedder` stuck on `local:test-a` misses `local:test-c` vectors.
    let mut stale_store = Store::open(dir.path())?;
    stale_store.set_embedder(Arc::new(AxisEmbedder {
        id: "local:test-a",
        axis: 0,
        enabled: true,
    }));
    let sock_stale = unit_socket("rfstl")?;
    let d_stale = DaemonServer::spawn_default(
        stale_store,
        DaemonConfig {
            socket_path: sock_stale.clone(),
            idle_extract_window: Duration::from_secs(60),
            idle_shutdown_timeout: None,
            max_frame_bytes: 64 * 1024,
            web_inspector: None,
        },
    )
    .await?;
    let ctx_stale = DaemonClient::new(&sock_stale)?
        .query_context(query_req)
        .await?;
    assert!(ctx_stale.cited_fact_ids.is_empty());
    d_stale.shutdown().await?;

    Ok(())
}

struct TestHttpResp {
    status: u16,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

async fn send_inspector_http(
    port: u16,
    method: &str,
    path_and_query: &str,
    extra_headers: &[(&str, &str)],
) -> TestResult<TestHttpResp> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(3))
        .build()?;
    let req_method = reqwest::Method::from_bytes(method.as_bytes())?;
    let url = format!("http://127.0.0.1:{port}{path_and_query}");
    let mut builder = client.request(req_method, &url);
    for &(k, v) in extra_headers {
        builder = builder.header(k, v);
    }

    match builder.send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let mut headers = BTreeMap::new();
            for (k, v) in resp.headers() {
                if let Ok(s) = v.to_str() {
                    headers.insert(k.as_str().to_ascii_lowercase(), s.to_owned());
                }
            }
            let body = resp.bytes().await?.to_vec();
            return Ok(TestHttpResp {
                status,
                headers,
                body,
            });
        }
        Err(err) if err.is_connect() => {}
        Err(err) => return Err(err.into()),
    }

    let state = super::web::inproc_inspector_registry()
        .lock()
        .map_err(|_| "inspector registry lock")?
        .get(&port)
        .cloned()
        .ok_or("missing inproc inspector state for port")?;

    let mut req_builder = axum::http::Request::builder()
        .method(method)
        .uri(path_and_query);
    for &(k, v) in extra_headers {
        req_builder = req_builder.header(k, v);
    }
    let req = req_builder.body(axum::body::Body::empty())?;
    let resp = super::web::handle_web_inspector_request(state, req).await;
    let status = resp.status().as_u16();
    let mut headers = BTreeMap::new();
    for (k, v) in resp.headers() {
        if let Ok(s) = v.to_str() {
            headers.insert(k.as_str().to_ascii_lowercase(), s.to_owned());
        }
    }
    let bytes = axum::body::to_bytes(resp.into_body(), 4 * 1024 * 1024).await?;
    Ok(TestHttpResp {
        status,
        headers,
        body: bytes.to_vec(),
    })
}

fn assert_inspector_security_headers(resp: &TestHttpResp) {
    assert_eq!(
        resp.headers
            .get("content-security-policy")
            .map(String::as_str),
        Some(INSPECTOR_CSP)
    );
    let forbidden_csp_directive = concat!("un", "safe-inline");
    assert!(
        !resp
            .headers
            .get("content-security-policy")
            .is_none_or(|v| v.contains(forbidden_csp_directive)),
        "CSP must never allow inline script/style directives"
    );
    assert_eq!(
        resp.headers
            .get("x-content-type-options")
            .map(String::as_str),
        Some("nosniff")
    );
    assert_eq!(
        resp.headers.get("x-frame-options").map(String::as_str),
        Some("DENY")
    );
    assert_eq!(
        resp.headers.get("referrer-policy").map(String::as_str),
        Some("no-referrer")
    );
    assert_eq!(
        resp.headers.get("cache-control").map(String::as_str),
        Some("no-store")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn web_inspector_enforces_loopback_policy_security_headers_tokens_and_four_column_isolation(
) -> TestResult {
    // 1. Non-`127.0.0.1` addresses (`0.0.0.0`, `::`, `::1`, LAN IP, `127.0.0.2`) fail immediately with `NetError::Policy`!
    for bad_addr in [
        "0.0.0.0:0",
        "[::]:0",
        "[::1]:0",
        "192.168.1.25:8080",
        "127.0.0.2:8080",
    ] {
        let parsed: std::net::SocketAddr = bad_addr.parse()?;
        assert!(matches!(
            validate_loopback_inspector_addr(parsed),
            Err(NetError::Policy(_))
        ));
    }

    for bad_spawn_addr in ["0.0.0.0:0", "[::1]:0"] {
        let (_bad_dir, bad_store) = temp_store("unit-web-bad-bind")?;
        let bad_sock = unit_socket("webbad")?;
        let bad_res = DaemonServer::spawn_default(
            bad_store,
            DaemonConfig {
                socket_path: bad_sock.clone(),
                idle_extract_window: Duration::from_secs(60),
                idle_shutdown_timeout: None,
                max_frame_bytes: 64 * 1024,
                web_inspector: Some(WebInspectorConfig {
                    bind_addr: bad_spawn_addr.parse()?,
                    viewer: "alice".parse()?,
                }),
            },
        )
        .await;
        assert!(matches!(bad_res, Err(NetError::Policy(_))));
        assert!(!is_socket_lock_held(&bad_sock));
    }

    // 2. Build a rich multi-member library (`alice`, `carol`, `dave`) exercising all 4 columns + lifecycle transitions.
    let (dir, mut store) = temp_store("unit-web-4col")?;
    let alice: ActorId = "alice".parse()?;
    let carol: ActorId = "carol".parse()?;
    let dave: ActorId = "dave".parse()?;
    let _ = store.add_member(carol.clone(), MemberRole::Member, AT)?;
    let _ = store.add_member(dave.clone(), MemberRole::Member, AT)?;

    // Ingest a Chinese source chunk readable ONLY by `alice` with rich prefix + highlighted quote + suffix.
    let lib_id = store.library_id().clone();
    let cn_body = "会议背景说明：为了验证核心存储集群的容灾切换能力，定于周四凌晨两点执行主从切换演练，演练期间请值班同学全程盯盘观察延迟指标。";
    let cn_quote = "周四凌晨两点执行主从切换演练";
    let stored_src = store.ingest_source(
        &lib_id,
        &"src_cutover_plan".parse()?,
        &RawSourceInput {
            uri: "doc://ops/cutover-plan.md".into(),
            media_type: "text/markdown".into(),
            content: cn_body.as_bytes().to_vec(),
            readable_by: AudienceSpec::Explicit(BTreeSet::from([alice.clone()])),
            captured_at: AT,
            chunks: vec![ChunkInput {
                heading: "主从演练安排".into(),
                body: cn_body.into(),
                anchor: AnchorSpec::default(),
            }],
            self_echo_filtered: false,
        },
    )?;

    let mut premise_event = make_unit_fact(
        "fact_alice_cutover_event",
        "alice",
        FactKind::Event,
        &["alice"],
        AT,
        json!({"claim_text": "周四凌晨两点执行主从切换演练"}),
    )?;
    premise_event.subject = "主从切换演练".into();
    premise_event.source_anchor = Some(AnchorSpec {
        source_id: Some(stored_src.source_id),
        verbatim_quote: Some(cn_quote.into()),
        ..AnchorSpec::default()
    });
    store.insert_fact(&premise_event)?;

    let mut premise_guard = make_unit_fact(
        "fact_alice_freeze_guard",
        "alice",
        FactKind::Constraint,
        &["alice"],
        AT,
        json!({"claim_text": "主从切换窗口暂停合并非回滚分支"}),
    )?;
    premise_guard.subject = "合并冻结规矩".into();
    store.insert_fact(&premise_guard)?;

    let derived_warning = kp_model::derived_fact(
        &lib_id,
        &alice,
        FactKind::CollisionWarning,
        "主从切换碰撞提醒",
        "kp.core.collision",
        json!({"claim_text": "主从切换演练期间命中分支冻结规矩"}),
        AT,
    )?;
    let warning_proof = kp_model::justification(
        &derived_warning,
        kp_model::DerivationMode::HardRule,
        "R_COLLISION_M4",
        &[&premise_event, &premise_guard],
    )?;
    store.save_derivations(std::slice::from_ref(&derived_warning), &[warning_proof])?;

    // Lifecycle timeline fixtures for `alice` (supersede + withdraw + expire + stale_premise):
    let old_slot_fact = make_unit_fact(
        "fact_alice_old_window",
        "alice",
        FactKind::Observation,
        &["alice"],
        AT,
        json!("旧演练时长三十分钟"),
    )?;
    store.insert_fact(&old_slot_fact)?;
    let new_slot_fact = make_unit_fact(
        "fact_alice_new_window",
        "alice",
        FactKind::Observation,
        &["alice"],
        EpochMs(AT.0 + 1_000),
        json!("新演练时长四十五分钟"),
    )?;
    store.supersede(&old_slot_fact.id, &new_slot_fact)?;

    let withdrawn_base = make_unit_fact(
        "fact_alice_withdrawn_premise",
        "alice",
        FactKind::Observation,
        &["alice"],
        AT,
        json!("临时灰度开关草案"),
    )?;
    store.insert_fact(&withdrawn_base)?;
    let stale_conclusion = kp_model::derived_fact(
        &lib_id,
        &alice,
        FactKind::DerivedFact,
        "灰度临时建议",
        "kp.core.advice",
        json!("启用旧版灰度观察哨"),
        AT,
    )?;
    let stale_proof = kp_model::justification(
        &stale_conclusion,
        kp_model::DerivationMode::AssumedPremise,
        "R_ASSUMED_M4",
        &[&withdrawn_base],
    )?;
    store.save_derivations(std::slice::from_ref(&stale_conclusion), &[stale_proof])?;
    let _ = store.forget(withdrawn_base.id.as_str(), &alice, EpochMs(AT.0 + 2_000))?;

    let mut expiring_fact = make_unit_fact(
        "fact_alice_expired_notice",
        "alice",
        FactKind::Event,
        &["alice"],
        AT,
        json!("短时封网窗口已结束"),
    )?;
    expiring_fact.access = FactAccess::Base {
        audience: AudienceSpec::Explicit(BTreeSet::from([alice.clone()])),
        stamp: BitemporalStamp {
            recorded_at: AT,
            expired_at: None,
            valid: ValidityWindow {
                valid_from: AT,
                valid_until: EpochMs(AT.0 + 3_000),
            },
        },
    };
    store.insert_fact(&expiring_fact)?;

    // XSS payload fact visible ONLY to `alice`.
    let xss_payload = "<script>alert(1)</script><img src=x onerror=alert(2)>";
    let mut xss_fact = make_unit_fact(
        "fact_alice_xss_probe",
        "alice",
        FactKind::Observation,
        &["alice"],
        AT,
        json!(xss_payload),
    )?;
    xss_fact.subject = xss_payload.into();
    store.insert_fact(&xss_fact)?;

    // Open `kp review` item belonging ONLY to `alice`.
    let alice_review = kp_model::ReviewItem {
        id: "rev_alice_m4_01".parse()?,
        actor: alice.clone(),
        kind: kp_model::ReviewKind::WritebackProposal,
        target_id: premise_guard.id.to_string(),
        question: "确认是否将导出笔记中的自由段落写入记忆库？".into(),
        options: vec!["confirm".into(), "dismiss".into()],
        payload: json!({"proposal_op": "freeform_block"}),
        status: kp_model::ReviewStatus::Open,
        created_at: AT,
        resolved_at: None,
    };
    let _ = store.upsert_open_review_item(&alice_review)?;

    // One fact visible to `carol` so `carol` has her own clean isolated view.
    let mut carol_public = make_unit_fact(
        "fact_carol_own_note",
        "carol",
        FactKind::Observation,
        &["carol"],
        AT,
        json!("Carol 个人只读观察记录"),
    )?;
    carol_public.subject = "Carol 笔记".into();
    store.insert_fact(&carol_public)?;

    let eval_at = EpochMs(AT.0 + 10_000);

    // 3. Spawn daemon with tight `idle_shutdown_timeout = 90ms`, then attach Web Inspector dynamically via UDS RPC!
    let sock = unit_socket("web4")?;
    let daemon = DaemonServer::spawn_default(
        store,
        DaemonConfig {
            socket_path: sock.clone(),
            idle_extract_window: Duration::from_secs(60),
            idle_shutdown_timeout: Some(Duration::from_millis(90)),
            max_frame_bytes: 64 * 1024,
            web_inspector: None,
        },
    )
    .await?;
    assert!(daemon.web_inspector_url().is_none());
    assert!(daemon.web_inspector_addr().is_none());

    let uds_client = DaemonClient::new(&sock)?;
    let uds_sync = DaemonSyncClient::new(&sock)?;

    // Non-member viewer is rejected on `StartWebInspector`.
    assert!(uds_client
        .start_web_inspector(0, Some("outsider".parse()?))
        .await
        .is_err());

    let alice_sess = uds_client.start_web_inspector(0, None).await?;
    assert_eq!(alice_sess.viewer, alice);
    let carol_sess = uds_sync.start_web_inspector(0, Some(carol.clone()))?;
    assert_eq!(carol_sess.viewer, carol);
    let dave_sess = uds_client
        .start_web_inspector(0, Some(dave.clone()))
        .await?;
    assert_eq!(dave_sess.viewer, dave);

    assert_eq!(alice_sess.bind_addr, carol_sess.bind_addr);
    assert_eq!(alice_sess.bind_addr, dave_sess.bind_addr);
    let bound_addr: std::net::SocketAddr = alice_sess.bind_addr.parse()?;
    assert_eq!(
        bound_addr.ip(),
        std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
    );
    let port = bound_addr.port();

    assert_eq!(daemon.web_inspector_addr(), Some(bound_addr));
    assert_eq!(
        daemon.web_inspector_url().as_deref(),
        Some(alice_sess.url.as_str())
    );
    let d_status = uds_client.status().await?;
    assert_eq!(
        d_status.web_inspector_url.as_deref(),
        Some(alice_sess.url.as_str())
    );
    assert_eq!(d_status.web_inspector_viewer, Some(alice.clone()));

    let extract_tok = |url_str: &str| -> TestResult<String> {
        let parsed = reqwest::Url::parse(url_str)?;
        let tok = parsed
            .query_pairs()
            .find(|(k, _)| k == "token")
            .map(|(_, v)| v.into_owned())
            .ok_or("missing ?token=")?;
        assert_eq!(tok.len(), 64);
        assert!(tok.bytes().all(|b| b.is_ascii_hexdigit()));
        Ok(tok)
    };
    let alice_tok = extract_tok(&alice_sess.url)?;
    let carol_tok = extract_tok(&carol_sess.url)?;
    let dave_tok = extract_tok(&dave_sess.url)?;
    assert_ne!(alice_tok, carol_tok);
    assert_ne!(alice_tok, dave_tok);

    // Web Inspector suppresses `idle_shutdown_timeout` (> 2x 90ms) while active!
    tokio::time::sleep(Duration::from_millis(210)).await;
    assert!(!*daemon.subscribe_shutdown().borrow());

    // 4. Security gates: 401 unauthenticated, 403 wrong Origin, 405 mutating verb, 303 cookie bootstrap, strict CSP!
    let unauth_root = send_inspector_http(port, "GET", "/", &[]).await?;
    assert_eq!(unauth_root.status, 401);
    assert_inspector_security_headers(&unauth_root);

    let unauth_api = send_inspector_http(port, "GET", "/api/v1/overview", &[]).await?;
    assert_eq!(unauth_api.status, 401);
    assert_inspector_security_headers(&unauth_api);

    let bad_tok_boot = send_inspector_http(
        port,
        "GET",
        "/?token=0000000000000000000000000000000000000000000000000000000000000000",
        &[],
    )
    .await?;
    assert_eq!(bad_tok_boot.status, 401);
    assert_inspector_security_headers(&bad_tok_boot);

    let boot_resp = send_inspector_http(port, "GET", &format!("/?token={alice_tok}"), &[]).await?;
    assert_eq!(boot_resp.status, 303);
    assert_inspector_security_headers(&boot_resp);
    assert_eq!(
        boot_resp.headers.get("location").map(String::as_str),
        Some("/")
    );
    let set_cookie = boot_resp
        .headers
        .get("set-cookie")
        .ok_or("missing Set-Cookie")?
        .clone();
    assert!(set_cookie.contains(&format!("{INSPECTOR_COOKIE_NAME}={alice_tok}")));
    assert!(set_cookie.contains("HttpOnly"));
    assert!(set_cookie.contains("SameSite=Strict"));
    assert!(set_cookie.contains("Path=/"));

    let alice_cookie = format!("{INSPECTOR_COOKIE_NAME}={alice_tok}");
    let carol_cookie = format!("{INSPECTOR_COOKIE_NAME}={carol_tok}");
    let dave_cookie = format!("{INSPECTOR_COOKIE_NAME}={dave_tok}");

    let bad_origin = send_inspector_http(
        port,
        "GET",
        "/api/v1/overview",
        &[("Cookie", &alice_cookie), ("Origin", "http://evil.example")],
    )
    .await?;
    assert_eq!(bad_origin.status, 403);
    assert_inspector_security_headers(&bad_origin);

    let good_origin_str = format!("http://127.0.0.1:{port}");
    let post_resp = send_inspector_http(
        port,
        "POST",
        "/api/v1/overview",
        &[("Cookie", &alice_cookie), ("Origin", &good_origin_str)],
    )
    .await?;
    assert_eq!(post_resp.status, 405);
    assert_inspector_security_headers(&post_resp);

    // 5. Static frontend assets (`/` + `/styles.css` + `/app.js`) obey strict CSP and zero-XSS rules.
    let html_resp = send_inspector_http(port, "GET", "/", &[("Cookie", &alice_cookie)]).await?;
    assert_eq!(html_resp.status, 200);
    assert_inspector_security_headers(&html_resp);
    let html_str = String::from_utf8(html_resp.body)?;
    assert!(html_str.contains(r#"<link rel="stylesheet" href="/styles.css">"#));
    assert!(html_str.contains(r#"<script src="/app.js" defer></script>"#));
    assert!(!html_str.contains("<style"));
    assert!(!html_str.contains("style="));

    let css_resp =
        send_inspector_http(port, "GET", "/styles.css", &[("Cookie", &alice_cookie)]).await?;
    assert_eq!(css_resp.status, 200);
    assert_inspector_security_headers(&css_resp);

    let js_resp = send_inspector_http(port, "GET", "/app.js", &[("Cookie", &alice_cookie)]).await?;
    assert_eq!(js_resp.status, 200);
    assert_inspector_security_headers(&js_resp);
    let js_str = String::from_utf8(js_resp.body)?;
    for forbidden_dom_sink in [
        "innerHTML",
        "outerHTML",
        "insertAdjacentHTML",
        "document.write",
    ] {
        assert!(
            !js_str.contains(forbidden_dom_sink),
            "/app.js must never contain DOM XSS sink `{forbidden_dom_sink}`"
        );
    }
    assert!(js_str.contains("document.createElement"));
    assert!(js_str.contains("textContent"));

    // 6. Alice's 4-column `/api/v1/overview` & `/api/v1/why/{id}` return full facts, Chinese quote highlights,
    //    `ProofTree`, all 4 timeline transition categories, and open `kp review` items!
    let alice_ov_resp = send_inspector_http(
        port,
        "GET",
        &format!("/api/v1/overview?at={}", eval_at.0),
        &[("Cookie", &alice_cookie), ("Origin", &good_origin_str)],
    )
    .await?;
    assert_eq!(alice_ov_resp.status, 200);
    assert_inspector_security_headers(&alice_ov_resp);
    let alice_ov: serde_json::Value = serde_json::from_slice(&alice_ov_resp.body)?;
    assert_eq!(alice_ov["viewer"], "alice");
    assert_eq!(alice_ov["writeback_hint"]["read_only"], true);
    assert_eq!(
        alice_ov["writeback_hint"]["export_cmd"],
        "kp export ./notes"
    );
    assert_eq!(alice_ov["writeback_hint"]["apply_cmd"], "kp apply ./notes");

    let alice_facts = alice_ov["facts"].as_array().ok_or("facts array")?;
    assert!(alice_facts
        .iter()
        .any(|f| f["fact_id"] == derived_warning.id.as_str() && f["is_derived"] == true));
    assert!(alice_facts
        .iter()
        .any(|f| f["fact_id"] == xss_fact.id.as_str() && f["claim"] == xss_payload));

    let alice_timeline = alice_ov["timeline"].as_array().ok_or("timeline array")?;
    let cats: BTreeSet<&str> = alice_timeline
        .iter()
        .filter_map(|e| e["category"].as_str())
        .collect();
    for expected_cat in ["supersede", "withdraw", "expire", "stale_premise"] {
        assert!(
            cats.contains(expected_cat),
            "expected timeline category `{expected_cat}`, got {cats:?}"
        );
    }

    let alice_reviews = alice_ov["reviews"].as_array().ok_or("reviews array")?;
    assert_eq!(alice_reviews.len(), 1);
    assert_eq!(alice_reviews[0]["id"], "rev_alice_m4_01");

    // Keyword search `q=主从切换` via `/api/v1/overview?q=...`.
    let search_ov_resp = send_inspector_http(
        port,
        "GET",
        &format!(
            "/api/v1/overview?q=%E4%B8%BB%E4%BB%8E%E5%88%87%E6%8D%A2&at={}",
            eval_at.0
        ),
        &[("Cookie", &alice_cookie)],
    )
    .await?;
    assert_eq!(search_ov_resp.status, 200);
    let search_ov: serde_json::Value = serde_json::from_slice(&search_ov_resp.body)?;
    assert!(!search_ov["facts"]
        .as_array()
        .ok_or("search facts")?
        .is_empty());

    // Column ② `/api/v1/why/kp-ref:<derived_id>` returns `ProofTree` + premises + lossless Chinese quote highlight!
    let why_resp = send_inspector_http(
        port,
        "GET",
        &format!("/api/v1/why/kp-ref:{}?at={}", derived_warning.id, eval_at.0),
        &[("Cookie", &alice_cookie)],
    )
    .await?;
    assert_eq!(why_resp.status, 200);
    assert_inspector_security_headers(&why_resp);
    let why_json: serde_json::Value = serde_json::from_slice(&why_resp.body)?;
    assert_eq!(why_json["fact"]["fact_id"], derived_warning.id.as_str());
    assert_eq!(
        why_json["proof"]["justifications"][0]["rule_id"],
        "R_COLLISION_M4"
    );
    assert_eq!(why_json["premise_facts"].as_array().map(Vec::len), Some(2));
    let ev_hl = &why_json["highlights"][premise_event.id.as_str()];
    assert_eq!(ev_hl["excerpt_highlighted"], cn_quote);
    let reconstructed = format!(
        "{}{}{}",
        ev_hl["excerpt_prefix"].as_str().unwrap_or(""),
        ev_hl["excerpt_highlighted"].as_str().unwrap_or(""),
        ev_hl["excerpt_suffix"].as_str().unwrap_or(""),
    );
    assert_eq!(reconstructed, cn_body);

    // 7. Carol's capability token (even with malicious `?viewer=alice`!) sees ZERO of Alice's private data!
    let carol_ov_resp = send_inspector_http(
        port,
        "GET",
        &format!("/api/v1/overview?viewer=alice&at={}", eval_at.0),
        &[("Cookie", &carol_cookie)],
    )
    .await?;
    assert_eq!(carol_ov_resp.status, 200);
    let carol_ov: serde_json::Value = serde_json::from_slice(&carol_ov_resp.body)?;
    assert_eq!(carol_ov["viewer"], "carol");
    let carol_facts = carol_ov["facts"].as_array().ok_or("carol facts")?;
    assert_eq!(carol_facts.len(), 1);
    assert_eq!(carol_facts[0]["fact_id"], carol_public.id.as_str());
    assert!(carol_ov["reviews"]
        .as_array()
        .ok_or("carol reviews")?
        .is_empty());
    let carol_body_str = String::from_utf8_lossy(&carol_ov_resp.body);
    assert!(!carol_body_str.contains("主从切换"));
    assert!(!carol_body_str.contains(xss_payload));
    assert!(!carol_body_str.contains("rev_alice_m4_01"));

    let carol_why_private = send_inspector_http(
        port,
        "GET",
        &format!(
            "/api/v1/why/{}?viewer=alice&at={}",
            premise_event.id, eval_at.0
        ),
        &[("Cookie", &carol_cookie)],
    )
    .await?;
    let carol_why_missing = send_inspector_http(
        port,
        "GET",
        &format!("/api/v1/why/fact_nonexistent_probe?at={}", eval_at.0),
        &[("Cookie", &carol_cookie)],
    )
    .await?;
    assert_eq!(carol_why_private.status, 404);
    assert_eq!(carol_why_private.body, carol_why_missing.body);

    // 8. Live member revocation immediately invalidates `dave`'s previously minted capability token (`403`)!
    {
        let mut admin_store = Store::open(dir.path())?;
        admin_store.revoke_member(&dave, eval_at)?;
    }
    let dave_revoked_api = send_inspector_http(
        port,
        "GET",
        &format!("/api/v1/overview?at={}", eval_at.0),
        &[("Cookie", &dave_cookie)],
    )
    .await?;
    assert_eq!(dave_revoked_api.status, 403);
    let dave_revoked_html =
        send_inspector_http(port, "GET", "/", &[("Cookie", &dave_cookie)]).await?;
    assert_eq!(dave_revoked_html.status, 403);

    daemon.shutdown().await?;

    // 9. When `default_viewer` (`carol`) is active, `DaemonRequest::Status` exposes `(Some(url), Some(carol))`;
    //    once `carol` is revoked via `revoke_member`, `DaemonRequest::Status` immediately omits both (`(None, None)`).
    let sock_rev = unit_socket("web5")?;
    let daemon_carol = DaemonServer::spawn_default(
        Store::open(dir.path())?,
        DaemonConfig {
            socket_path: sock_rev.clone(),
            idle_extract_window: Duration::from_secs(60),
            idle_shutdown_timeout: None,
            max_frame_bytes: 64 * 1024,
            web_inspector: Some(WebInspectorConfig::loopback(0, carol.clone())),
        },
    )
    .await?;
    let client_rev = DaemonClient::new(&sock_rev)?;
    let st_before = client_rev.status().await?;
    assert_eq!(st_before.web_inspector_viewer, Some(carol.clone()));
    assert!(st_before.web_inspector_url.is_some());

    {
        let mut admin_store = Store::open(dir.path())?;
        admin_store.revoke_member(&carol, eval_at)?;
    }
    let st_after = client_rev.status().await?;
    assert_eq!(st_after.web_inspector_url, None);
    assert_eq!(st_after.web_inspector_viewer, None);

    daemon_carol.shutdown().await?;
    Ok(())
}
