use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use kp_connect::feishu::{
    resolve_lark_cli_binary, validate_lark_cli_readonly_args, FeishuConnector,
    FeishuRateLimitPolicy, FeishuSyncConfig, LarkCliTransport, FEISHU_ERR_RATE_LIMITED,
};
use kp_connect::github::{
    validate_gh_cli_readonly_args, validate_github_api_base_url, validate_github_readonly_endpoint,
    GhCliFallbackTransport, GitHubConnector, GitHubRateLimitPolicy, GitHubSyncConfig,
    GitHubTransportChain, ReqwestGitHubTransport, ENV_GH_CLI_BIN, ENV_GITHUB_API_BASE,
};
use kp_connect::sessions::{
    read_session_file_incremental, AgentSessionConnector, SessionBackfillCheckpoint,
    SessionBackfillConfig, SessionClientKind,
};
use kp_connect::subprocess::{CommandOutput, CommandRunner, MockCommandRunner, RecordingSleeper};
use kp_connect::{
    feishu_enable_notice, parse_public_jsonl_str, ConnectError, Connector, ConnectorCursor,
    ConnectorKind, ConnectorScope, ConnectorStateStore, FeishuTransport, GitHubConditionalPage,
    GitHubTransport, PublicAudience, PublicIngestLine, PublicIngestOp, Result, SourceEventOp,
};
use kp_model::{ActorId, AudienceSpec, EpochMs, ModelRoutePolicy};
use serde_json::{json, Value};

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn load_fixture_json(rel: &str) -> Result<Value> {
    let bytes = fs::read(fixtures_dir().join(rel))?;
    Ok(serde_json::from_slice(&bytes)?)
}

#[derive(Default)]
struct ScriptedFeishuTransport {
    chat_members: Mutex<BTreeMap<String, VecDeque<Value>>>,
    chat_messages: Mutex<BTreeMap<String, VecDeque<Value>>>,
    thread_messages: Mutex<BTreeMap<String, VecDeque<Value>>>,
    wiki_nodes: Mutex<VecDeque<Value>>,
    wiki_calls: Mutex<Vec<(Option<String>, Option<String>)>>,
    docs: Mutex<BTreeMap<String, VecDeque<Value>>>,
    perm_members: Mutex<BTreeMap<String, Value>>,
    doc_fetch_calls: Mutex<usize>,
}

impl ScriptedFeishuTransport {
    fn push_chat_poll(&self, chat_id: &str, members: Value, messages: Value) -> Result<()> {
        self.chat_members
            .lock()
            .map_err(|_| ConnectError::Transport("lock".into()))?
            .entry(chat_id.to_owned())
            .or_default()
            .push_back(members);
        self.chat_messages
            .lock()
            .map_err(|_| ConnectError::Transport("lock".into()))?
            .entry(chat_id.to_owned())
            .or_default()
            .push_back(messages);
        Ok(())
    }

    fn push_thread_poll(&self, thread_id: &str, messages: Value) -> Result<()> {
        self.thread_messages
            .lock()
            .map_err(|_| ConnectError::Transport("lock".into()))?
            .entry(thread_id.to_owned())
            .or_default()
            .push_back(messages);
        Ok(())
    }

    fn doc_fetches(&self) -> Result<usize> {
        Ok(*self
            .doc_fetch_calls
            .lock()
            .map_err(|_| ConnectError::Transport("lock".into()))?)
    }
}

impl FeishuTransport for ScriptedFeishuTransport {
    fn list_chat_messages<'a>(
        &'a self,
        chat_id: &'a str,
        _start_time_sec: Option<i64>,
        _end_time_sec: Option<i64>,
        _page_token: Option<&'a str>,
    ) -> kp_model::BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            let mut guard = self
                .chat_messages
                .lock()
                .map_err(|_| ConnectError::Transport("lock".into()))?;
            guard
                .get_mut(chat_id)
                .and_then(VecDeque::pop_front)
                .ok_or_else(|| ConnectError::Transport(format!("no chat messages for {chat_id}")))
        })
    }

    fn list_thread_messages<'a>(
        &'a self,
        thread_id: &'a str,
        _page_token: Option<&'a str>,
    ) -> kp_model::BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            let mut guard = self
                .thread_messages
                .lock()
                .map_err(|_| ConnectError::Transport("lock".into()))?;
            guard
                .get_mut(thread_id)
                .and_then(VecDeque::pop_front)
                .ok_or_else(|| {
                    ConnectError::Transport(format!("no thread messages for {thread_id}"))
                })
        })
    }

    fn list_chat_members<'a>(
        &'a self,
        chat_id: &'a str,
        _page_token: Option<&'a str>,
    ) -> kp_model::BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            let mut guard = self
                .chat_members
                .lock()
                .map_err(|_| ConnectError::Transport("lock".into()))?;
            guard
                .get_mut(chat_id)
                .and_then(VecDeque::pop_front)
                .ok_or_else(|| ConnectError::Transport(format!("no chat members for {chat_id}")))
        })
    }

    fn fetch_document<'a>(&'a self, doc_token: &'a str) -> kp_model::BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            {
                let mut calls = self
                    .doc_fetch_calls
                    .lock()
                    .map_err(|_| ConnectError::Transport("lock".into()))?;
                *calls += 1;
            }
            let mut guard = self
                .docs
                .lock()
                .map_err(|_| ConnectError::Transport("lock".into()))?;
            guard
                .get_mut(doc_token)
                .and_then(VecDeque::pop_front)
                .ok_or_else(|| ConnectError::Transport(format!("no doc fixture for {doc_token}")))
        })
    }

    fn list_wiki_nodes<'a>(
        &'a self,
        _space_id: &'a str,
        parent_node_token: Option<&'a str>,
        page_token: Option<&'a str>,
    ) -> kp_model::BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            self.wiki_calls
                .lock()
                .map_err(|_| ConnectError::Transport("lock".into()))?
                .push((
                    parent_node_token.map(str::to_owned),
                    page_token.map(str::to_owned),
                ));
            let mut guard = self
                .wiki_nodes
                .lock()
                .map_err(|_| ConnectError::Transport("lock".into()))?;
            guard
                .pop_front()
                .ok_or_else(|| ConnectError::Transport("no wiki_nodes fixture".into()))
        })
    }

    fn fetch_minutes_transcript<'a>(
        &'a self,
        minute_token: &'a str,
    ) -> kp_model::BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            Ok(json!({
                "ok": true,
                "identity": "user",
                "data": {
                    "notes": [{
                        "minute_token": minute_token,
                        "verbatim_doc_token": "doxcn_runbook",
                        "creator_id": "ou_pm_li"
                    }]
                }
            }))
        })
    }

    fn list_calendar_events<'a>(
        &'a self,
        _calendar_id: &'a str,
        _start: EpochMs,
        _end: EpochMs,
    ) -> kp_model::BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            Ok(json!({
                "ok": true,
                "identity": "user",
                "data": { "items": [] }
            }))
        })
    }

    fn list_permission_members<'a>(
        &'a self,
        _resource_type: &'a str,
        resource_token: &'a str,
    ) -> kp_model::BoxFuture<'a, Result<Value>> {
        Box::pin(async move {
            let guard = self
                .perm_members
                .lock()
                .map_err(|_| ConnectError::Transport("lock".into()))?;
            guard.get(resource_token).cloned().ok_or_else(|| {
                ConnectError::Transport(format!("no perm fixture for {resource_token}"))
            })
        })
    }
}

#[tokio::test]
async fn feishu_lark_cli_transport_blocks_writes_discovers_nvm_bin_and_reads_minutes_tempdir(
) -> Result<()> {
    // 1. Valid `lark-cli 1.0.50` read-only flags (`--no-reactions`, `--doc-format`, `--detail`,
    //    `--revision-id`, `--output-dir` relative) are accepted by the validator, while mutating
    //    verbs, path traversal, and nonexistent flags are hard-rejected.
    for valid_case in [
        &[
            "im",
            "+chat-messages-list",
            "--chat-id",
            "oc_1",
            "--no-reactions",
        ][..],
        &[
            "docs",
            "+fetch",
            "--doc",
            "doxcn_1",
            "--doc-format",
            "markdown",
            "--detail",
            "simple",
            "--revision-id",
            "-1",
        ][..],
        &[
            "vc",
            "+notes",
            "--as",
            "user",
            "--minute-tokens",
            "obcn_1",
            "--output-dir",
            "minutes_out",
        ][..],
    ] {
        let args: Vec<String> = valid_case.iter().map(|s| (*s).to_owned()).collect();
        validate_lark_cli_readonly_args(&args)?;
    }

    let forbidden_cases: &[&[&str]] = &[
        &["im", "+messages-send", "--chat-id", "oc_1", "--text", "hi"],
        &[
            "im",
            "+messages-reply",
            "--message-id",
            "om_1",
            "--text",
            "hi",
        ],
        &["im", "messages", "delete", "--params", "{}"],
        &["im", "+chat-create", "--name", "test"],
        &["docs", "+create", "--title", "hack"],
        &["docs", "+update", "--doc", "doxcn_1"],
        &["wiki", "+node-delete", "--node-token", "wikcn_1"],
        &["drive", "+upload", "--file", "secret.txt"],
        &["api", "POST", "/open-apis/im/v1/messages", "--data", "{}"],
        &[
            "api",
            "GET",
            "/open-apis/drive/v1/permissions/doxcn_1/members",
            "--data",
            "{}",
        ],
        &[
            "im",
            "+chat-messages-list",
            "--chat-id",
            "oc_1",
            "--nonexistent-flag",
        ],
        &[
            "docs",
            "+fetch",
            "--doc",
            "doxcn_1",
            "--nonexistent-doc-flag",
            "val",
        ],
        &[
            "vc",
            "+notes",
            "--as",
            "user",
            "--minute-tokens",
            "obcn_1",
            "--overwrite",
        ],
        &[
            "vc",
            "+notes",
            "--as",
            "user",
            "--minute-tokens",
            "obcn_1",
            "--output-dir",
            "../escape",
        ],
    ];
    for case in forbidden_cases {
        let args: Vec<String> = case.iter().map(|s| (*s).to_owned()).collect();
        let res = validate_lark_cli_readonly_args(&args);
        assert!(
            matches!(res, Err(ConnectError::ReadOnlyViolation(_))),
            "expected ReadOnlyViolation for {case:?}, got {res:?}"
        );
    }

    // MockCommandRunner ITSELF rejects invalid flags even if invoked directly.
    let runner = Arc::new(MockCommandRunner::default());
    let direct_bad = runner
        .run(
            "lark-cli",
            &[
                "docs".into(),
                "+fetch".into(),
                "--doc".into(),
                "doxcn_1".into(),
                "--nonexistent-flag".into(),
            ],
        )
        .await;
    assert!(matches!(
        direct_bad,
        Err(ConnectError::ReadOnlyViolation(_))
    ));

    // 2. Verify all 8 read-only methods emit portable CLI invocations, handle 99991400 backoff,
    //    and capture `vc +notes` `artifacts.transcript_file` inside an ephemeral tempdir without
    //    polluting the caller's cwd!
    let sleeper = Arc::new(RecordingSleeper::default());

    runner.push_json_ok(&json!({
        "code": FEISHU_ERR_RATE_LIMITED,
        "msg": "request trigger frequency limit"
    }))?;
    runner.push_output(Ok(CommandOutput {
        status_code: 1,
        stdout: Vec::new(),
        stderr: format!("error code {FEISHU_ERR_RATE_LIMITED}").into_bytes(),
    }))?;
    runner.push_json_ok(&load_fixture_json("feishu/pm_chat_messages_initial.json")?)?;
    runner.push_json_ok(&load_fixture_json("feishu/eng_thread_messages.json")?)?;
    runner.push_json_ok(&load_fixture_json("feishu/pm_chat_members_initial.json")?)?;
    runner.push_json_ok(&json!({
        "ok": true,
        "identity": "user",
        "data": { "document": { "document_id": "doxcn_runbook", "revision_id": 1, "content": "<title>Spec</title><p>Body</p>" } }
    }))?;
    runner.push_json_ok(&load_fixture_json("feishu/wiki_nodes_initial.json")?)?;
    runner.push_json_ok(&json!({"ok": true, "identity": "user", "data": {"items": []}}))?;
    runner.push_json_ok(&load_fixture_json("feishu/drive_perm_members.json")?)?;

    // For `vc +notes`, simulate real `lark-cli` exporting `./minutes_out/artifact-cutover/transcript.txt`
    // inside the ephemeral `cwd` (`tempfile::tempdir()`) and returning `artifacts.transcript_file` + JSON `summary`.
    let captured_temp_cwd: Arc<Mutex<Option<PathBuf>>> = Arc::new(Mutex::new(None));
    let captured_clone = captured_temp_cwd.clone();
    runner.push_handler(move |_, args, cwd| {
        assert!(args
            .windows(2)
            .any(|w| w == ["--output-dir", "minutes_out"]));
        let temp_root = cwd
            .ok_or_else(|| ConnectError::Transport("expected ephemeral tempdir cwd".into()))?
            .to_path_buf();
        if let Ok(mut g) = captured_clone.lock() {
            *g = Some(temp_root.clone());
        }
        let rel_file = "minutes_out/artifact-release-obcn_min1/transcript.txt";
        let full_path = temp_root.join(rel_file);
        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(
            &full_path,
            "李四: 下周四凌晨两点数据库主从切换\n王五: 主从切换窗口禁止跑批处理任务\n",
        )?;
        let payload = json!({
            "ok": true,
            "identity": "user",
            "data": {
                "owner_id": "ou_pm_li",
                "artifacts": {
                    "summary": { "overview": "简短 AI 总结（不应覆盖逐字稿）" },
                    "todos": [],
                    "transcript_file": format!("./{rel_file}")
                }
            }
        });
        Ok(CommandOutput {
            status_code: 0,
            stdout: serde_json::to_vec(&payload)?,
            stderr: Vec::new(),
        })
    })?;

    let resolved_bin = resolve_lark_cli_binary();
    let transport = LarkCliTransport::new(&resolved_bin)
        .with_profile("corp")
        .with_rate_limit_policy(FeishuRateLimitPolicy {
            base_cooldown_ms: 1_000,
            max_cooldown_ms: 10_000,
            max_retries: 3,
        })
        .with_runner_and_sleeper(runner.clone(), sleeper.clone());

    let msgs = transport
        .list_chat_messages(
            "oc_pm_release",
            Some(1_795_000_000),
            Some(1_795_200_000),
            None,
        )
        .await?;
    assert_eq!(msgs.pointer("/data/total").and_then(Value::as_u64), Some(3));
    assert_eq!(sleeper.recorded_sleeps()?, vec![1_000, 2_000]);

    let _ = transport
        .list_thread_messages("omt_eng_cutover_thread", None)
        .await?;
    let _ = transport.list_chat_members("oc_pm_release", None).await?;
    let _ = transport.fetch_document("doxcn_runbook").await?;
    let _ = transport.list_wiki_nodes("7410001", None, None).await?;

    let _ = transport
        .list_calendar_events(
            "primary",
            EpochMs(1_795_000_000_000),
            EpochMs(1_795_100_000_000),
        )
        .await?;
    let _ = transport
        .list_permission_members("docx", "doxcn_runbook")
        .await?;

    // Run `FeishuConnector` on `minutes:obcn_min1` so both `fetch_minutes_transcript` tempdir export
    // AND `extract_minutes_text` preferring `transcript` ahead of `artifacts.summary` are verified!
    let caller_cwd_minutes = std::env::current_dir()?.join("minutes");
    let minutes_existed_before = caller_cwd_minutes.exists();

    let min_connector = FeishuConnector::new(transport, FeishuSyncConfig::default());
    let min_scope = ConnectorScope {
        connector_id: "feishu_minutes".parse()?,
        source_scope_id: "feishu.minutes.obcn_min1".parse()?,
        upstream_container_id: "minutes:obcn_min1".into(),
        label: "发布评审妙记".into(),
        enabled: true,
        model_policy: ModelRoutePolicy::RawOnly,
    };
    let alice: ActorId = "eng_alice".parse()?;
    let min_batch = min_connector
        .poll_incremental(&min_scope, &ConnectorCursor::default(), &alice)
        .await?;
    assert_eq!(min_batch.events.len(), 1);
    assert!(min_batch.events[0]
        .content
        .contains("李四: 下周四凌晨两点数据库主从切换"));
    assert!(!min_batch.events[0].content.contains("简短 AI 总结"));

    if !minutes_existed_before {
        assert!(
            !caller_cwd_minutes.exists(),
            "`vc +notes` must never create `./minutes` in caller cwd"
        );
    }
    let temp_dir_used = captured_temp_cwd
        .lock()
        .map_err(|_| ConnectError::Transport("lock".into()))?
        .clone()
        .ok_or_else(|| ConnectError::Transport("expected recorded tempdir".into()))?;
    assert!(
        !temp_dir_used.exists(),
        "ephemeral tempdir must be cleaned up automatically after `fetch_minutes_transcript` returns"
    );

    let invocations = runner.invocations()?;
    assert_eq!(invocations.len(), 10);
    for (_, args) in &invocations {
        validate_lark_cli_readonly_args(args)?;
    }

    // Execute real `--dry-run` via `resolve_lark_cli_binary()` (finds NVM `lark-cli` even under stripped PATHs)
    if Path::new(&resolved_bin).is_file() {
        let mut dry_runs_executed = 0_usize;
        for (_, args) in invocations.iter().skip(2) {
            let mut dry_args: Vec<String> = args[2..].to_vec(); // strip `--profile corp`
            dry_args.push("--dry-run".into());
            let out = Command::new(&resolved_bin)
                .args(&dry_args)
                .stdin(Stdio::null())
                .output()?;
            dry_runs_executed += 1;
            let stdout_str = String::from_utf8_lossy(&out.stdout);
            let stderr_str = String::from_utf8_lossy(&out.stderr);
            let combined = format!("{stdout_str}\n{stderr_str}");
            assert!(
                !combined.contains("\"unknown_flag\"")
                    && !combined.contains("\"unknown_command\"")
                    && !combined.contains("unknown flag")
                    && !combined.contains("unknown command")
                    && !combined.contains("\"invalid_argument\""),
                "real `{resolved_bin} {}` rejected command/flags:\n{combined}",
                dry_args.join(" ")
            );
        }
        assert_eq!(dry_runs_executed, 8);
    }

    Ok(())
}

#[tokio::test]
async fn feishu_m3_cross_group_cutover_thread_replies_departure_edit_and_recall() -> Result<()> {
    assert_eq!(
        ConnectorKind::Feishu.default_model_policy(),
        ModelRoutePolicy::HostCli
    );
    assert_eq!(
        feishu_enable_notice("Codex / OpenAI"),
        "提示：同步内容将使用当前全局模型处理（当前：Codex / OpenAI），如需更改或仅存原文请见 kp config"
    );

    let transport = ScriptedFeishuTransport::default();
    transport.push_chat_poll(
        "oc_pm_release",
        load_fixture_json("feishu/pm_chat_members_initial.json")?,
        load_fixture_json("feishu/pm_chat_messages_initial.json")?,
    )?;
    transport.push_chat_poll(
        "oc_eng_core",
        load_fixture_json("feishu/eng_chat_members_initial.json")?,
        load_fixture_json("feishu/eng_chat_messages_initial.json")?,
    )?;
    transport.push_thread_poll(
        "omt_eng_cutover_thread",
        load_fixture_json("feishu/eng_thread_messages.json")?,
    )?;

    transport.push_chat_poll(
        "oc_pm_release",
        load_fixture_json("feishu/pm_chat_members_after_departure.json")?,
        load_fixture_json("feishu/pm_chat_messages_rescan_edit.json")?,
    )?;
    transport.push_chat_poll(
        "oc_pm_release",
        load_fixture_json("feishu/pm_chat_members_after_departure.json")?,
        load_fixture_json("feishu/pm_chat_messages_rescan_edit.json")?,
    )?;
    transport.push_chat_poll(
        "oc_pm_release",
        load_fixture_json("feishu/pm_chat_members_after_departure.json")?,
        load_fixture_json("feishu/pm_chat_messages_rescan_recall.json")?,
    )?;

    let connector = FeishuConnector::new(
        transport,
        FeishuSyncConfig {
            now: EpochMs(1_795_245_380_000),
            ..FeishuSyncConfig::default()
        },
    );

    let importer_alice: ActorId = "eng_alice".parse()?;
    let pm_li: ActorId = "pm_li".parse()?;
    let lead_wang: ActorId = "lead_wang".parse()?;
    let intern_bob: ActorId = "intern_bob".parse()?;
    let outsider_carol: ActorId = "carol".parse()?;

    let mut pm_scope = ConnectorScope {
        connector_id: "feishu_main".parse()?,
        source_scope_id: "feishu.chat.oc_pm_release".parse()?,
        upstream_container_id: "chat:oc_pm_release".into(),
        label: "产品发布对齐群".into(),
        enabled: false,
        model_policy: ConnectorKind::Feishu.default_model_policy(),
    };

    let disabled_err = connector
        .poll_incremental(&pm_scope, &ConnectorCursor::default(), &importer_alice)
        .await;
    assert!(matches!(disabled_err, Err(ConnectError::ScopeDisabled(_))));
    pm_scope.enabled = true;

    let eng_scope = ConnectorScope {
        connector_id: "feishu_main".parse()?,
        source_scope_id: "feishu.chat.oc_eng_core".parse()?,
        upstream_container_id: "chat:oc_eng_core".into(),
        label: "研发核心群".into(),
        enabled: true,
        model_policy: ConnectorKind::Feishu.default_model_policy(),
    };

    let pm_batch_1 = connector
        .poll_incremental(&pm_scope, &ConnectorCursor::default(), &importer_alice)
        .await?;
    let eng_batch_1 = connector
        .poll_incremental(&eng_scope, &ConnectorCursor::default(), &importer_alice)
        .await?;

    assert_eq!(pm_batch_1.skipped_low_signal, 2);
    assert_eq!(pm_batch_1.events.len(), 1);
    let cutover_ev = &pm_batch_1.events[0];
    assert_eq!(cutover_ev.content, "下周四凌晨两点数据库主从切换");
    assert_eq!(cutover_ev.speaker_external_id, "ou_pm_li");
    assert_eq!(cutover_ev.speaker_display_name, "李四");
    assert_eq!(cutover_ev.ingested_by, importer_alice);
    assert_eq!(cutover_ev.model_policy, ModelRoutePolicy::HostCli);
    assert_eq!(cutover_ev.event_op, SourceEventOp::Upsert);
    let cutover_origin = cutover_ev.into_origin_meta(pm_li.clone());
    assert_eq!(cutover_origin.speaker, pm_li);
    assert_eq!(cutover_origin.ingested_by, importer_alice);
    assert_ne!(cutover_origin.speaker, cutover_origin.ingested_by);

    assert_eq!(eng_batch_1.skipped_low_signal, 1);
    assert_eq!(eng_batch_1.events.len(), 2);
    let policy_ev = eng_batch_1
        .events
        .iter()
        .find(|e| e.external_key == "om_eng_2001")
        .ok_or_else(|| ConnectError::Transport("missing thread reply om_eng_2001".into()))?;
    assert_eq!(policy_ev.content, "主从切换窗口禁止跑批处理任务");
    assert_eq!(policy_ev.speaker_external_id, "ou_lead_wang");
    assert_eq!(policy_ev.speaker_display_name, "王五");
    assert_eq!(policy_ev.ingested_by, importer_alice);
    assert_eq!(
        policy_ev.into_origin_meta(lead_wang.clone()).speaker,
        lead_wang
    );

    let confirmed = BTreeMap::from([
        (("feishu".into(), "ou_pm_li".into()), pm_li.clone()),
        (
            ("feishu".into(), "ou_eng_alice".into()),
            importer_alice.clone(),
        ),
        (
            ("feishu".into(), "ou_intern_bob".into()),
            intern_bob.clone(),
        ),
        (("feishu".into(), "ou_lead_wang".into()), lead_wang.clone()),
    ]);
    let all_members = BTreeSet::from([
        pm_li.clone(),
        importer_alice.clone(),
        intern_bob.clone(),
        lead_wang,
        outsider_carol.clone(),
    ]);

    let live_scope_t1 = pm_batch_1.scope_rosters[0].into_source_scope(&confirmed);
    assert!(live_scope_t1.allows(&intern_bob, &all_members));
    assert!(!live_scope_t1.allows(&outsider_carol, &all_members));

    let pm_batch_2 = connector
        .poll_incremental(&pm_scope, &pm_batch_1.next_cursor, &importer_alice)
        .await?;
    let live_scope_t2 = pm_batch_2.scope_rosters[0].into_source_scope(&confirmed);
    assert!(
        !live_scope_t2.allows(&intern_bob, &all_members),
        "departed member Bob must immediately lose SourceScope read access"
    );
    assert!(live_scope_t2
        .validate_publish_audience(
            &AudienceSpec::Explicit(BTreeSet::from([intern_bob])),
            &all_members
        )
        .is_err());

    assert_eq!(pm_batch_2.events.len(), 1);
    assert_eq!(
        pm_batch_2.events[0].event_op,
        SourceEventOp::Supersede {
            previous_external_key: "om_pm_1001".into()
        }
    );
    assert_eq!(pm_batch_2.events[0].external_key, "om_pm_1001@rev1");
    assert_eq!(pm_batch_2.events[0].content, "下周四凌晨三点数据库主从切换");

    let pm_batch_3 = connector
        .poll_incremental(&pm_scope, &pm_batch_2.next_cursor, &importer_alice)
        .await?;
    assert!(pm_batch_3.events.is_empty());

    let pm_batch_4 = connector
        .poll_incremental(&pm_scope, &pm_batch_3.next_cursor, &importer_alice)
        .await?;
    assert_eq!(pm_batch_4.events.len(), 1);
    assert_eq!(pm_batch_4.events[0].event_op, SourceEventOp::Withdraw);
    assert_eq!(pm_batch_4.events[0].external_key, "om_pm_1001@rev1");

    let paged_transport = ScriptedFeishuTransport::default();
    paged_transport.push_chat_poll(
        "oc_paged",
        load_fixture_json("feishu/pm_chat_members_initial.json")?,
        json!({
            "ok": true,
            "identity": "user",
            "data": {
                "messages": [{
                    "message_id": "om_p1",
                    "msg_type": "text",
                    "create_time": "1795100000000",
                    "sender": { "id": "ou_pm_li", "name": "李四" },
                    "content": "第一页历史消息",
                    "deleted": false,
                    "updated": false
                }],
                "total": 1,
                "has_more": true,
                "page_token": "tok_page_2"
            }
        }),
    )?;
    paged_transport.push_chat_poll(
        "oc_paged",
        load_fixture_json("feishu/pm_chat_members_initial.json")?,
        json!({
            "ok": true,
            "identity": "user",
            "data": {
                "messages": [{
                    "message_id": "om_p2",
                    "msg_type": "text",
                    "create_time": "1795100060000",
                    "sender": { "id": "ou_pm_li", "name": "李四" },
                    "content": "第二页断点续传消息",
                    "deleted": false,
                    "updated": false
                }],
                "total": 1,
                "has_more": false,
                "page_token": ""
            }
        }),
    )?;
    let paged_connector = FeishuConnector::new(
        paged_transport,
        FeishuSyncConfig {
            now: EpochMs(1_795_245_380_000),
            max_pages_per_poll: 1,
            ..FeishuSyncConfig::default()
        },
    );
    let paged_scope = ConnectorScope {
        connector_id: "feishu_main".parse()?,
        source_scope_id: "feishu.chat.oc_paged".parse()?,
        upstream_container_id: "chat:oc_paged".into(),
        label: "分页群".into(),
        enabled: true,
        model_policy: ModelRoutePolicy::RawOnly,
    };
    let page_1_batch = paged_connector
        .poll_incremental(&paged_scope, &ConnectorCursor::default(), &importer_alice)
        .await?;
    assert_eq!(page_1_batch.events.len(), 1);
    assert_eq!(page_1_batch.events[0].external_key, "om_p1");

    let page_2_batch = paged_connector
        .poll_incremental(&paged_scope, &page_1_batch.next_cursor, &importer_alice)
        .await?;
    assert_eq!(page_2_batch.events.len(), 1);
    assert_eq!(page_2_batch.events[0].external_key, "om_p2");

    Ok(())
}

#[tokio::test]
async fn feishu_weknora_wiki_obj_edit_time_pagination_and_no_false_withdraw_under_budget(
) -> Result<()> {
    let transport = ScriptedFeishuTransport::default();
    let perm = load_fixture_json("feishu/drive_perm_members.json")?;
    {
        let mut perms = transport
            .perm_members
            .lock()
            .map_err(|_| ConnectError::Transport("lock".into()))?;
        perms.insert("doxcn_runbook".into(), perm.clone());
        perms.insert("doxcn_legacy".into(), perm.clone());
        perms.insert("doxcn_child".into(), perm.clone());
    }
    {
        let mut wiki = transport
            .wiki_nodes
            .lock()
            .map_err(|_| ConnectError::Transport("lock".into()))?;
        wiki.push_back(load_fixture_json("feishu/wiki_nodes_initial.json")?);
        wiki.push_back(load_fixture_json("feishu/wiki_nodes_initial.json")?);
        wiki.push_back(load_fixture_json(
            "feishu/wiki_nodes_updated_and_deleted.json",
        )?);
    }
    {
        let mut docs = transport
            .docs
            .lock()
            .map_err(|_| ConnectError::Transport("lock".into()))?;
        docs.insert(
            "doxcn_runbook".into(),
            VecDeque::from([
                json!({
                    "ok": true,
                    "identity": "user",
                    "data": {
                        "document": {
                            "document_id": "doxcn_runbook",
                            "revision_id": 1,
                            "content": "<title>数据库值班手册 v1</title><p>主从切换窗口禁止跑批处理任务</p>"
                        }
                    }
                }),
                json!({
                    "ok": true,
                    "identity": "user",
                    "data": {
                        "document": {
                            "document_id": "doxcn_runbook",
                            "revision_id": 2,
                            "content": "<title>数据库值班手册 v2</title><p>主从切换后需校验复制延迟</p>"
                        }
                    }
                }),
            ]),
        );
        docs.insert(
            "doxcn_legacy".into(),
            VecDeque::from([json!({
                "ok": true,
                "identity": "user",
                "data": {
                    "document": {
                        "document_id": "doxcn_legacy",
                        "revision_id": 1,
                        "content": "<title>旧版发版草稿</title><p>已废弃</p>"
                    }
                }
            })]),
        );
    }

    let connector = FeishuConnector::new(transport, FeishuSyncConfig::default());
    let scope = ConnectorScope {
        connector_id: "feishu_wiki".parse()?,
        source_scope_id: "feishu.wiki.7410001".parse()?,
        upstream_container_id: "wiki:7410001".into(),
        label: "研发知识库".into(),
        enabled: true,
        model_policy: ModelRoutePolicy::RawOnly,
    };
    let alice: ActorId = "eng_alice".parse()?;

    let b1 = connector
        .poll_incremental(&scope, &ConnectorCursor::default(), &alice)
        .await?;
    assert_eq!(b1.events.len(), 2);
    assert_eq!(connector.transport().doc_fetches()?, 2);
    assert!(b1.events.iter().any(|e| {
        e.content.contains("# 数据库值班手册 v1")
            && e.content.contains("主从切换窗口禁止跑批处理任务")
    }));
    assert_eq!(
        b1.scope_rosters[0].external_member_ids,
        vec!["ou_eng_alice".to_owned(), "ou_lead_wang".to_owned()]
    );

    let b2 = connector
        .poll_incremental(&scope, &b1.next_cursor, &alice)
        .await?;
    assert!(b2.events.is_empty());
    assert_eq!(connector.transport().doc_fetches()?, 2);

    let b3 = connector
        .poll_incremental(&scope, &b2.next_cursor, &alice)
        .await?;
    assert_eq!(connector.transport().doc_fetches()?, 3);
    assert_eq!(b3.events.len(), 2);
    assert!(b3.events.iter().any(|e| {
        e.external_key == "wiki:7410001:wikcn_runbook@rev1"
            && e.content.contains("主从切换后需校验复制延迟")
            && e.event_op
                == SourceEventOp::Supersede {
                    previous_external_key: "wiki:7410001:wikcn_runbook".into(),
                }
    }));
    assert!(b3.events.iter().any(|e| {
        e.external_key == "wiki:7410001:wikcn_legacy" && e.event_op == SourceEventOp::Withdraw
    }));

    // Regression test for `#27` Item 2:
    // 1) Full walk (`max_pages_per_poll = 5`) ingests root page 1 (`has_more = true`, `page_token = "wp2"`),
    //    root page 2 (`has_child = true` on `wikcn_folder`), and child doc `wikcn_child` under `wikcn_folder`.
    // 2) Subsequent poll under tight budget (`max_pages_per_poll = 1`) stops after page 1 (leaving BOTH
    //    `page_token = "wp2"` AND `wikcn_folder` unvisited) -> MUST NOT emit false `Withdraw`s!
    let tree_transport = ScriptedFeishuTransport::default();
    {
        let mut perms = tree_transport
            .perm_members
            .lock()
            .map_err(|_| ConnectError::Transport("lock".into()))?;
        perms.insert("doxcn_p1".into(), perm.clone());
        perms.insert("doxcn_folder".into(), perm.clone());
        perms.insert("doxcn_child".into(), perm);
    }
    let root_p1 = json!({
        "ok": true,
        "identity": "user",
        "data": {
            "items": [{
                "space_id": "7410001",
                "node_token": "wikcn_p1",
                "obj_token": "doxcn_p1",
                "obj_type": "docx",
                "has_child": false,
                "title": "Page 1 Doc",
                "obj_edit_time": "1795100000",
                "owner": "ou_lead_wang"
            }],
            "has_more": true,
            "page_token": "wp2"
        }
    });
    let root_p2 = json!({
        "ok": true,
        "identity": "user",
        "data": {
            "items": [{
                "space_id": "7410001",
                "node_token": "wikcn_folder",
                "obj_token": "doxcn_folder",
                "obj_type": "docx",
                "has_child": true,
                "title": "Parent Folder",
                "obj_edit_time": "1795100000",
                "owner": "ou_lead_wang"
            }],
            "has_more": false,
            "page_token": ""
        }
    });
    let child_page = json!({
        "ok": true,
        "identity": "user",
        "data": {
            "items": [{
                "space_id": "7410001",
                "node_token": "wikcn_child",
                "obj_token": "doxcn_child",
                "obj_type": "docx",
                "parent_node_token": "wikcn_folder",
                "has_child": false,
                "title": "Child Doc",
                "obj_edit_time": "1795100000",
                "owner": "ou_eng_alice"
            }],
            "has_more": false,
            "page_token": ""
        }
    });
    {
        let mut wiki = tree_transport
            .wiki_nodes
            .lock()
            .map_err(|_| ConnectError::Transport("lock".into()))?;
        wiki.push_back(root_p1.clone());
        wiki.push_back(root_p2);
        wiki.push_back(child_page);
        // Budget-capped poll (`max_pages_per_poll = 1`) only reads `root_p1`:
        wiki.push_back(root_p1);
        // Another budget-capped poll where root has `has_child: true` (`has_more: false`):
        wiki.push_back(json!({
            "ok": true,
            "identity": "user",
            "data": {
                "items": [{
                    "space_id": "7410001",
                    "node_token": "wikcn_folder",
                    "obj_token": "doxcn_folder",
                    "obj_type": "docx",
                    "has_child": true,
                    "title": "Parent Folder",
                    "obj_edit_time": "1795100000",
                    "owner": "ou_lead_wang"
                }],
                "has_more": false,
                "page_token": ""
            }
        }));
    }
    {
        let mut docs = tree_transport
            .docs
            .lock()
            .map_err(|_| ConnectError::Transport("lock".into()))?;
        for (tok, title) in [
            ("doxcn_p1", "Page 1"),
            ("doxcn_folder", "Folder"),
            ("doxcn_child", "Child"),
        ] {
            docs.insert(
                tok.into(),
                VecDeque::from([json!({
                    "ok": true,
                    "identity": "user",
                    "data": { "document": { "document_id": tok, "revision_id": 1, "content": format!("<title>{title}</title>") } }
                })]),
            );
        }
    }

    let full_walk = FeishuConnector::new(
        tree_transport,
        FeishuSyncConfig {
            max_pages_per_poll: 5,
            ..FeishuSyncConfig::default()
        },
    );
    let init_tree = full_walk
        .poll_incremental(&scope, &ConnectorCursor::default(), &alice)
        .await?;
    assert_eq!(init_tree.events.len(), 3);
    let recorded_calls = full_walk
        .transport()
        .wiki_calls
        .lock()
        .map_err(|_| ConnectError::Transport("lock".into()))?
        .clone();
    assert_eq!(
        recorded_calls,
        vec![
            (None, None),
            (None, Some("wp2".into())),
            (Some("wikcn_folder".into()), None),
        ]
    );

    // Swap to `max_pages_per_poll = 1` over the remaining queued responses: neither `has_more: true`
    // nor unvisited `has_child: true` may trigger false `Withdraw` events!
    let capped_walk = FeishuConnector::new(
        ScriptedFeishuTransport {
            wiki_nodes: Mutex::new(
                full_walk
                    .transport()
                    .wiki_nodes
                    .lock()
                    .map_err(|_| ConnectError::Transport("lock".into()))?
                    .clone(),
            ),
            perm_members: Mutex::new(
                full_walk
                    .transport()
                    .perm_members
                    .lock()
                    .map_err(|_| ConnectError::Transport("lock".into()))?
                    .clone(),
            ),
            ..ScriptedFeishuTransport::default()
        },
        FeishuSyncConfig {
            max_pages_per_poll: 1,
            ..FeishuSyncConfig::default()
        },
    );
    let capped_has_more = capped_walk
        .poll_incremental(&scope, &init_tree.next_cursor, &alice)
        .await?;
    assert!(
        capped_has_more.events.is_empty(),
        "budget break on `has_more = true` must never emit false Withdraws, got {:?}",
        capped_has_more.events
    );
    let capped_has_child = capped_walk
        .poll_incremental(&scope, &capped_has_more.next_cursor, &alice)
        .await?;
    assert!(
        capped_has_child.events.is_empty(),
        "budget break with unvisited `has_child = true` folder must never emit false Withdraws, got {:?}",
        capped_has_child.events
    );

    Ok(())
}

#[derive(Default)]
struct FailingPrimaryGitHub;

impl GitHubTransport for FailingPrimaryGitHub {
    fn get_json<'a>(
        &'a self,
        endpoint: &'a str,
        _etag: Option<&'a str>,
    ) -> kp_model::BoxFuture<'a, Result<Option<GitHubConditionalPage>>> {
        Box::pin(async move {
            validate_github_readonly_endpoint(endpoint)?;
            Err(ConnectError::Transport(
                "offline sandbox: use gh fallback".into(),
            ))
        })
    }
}

#[derive(Default)]
struct ScriptedGitHubTransport {
    routes: Mutex<BTreeMap<String, VecDeque<Result<Option<GitHubConditionalPage>>>>>,
    seen_etags: Mutex<Vec<(String, Option<String>)>>,
}

impl ScriptedGitHubTransport {
    fn push_route(&self, prefix: &str, resp: Result<Option<GitHubConditionalPage>>) -> Result<()> {
        self.routes
            .lock()
            .map_err(|_| ConnectError::Transport("lock".into()))?
            .entry(prefix.to_owned())
            .or_default()
            .push_back(resp);
        Ok(())
    }
}

impl GitHubTransport for ScriptedGitHubTransport {
    fn get_json<'a>(
        &'a self,
        endpoint: &'a str,
        etag: Option<&'a str>,
    ) -> kp_model::BoxFuture<'a, Result<Option<GitHubConditionalPage>>> {
        Box::pin(async move {
            validate_github_readonly_endpoint(endpoint)?;
            self.seen_etags
                .lock()
                .map_err(|_| ConnectError::Transport("lock".into()))?
                .push((endpoint.to_owned(), etag.map(str::to_owned)));
            let mut guard = self
                .routes
                .lock()
                .map_err(|_| ConnectError::Transport("lock".into()))?;
            for (prefix, queue) in guard.iter_mut() {
                if endpoint == prefix || endpoint.starts_with(&format!("{prefix}?")) {
                    if let Some(next) = queue.pop_front() {
                        return next;
                    }
                }
            }
            Ok(Some(GitHubConditionalPage {
                body: json!([]),
                etag: Some("W/\"empty\"".into()),
                rate_limit_remaining: Some(4990),
                rate_limit_reset_epoch_sec: Some(1_795_250_000),
            }))
        })
    }
}

#[tokio::test]
async fn github_readonly_guards_etag_304_rate_limit_backoff_and_scope_rosters() -> Result<()> {
    assert!(matches!(
        validate_github_readonly_endpoint("/graphql"),
        Err(ConnectError::ReadOnlyViolation(_))
    ));
    assert!(matches!(
        validate_github_readonly_endpoint("/repos/acme/payments-core/merges"),
        Err(ConnectError::ReadOnlyViolation(_))
    ));
    let reqwest_transport = ReqwestGitHubTransport::new(None)?;
    assert!(matches!(
        reqwest_transport
            .get_json("/repos/acme/payments-core/dispatches", None)
            .await,
        Err(ConnectError::ReadOnlyViolation(_))
    ));
    assert!(matches!(
        validate_gh_cli_readonly_args(&[
            "api".into(),
            "--method".into(),
            "POST".into(),
            "--include".into(),
            "/repos/acme/payments-core/issues".into()
        ]),
        Err(ConnectError::ReadOnlyViolation(_))
    ));
    assert!(matches!(
        validate_gh_cli_readonly_args(&[
            "api".into(),
            "--method".into(),
            "GET".into(),
            "--include".into(),
            "--nonexistent-gh-flag".into(),
            "/repos/acme/payments-core/issues".into()
        ]),
        Err(ConnectError::ReadOnlyViolation(_))
    ));

    let runner = Arc::new(MockCommandRunner::default());
    let sleeper = Arc::new(RecordingSleeper::default());
    let now_sec = 1_795_245_380_i64;

    runner.push_output(Ok(CommandOutput {
        status_code: 1,
        stdout: format!(
            "HTTP/2.0 403 Forbidden\r\nX-RateLimit-Remaining: 0\r\nX-RateLimit-Reset: {}\r\n\r\n{{\"message\":\"rate limit\"}}",
            now_sec + 7
        )
        .into_bytes(),
        stderr: Vec::new(),
    }))?;
    let pub_repo_str = serde_json::to_string(&load_fixture_json("github/repo_public.json")?)?;
    runner.push_output(Ok(CommandOutput {
        status_code: 0,
        stdout: format!(
            "HTTP/2.0 200 OK\r\nETag: W/\"repo-v1\"\r\nX-RateLimit-Remaining: 4500\r\nX-RateLimit-Reset: {}\r\n\r\n{pub_repo_str}",
            now_sec + 3600
        )
        .into_bytes(),
        stderr: Vec::new(),
    }))?;
    runner.push_output(Ok(CommandOutput {
        status_code: 0,
        stdout: b"HTTP/2.0 304 Not Modified\r\nETag: W/\"repo-v1\"\r\n\r\n".to_vec(),
        stderr: Vec::new(),
    }))?;

    let gh_fallback = GhCliFallbackTransport::new("gh").with_runner_and_sleeper(
        GitHubRateLimitPolicy {
            low_watermark: 10,
            reset_cushion_sec: 2,
            default_backoff_ms: 5_000,
            max_backoff_ms: 60_000,
            max_retries: 2,
        },
        Arc::new(move || now_sec),
        runner,
        sleeper.clone(),
    );
    let chain = GitHubTransportChain::new(FailingPrimaryGitHub, gh_fallback, false);

    let first = chain
        .get_json("/repos/acme/open-sdk", None)
        .await?
        .ok_or_else(|| ConnectError::Transport("expected 200 page".into()))?;
    assert_eq!(first.etag.as_deref(), Some("W/\"repo-v1\""));
    assert_eq!(sleeper.recorded_sleeps()?, vec![9_000]);

    let not_modified = chain
        .get_json("/repos/acme/open-sdk", Some("W/\"repo-v1\""))
        .await?;
    assert!(not_modified.is_none());

    let scripted = ScriptedGitHubTransport::default();
    scripted.push_route(
        "/repos/acme/payments-core",
        Ok(Some(GitHubConditionalPage {
            body: load_fixture_json("github/repo_private.json")?,
            etag: Some("W/\"priv-1\"".into()),
            rate_limit_remaining: Some(4900),
            rate_limit_reset_epoch_sec: Some(now_sec + 3600),
        })),
    )?;
    scripted.push_route(
        "/repos/acme/payments-core/collaborators",
        Ok(Some(GitHubConditionalPage {
            body: load_fixture_json("github/collaborators.json")?,
            etag: Some("W/\"collab-1\"".into()),
            rate_limit_remaining: Some(4899),
            rate_limit_reset_epoch_sec: Some(now_sec + 3600),
        })),
    )?;
    scripted.push_route(
        "/repos/acme/payments-core/issues",
        Ok(Some(GitHubConditionalPage {
            body: load_fixture_json("github/issues_page1.json")?,
            etag: Some("W/\"issues-p1\"".into()),
            rate_limit_remaining: Some(4898),
            rate_limit_reset_epoch_sec: Some(now_sec + 3600),
        })),
    )?;
    scripted.push_route(
        "/repos/acme/payments-core/issues",
        Ok(Some(GitHubConditionalPage {
            body: json!([{
                "number": 44,
                "title": "Pause batch settlements during DB replica cutover",
                "body": "Guards primary-replica cutover maintenance window.",
                "html_url": "https://github.com/acme/payments-core/issues/44",
                "user": { "login": "eng-alice" },
                "created_at": "2026-11-20T08:00:00Z",
                "updated_at": "2026-11-20T08:00:00Z"
            }]),
            etag: None,
            rate_limit_remaining: Some(4897),
            rate_limit_reset_epoch_sec: Some(now_sec + 3600),
        })),
    )?;
    scripted.push_route("/repos/acme/payments-core", Ok(None))?;
    scripted.push_route("/repos/acme/payments-core/collaborators", Ok(None))?;
    scripted.push_route(
        "/repos/acme/payments-core/issues/comments",
        Ok(Some(GitHubConditionalPage {
            body: json!([
                {
                    "id": 701,
                    "body": "LGTM!",
                    "user": { "login": "eng-alice" },
                    "updated_at": "2026-11-19T12:00:00Z"
                },
                {
                    "id": 702,
                    "body": "Please wait until Thursday's DB replica cutover finishes.",
                    "user": { "login": "lead-wang" },
                    "updated_at": "2026-11-19T12:05:00Z"
                }
            ]),
            etag: Some("W/\"ic-1\"".into()),
            rate_limit_remaining: Some(4896),
            rate_limit_reset_epoch_sec: Some(now_sec + 3600),
        })),
    )?;
    scripted.push_route(
        "/repos/acme/payments-core/issues/comments",
        Ok(Some(GitHubConditionalPage {
            body: json!([
                {
                    "id": 702,
                    "body": "Updated: hotfix approved by release lead.",
                    "user": { "login": "lead-wang" },
                    "updated_at": "2026-11-20T09:00:00Z"
                }
            ]),
            etag: Some("W/\"ic-2\"".into()),
            rate_limit_remaining: Some(4895),
            rate_limit_reset_epoch_sec: Some(now_sec + 3600),
        })),
    )?;

    let gh_connector = GitHubConnector::new(
        scripted,
        GitHubSyncConfig {
            now: EpochMs(1_795_245_380_000),
            initial_backfill_ms: 30 * 86_400 * 1_000,
            per_page: 2,
            max_pages_per_stream: 1,
        },
    );
    let scope = ConnectorScope {
        connector_id: "github_main".parse()?,
        source_scope_id: "github.repo.acme.payments_core".parse()?,
        upstream_container_id: "acme/payments-core".into(),
        label: "payments-core".into(),
        enabled: true,
        model_policy: ConnectorKind::GitHub.default_model_policy(),
    };
    let alice: ActorId = "eng_alice".parse()?;

    let b1 = gh_connector
        .poll_incremental(&scope, &ConnectorCursor::default(), &alice)
        .await?;
    assert!(!b1.scope_rosters[0].all_library_members);
    assert_eq!(
        b1.scope_rosters[0].external_member_ids,
        vec!["eng-alice".to_owned(), "lead-wang".to_owned()]
    );
    assert_eq!(b1.skipped_low_signal, 1);
    assert!(b1
        .events
        .iter()
        .any(|e| e.external_key == "gh:acme/payments-core:pr:42"));
    assert!(b1
        .events
        .iter()
        .any(|e| e.external_key == "gh:acme/payments-core:issue_comments:702"));

    let b2 = gh_connector
        .poll_incremental(&scope, &b1.next_cursor, &alice)
        .await?;
    assert!(b2.events.iter().any(|e| {
        e.external_key == "gh:acme/payments-core:issue:44" && e.event_op == SourceEventOp::Upsert
    }));
    assert!(b2.events.iter().any(|e| {
        e.external_key == "gh:acme/payments-core:issue_comments:702@rev1"
            && e.event_op
                == SourceEventOp::Supersede {
                    previous_external_key: "gh:acme/payments-core:issue_comments:702".into(),
                }
    }));

    let pub_scripted = ScriptedGitHubTransport::default();
    pub_scripted.push_route(
        "/repos/acme/open-sdk",
        Ok(Some(GitHubConditionalPage {
            body: load_fixture_json("github/repo_public.json")?,
            etag: Some("W/\"pub\"".into()),
            rate_limit_remaining: Some(4999),
            rate_limit_reset_epoch_sec: Some(now_sec + 3600),
        })),
    )?;
    let pub_connector = GitHubConnector::new(pub_scripted, GitHubSyncConfig::default());
    let pub_scope = ConnectorScope {
        connector_id: "github_pub".parse()?,
        source_scope_id: "github.repo.acme.open_sdk".parse()?,
        upstream_container_id: "acme/open-sdk".into(),
        label: "open-sdk".into(),
        enabled: true,
        model_policy: ModelRoutePolicy::HostCli,
    };
    let pub_batch = pub_connector
        .poll_incremental(&pub_scope, &ConnectorCursor::default(), &alice)
        .await?;
    assert!(pub_batch.scope_rosters[0].all_library_members);
    let resolved_pub = pub_batch.scope_rosters[0].into_source_scope(&BTreeMap::new());
    assert_eq!(resolved_pub.current_audience, AudienceSpec::AllMembers);

    Ok(())
}

#[tokio::test]
async fn historical_sessions_strip_meta_and_harness_blocks_and_handoff_offsets() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let cc_dir = temp.path().join("claude/projects/demo");
    let codex_dir = temp.path().join("codex/2026/11/20");
    let agy_dir = temp
        .path()
        .join("antigravity/brain/conv-123/.system_generated/logs");
    fs::create_dir_all(&cc_dir)?;
    fs::create_dir_all(&codex_dir)?;
    fs::create_dir_all(&agy_dir)?;

    let cc_file = cc_dir.join("sess1.jsonl");
    let codex_file = codex_dir.join("rollout-2026-11-20.jsonl");
    let agy_file = agy_dir.join("transcript.jsonl");
    let agy_full_file = agy_dir.join("transcript_full.jsonl");

    fs::copy(
        fixtures_dir().join("sessions/claude_code_session.jsonl"),
        &cc_file,
    )?;
    fs::copy(
        fixtures_dir().join("sessions/codex_rollout.jsonl"),
        &codex_file,
    )?;
    fs::copy(
        fixtures_dir().join("sessions/antigravity_transcript.jsonl"),
        &agy_file,
    )?;
    fs::copy(
        fixtures_dir().join("sessions/antigravity_transcript.jsonl"),
        &agy_full_file,
    )?;

    let connector = AgentSessionConnector::new(SessionBackfillConfig {
        now: EpochMs(1_795_245_380_000),
        initial_backfill_ms: 30 * 86_400 * 1_000,
        max_files_per_poll: 50,
    });
    let scope = ConnectorScope {
        connector_id: "sessions_local".parse()?,
        source_scope_id: "session.local.alice".parse()?,
        upstream_container_id: temp.path().to_string_lossy().into_owned(),
        label: "Local Agent Sessions".into(),
        enabled: true,
        model_policy: ConnectorKind::AgentSession.default_model_policy(),
    };
    let alice: ActorId = "alice".parse()?;

    let batch_1 = connector
        .poll_incremental(&scope, &ConnectorCursor::default(), &alice)
        .await?;

    assert_eq!(batch_1.events.len(), 3);
    assert_eq!(batch_1.skipped_low_signal, 3);

    for ev in &batch_1.events {
        assert!(!ev.content.contains("<kp-context>"));
        assert!(!ev.content.contains("[kp-ref:"));
        assert!(!ev.content.contains("<system-reminder>"));
        assert!(!ev.content.contains("<command-message>"));
        assert!(!ev.content.contains("<command-name>"));
        assert!(!ev.content.contains("Caveat: The messages below"));
        assert!(!ev.content.contains("<environment_context>"));
        assert!(!ev.content.contains("<guardian_tool_descriptions>"));
        assert!(!ev.content.contains("AGENTS.md instructions"));
        assert!(!ev.content.contains("<SYSTEM_MESSAGE>"));
        assert!(!ev.content.contains("<CONTEXT_SUMMARY>"));
        assert!(!ev.content.contains("prior compacted history"));
        assert!(!ev.content.contains("<USER_REQUEST>"));
        assert!(!ev.content.contains("Showing lines 1-10"));
        assert!(!ev.content.contains("sk-ant-secret1234567890"));
        assert!(!ev.content.contains("huge raw stdout ignored"));
        assert!(!ev.content.contains("internal reasoning"));
    }

    assert!(batch_1.events.iter().any(|e| {
        e.content
            .contains("本周六读书会改在西湖书店举办，差旅酒店报销上限每晚六百元。")
            && e.content.contains("[REDACTED]")
    }));
    assert!(batch_1.events.iter().any(|e| {
        e.content
            .contains("[speaker:alice] 下周四凌晨两点数据库主从切换")
            && e.content
                .contains("[assistant] 收到，已同步数据库主从切换时间窗。")
    }));
    assert!(batch_1.events.iter().any(|e| {
        e.content.contains(
            "[speaker:alice] 主从切换窗口禁止跑批处理任务，日常统计脚本等切换结束后再跑。",
        ) && e
            .content
            .contains("[assistant] 我会在启动批处理脚本前先确认主从切换是否已经完成。")
    }));

    let cp_1 = SessionBackfillCheckpoint::from_cursor(&batch_1.next_cursor)?;
    let cc_key = cc_file.to_string_lossy().into_owned();
    let cc_cursor = cp_1
        .handoff_cursors
        .get(&cc_key)
        .ok_or_else(|| ConnectError::Transport("missing claude handoff cursor".into()))?;
    assert_eq!(cc_cursor.client_kind, SessionClientKind::ClaudeCode);
    assert_eq!(cc_cursor.session_id, "cc-sess-001");
    assert_eq!(cc_cursor.last_byte_offset, fs::metadata(&cc_file)?.len());
    assert!(!cc_cursor.prefix_sha256.is_empty());

    {
        let mut f = OpenOptions::new().append(true).open(&cc_file)?;
        f.write_all(
            r#"{"type":"user","sessionId":"cc-sess-001","timestamp":"2026-11-20T09:05:00Z","message":{"role":"user","content":"读书会提前到下午两点开始"#
                .as_bytes(),
        )?;
    }
    let mid_write = read_session_file_incremental(
        &cc_file,
        Some(SessionClientKind::ClaudeCode),
        Some(cc_cursor),
        EpochMs(1_795_245_400_000),
    )?;
    assert!(mid_write.turns.is_empty());
    assert_eq!(
        mid_write.cursor.last_byte_offset,
        cc_cursor.last_byte_offset
    );

    {
        let mut f = OpenOptions::new().append(true).open(&cc_file)?;
        f.write_all(b"\"}}\n")?;
    }
    let batch_2 = connector
        .poll_incremental(&scope, &batch_1.next_cursor, &alice)
        .await?;
    assert_eq!(batch_2.events.len(), 1);
    assert_eq!(
        batch_2.events[0].content,
        "[speaker:alice] 读书会提前到下午两点开始"
    );
    assert_eq!(batch_2.events[0].event_op, SourceEventOp::Upsert);

    fs::write(
        &cc_file,
        r#"{"type":"user","sessionId":"cc-sess-001","timestamp":"2026-11-20T09:10:00Z","message":{"role":"user","content":"会话重写后只保留最新结论"}}
"#,
    )?;
    let batch_3 = connector
        .poll_incremental(&scope, &batch_2.next_cursor, &alice)
        .await?;
    assert_eq!(batch_3.events.len(), 1);
    assert!(matches!(
        batch_3.events[0].event_op,
        SourceEventOp::Supersede { .. }
    ));

    Ok(())
}

#[tokio::test]
async fn connector_state_store_and_public_ingest_jsonl_contract_decouple_from_store() -> Result<()>
{
    let temp = tempfile::tempdir()?;
    let state_file = temp.path().join(".kp/connectors/state.json");
    let state_store = ConnectorStateStore::new(&state_file);

    let transport = ScriptedFeishuTransport::default();
    transport.push_chat_poll(
        "oc_pm_release",
        load_fixture_json("feishu/pm_chat_members_initial.json")?,
        load_fixture_json("feishu/pm_chat_messages_initial.json")?,
    )?;
    transport.push_chat_poll(
        "oc_pm_release",
        load_fixture_json("feishu/pm_chat_members_after_departure.json")?,
        load_fixture_json("feishu/pm_chat_messages_rescan_edit.json")?,
    )?;
    transport.push_chat_poll(
        "oc_pm_release",
        load_fixture_json("feishu/pm_chat_members_after_departure.json")?,
        load_fixture_json("feishu/pm_chat_messages_rescan_recall.json")?,
    )?;

    let connector = FeishuConnector::new(
        transport,
        FeishuSyncConfig {
            now: EpochMs(1_795_245_380_000),
            ..FeishuSyncConfig::default()
        },
    );

    let importer_alice: ActorId = "eng_alice".parse()?;
    let pm_li: ActorId = "pm_li".parse()?;
    let confirmed = BTreeMap::from([
        (
            ("feishu".into(), "ou_eng_alice".into()),
            importer_alice.clone(),
        ),
        (("feishu".into(), "ou_pm_li".into()), pm_li.clone()),
    ]);

    let pm_scope = ConnectorScope {
        connector_id: "feishu_main".parse()?,
        source_scope_id: "feishu.chat.oc_pm_release".parse()?,
        upstream_container_id: "chat:oc_pm_release".into(),
        label: "运维主从切换群".into(),
        enabled: true,
        model_policy: ConnectorKind::Feishu.default_model_policy(),
    };

    // Poll 1: persists cursor in `state.json` and emits roster + `upsert` JSONL lines.
    let b1 = state_store
        .poll_and_persist(&connector, &pm_scope, &importer_alice)
        .await?;
    assert!(state_file.is_file());
    let jsonl_1 = b1.to_public_jsonl_string(ConnectorKind::Feishu, &confirmed)?;
    let parsed_1 = parse_public_jsonl_str(&jsonl_1)?;
    assert_eq!(parsed_1.len(), 2);
    match &parsed_1[0] {
        PublicIngestLine::ContainerRoster(r) => {
            assert_eq!(r.container_id, "feishu.chat.oc_pm_release");
            assert_eq!(r.label, "运维主从切换群");
            assert!(!r.all_library_members);
            assert!(r.members.contains(&"eng_alice".to_owned()));
            assert!(r.members.contains(&"pm_li".to_owned()));
            assert!(r.members.contains(&"feishu:ou_intern_bob".to_owned()));
        }
        other => panic!("expected ContainerRoster first, got {other:?}"),
    }
    match &parsed_1[1] {
        PublicIngestLine::Record(rec) => {
            assert_eq!(rec.source.kind, "feishu");
            assert_eq!(rec.source.external_id, "om_pm_1001");
            assert_eq!(
                rec.source.container_id.as_deref(),
                Some("feishu.chat.oc_pm_release")
            );
            assert_eq!(rec.speaker.id, "pm_li");
            assert_eq!(rec.speaker.display_name.as_deref(), Some("李四"));
            assert_eq!(rec.audience, PublicAudience::ContainerMembers);
            assert_eq!(rec.content, "下周四凌晨两点数据库主从切换");
            assert_eq!(rec.op, PublicIngestOp::Upsert);
            assert_eq!(rec.ingested_by, Some(importer_alice.clone()));
            assert_eq!(rec.model_policy, None);
        }
        other => panic!("expected Record second, got {other:?}"),
    }

    // Reopen `ConnectorStateStore` from disk (simulating fresh CLI invocation) -> Poll 2 emits narrowed roster + `supersede`.
    let reopened_store = ConnectorStateStore::new(&state_file);
    let b2 = reopened_store
        .poll_and_persist(&connector, &pm_scope, &importer_alice)
        .await?;
    let jsonl_2 = b2.to_public_jsonl_string(ConnectorKind::Feishu, &confirmed)?;
    let parsed_2 = parse_public_jsonl_str(&jsonl_2)?;
    assert_eq!(parsed_2.len(), 2);
    match &parsed_2[0] {
        PublicIngestLine::ContainerRoster(r) => {
            assert!(!r.members.contains(&"feishu:ou_intern_bob".to_owned()));
        }
        other => panic!("expected narrowed ContainerRoster, got {other:?}"),
    }
    match &parsed_2[1] {
        PublicIngestLine::Record(rec) => {
            assert_eq!(
                rec.op,
                PublicIngestOp::Supersede {
                    previous_external_id: "om_pm_1001".into()
                }
            );
            assert!(rec.content.contains("凌晨三点"));
        }
        other => panic!("expected Supersede record, got {other:?}"),
    }

    // Poll 3 with explicit `RawOnly` override -> emits `withdraw` + explicit `model_policy: RawOnly`.
    let raw_override_scope = ConnectorScope {
        model_policy: ModelRoutePolicy::RawOnly,
        ..pm_scope
    };
    let b3 = reopened_store
        .poll_and_persist(&connector, &raw_override_scope, &importer_alice)
        .await?;
    let jsonl_3 = b3.to_public_jsonl_string(ConnectorKind::Feishu, &confirmed)?;
    let parsed_3 = parse_public_jsonl_str(&jsonl_3)?;
    assert_eq!(parsed_3.len(), 2);
    match &parsed_3[1] {
        PublicIngestLine::Record(rec) => {
            assert_eq!(rec.op, PublicIngestOp::Withdraw);
            assert_eq!(rec.model_policy, Some(ModelRoutePolicy::RawOnly));
        }
        other => panic!("expected Withdraw record, got {other:?}"),
    }

    Ok(())
}

#[cfg(unix)]
fn write_executable_script(path: &Path, body: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::write(path, body)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    Ok(())
}

#[cfg(unix)]
#[test]
fn kp_connector_cli_feishu_github_session_staged_sync_and_cursor_rollback_e2e() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    // 0. Architectural red-line check: `kp-connect/Cargo.toml` has ZERO `kp-store` or `rusqlite` dependency!
    let cargo_toml = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))?;
    assert!(
        !cargo_toml.contains("kp-store") && !cargo_toml.contains("rusqlite"),
        "kp-connect/Cargo.toml must have zero kp-store or rusqlite dependencies"
    );

    let temp = tempfile::tempdir()?;
    let state_path = temp.path().join("connectors/state.json");
    let lib_dir = temp.path().join("mock-library");
    fs::create_dir_all(&lib_dir)?;

    let sessions_dir = temp.path().join("sessions/.codex");
    fs::create_dir_all(&sessions_dir)?;
    let session_file = sessions_dir.join("rollout-backfill-01.jsonl");
    let initial_session_lines = concat!(
        r#"{"type":"session_meta","payload":{"id":"sess_m3_01","timestamp":"2026-11-21T07:00:00Z"}}"#,
        "\n",
        r#"{"type":"event_msg","timestamp":"2026-11-21T07:01:00Z","payload":{"type":"user_message","message":"好的"}}"#,
        "\n",
        r#"{"type":"event_msg","timestamp":"2026-11-21T07:02:00Z","payload":{"type":"user_message","message":"发版冻结窗口禁止手动重启核心队列消费者。"}}"#,
        "\n",
    );
    fs::write(&session_file, initial_session_lines)?;
    let first_slice_bytes = initial_session_lines.len();

    // Build real executable `mock_lark_cli.sh` serving members + Wave 1 upsert -> Wave 2 edit (`supersede`) -> Wave 3 recall (`withdraw`).
    let fixtures = fixtures_dir().join("feishu");
    let lark_wave_file = temp.path().join("lark_wave.txt");
    fs::write(&lark_wave_file, "1")?;
    let mock_lark = temp.path().join("mock-lark-cli");
    write_executable_script(
        &mock_lark,
        &format!(
            r#"#!/bin/sh
set -eu
WAVE=$(cat "{wave}")
case "$*" in
  *"im chat.members get"*)
    if [ "$WAVE" = "1" ]; then
      cat "{f_mem1}"
    else
      cat "{f_mem2}"
    fi
    ;;
  *"im +chat-messages-list"*)
    if [ "$WAVE" = "1" ]; then
      cat "{f_msg1}"
    elif [ "$WAVE" = "2" ]; then
      cat "{f_msg2}"
    else
      cat "{f_msg3}"
    fi
    ;;
  *)
    echo '{{"ok":true,"data":{{"items":[],"has_more":false}}}}'
    ;;
esac
"#,
            wave = lark_wave_file.display(),
            f_mem1 = fixtures.join("pm_chat_members_initial.json").display(),
            f_mem2 = fixtures
                .join("pm_chat_members_after_departure.json")
                .display(),
            f_msg1 = fixtures.join("pm_chat_messages_initial.json").display(),
            f_msg2 = fixtures.join("pm_chat_messages_rescan_edit.json").display(),
            f_msg3 = fixtures
                .join("pm_chat_messages_rescan_recall.json")
                .display(),
        ),
    )?;

    // Build real executable `mock_gh.sh` responding to read-only `gh api --method GET --include /repos/...`.
    let mock_gh = temp.path().join("mock-gh");
    write_executable_script(
        &mock_gh,
        r#"#!/bin/sh
set -eu
LAST=""
for arg in "$@"; do LAST="$arg"; done
printf "HTTP/2.0 200 OK\r\nETag: \"etag-v1\"\r\nX-RateLimit-Remaining: 4999\r\n\r\n"
case "$LAST" in
  "/repos/org-alpha/core-engine")
    echo '{"private":true,"visibility":"private","owner":{"login":"lead_zhang"}}'
    ;;
  /repos/org-alpha/core-engine/collaborators*)
    echo '[{"login":"lead_zhang","permissions":{"pull":true}},{"login":"eng_alice_gh","permissions":{"pull":true}}]'
    ;;
  /repos/org-alpha/core-engine/issues\?*)
    echo '[{"number":42,"title":"Freeze non-rollback merges during replica drill","body":"Hold merges until replica promotion finishes.","updated_at":"2026-11-21T07:05:00Z","user":{"login":"lead_zhang"},"html_url":"https://example.invalid/org-alpha/core-engine/issues/42"}]'
    ;;
  /repos/org-alpha/core-engine/issues/comments*)
    echo '[{"id":9001,"body":"LGTM","updated_at":"2026-11-21T07:06:00Z","user":{"login":"eng_alice_gh"}}]'
    ;;
  *)
    echo '[]'
    ;;
esac
"#,
    )?;

    let mock_kp_fail = temp.path().join("mock-kp-fail");
    write_executable_script(
        &mock_kp_fail,
        "#!/bin/sh\necho 'simulated downstream ingest failure' >&2\nexit 1\n",
    )?;

    let captured_pipe = temp.path().join("captured_pipe.jsonl");
    let captured_args = temp.path().join("captured_kp_args.txt");
    let mock_kp_ok = temp.path().join("mock-kp-ok");
    write_executable_script(
        &mock_kp_ok,
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" > \"{args}\"\ncat > \"{cap}\"\necho '{{\"records_upserted\":3}}'\n",
            args = captured_args.display(),
            cap = captured_pipe.display(),
        ),
    )?;

    let bin = env!("CARGO_BIN_EXE_kp-connector");

    // 1. `scope add` for Feishu (default inherit -> emits one-line `feishu_enable_notice` on stderr!),
    //    GitHub (`named_endpoint:corp-gw`), and AgentSession (`raw_only`).
    let add_feishu = Command::new(bin)
        .arg("--state")
        .arg(&state_path)
        .arg("--json")
        .args([
            "scope",
            "add",
            "--kind",
            "feishu",
            "--scope-id",
            "feishu.chat.oc_pm_release",
            "--upstream",
            "chat:oc_pm_release",
            "--label",
            "运维主从切换群",
            "--global-model-label",
            "Codex / OpenAI",
        ])
        .output()?;
    assert!(
        add_feishu.status.success(),
        "feishu scope add failed: {}",
        String::from_utf8_lossy(&add_feishu.stderr)
    );
    let feishu_stderr = String::from_utf8_lossy(&add_feishu.stderr);
    assert_eq!(feishu_stderr.trim(), feishu_enable_notice("Codex / OpenAI"));
    assert_eq!(
        fs::metadata(&state_path)?.permissions().mode() & 0o777,
        0o600
    );

    let add_gh = Command::new(bin)
        .arg("--state")
        .arg(&state_path)
        .args([
            "scope",
            "add",
            "--kind",
            "github",
            "--scope-id",
            "github.repo.core_engine",
            "--upstream",
            "org-alpha/core-engine",
            "--label",
            "核心引擎仓库",
            "--model-policy",
            "named_endpoint:corp-gw",
        ])
        .output()?;
    assert!(add_gh.status.success());
    assert!(
        add_gh.stderr.is_empty(),
        "non-feishu scope add must not emit feishu notice"
    );

    let add_sess = Command::new(bin)
        .arg("--state")
        .arg(&state_path)
        .args([
            "scope",
            "add",
            "--kind",
            "agent-session",
            "--scope-id",
            "session.codex_local",
            "--upstream",
            &sessions_dir.to_string_lossy(),
            "--label",
            "本地会话回填",
            "--model-policy",
            "raw_only",
        ])
        .output()?;
    assert!(add_sess.status.success());

    // Verify `scope list`, `scope disable`, and `scope enable`.
    let disable_out = Command::new(bin)
        .arg("--state")
        .arg(&state_path)
        .args(["scope", "disable", "github", "github.repo.core_engine"])
        .output()?;
    assert!(disable_out.status.success());
    let list_disabled: Value = serde_json::from_slice(
        &Command::new(bin)
            .arg("--state")
            .arg(&state_path)
            .args(["--json", "scope", "list"])
            .output()?
            .stdout,
    )?;
    assert_eq!(list_disabled["scopes"].as_array().map(Vec::len), Some(3));
    let reenable_out = Command::new(bin)
        .arg("--state")
        .arg(&state_path)
        .args(["scope", "enable", "github", "github.repo.core_engine"])
        .output()?;
    assert!(reenable_out.status.success());

    // 2. Downstream `kp ingest` failure (`--pipe-kp mock_kp_fail`) MUST keep `state.json` cursors unadvanced!
    let failed_sync = Command::new(bin)
        .arg("--state")
        .arg(&state_path)
        .env("KP_LARK_CLI_BIN", &mock_lark)
        .env(ENV_GH_CLI_BIN, &mock_gh)
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .env_remove(ENV_GITHUB_API_BASE)
        .args([
            "sync",
            "--ingested-by",
            "eng_alice",
            "--now",
            "1795245380000",
            "--pipe-kp",
        ])
        .arg(&mock_kp_fail)
        .arg("--library")
        .arg(&lib_dir)
        .output()?;
    assert!(
        !failed_sync.status.success(),
        "sync --pipe-kp must fail when downstream kp ingest fails"
    );
    assert!(
        ConnectorStateStore::new(&state_path)
            .load()?
            .cursors
            .is_empty(),
        "cursors in state.json must NOT advance when downstream kp ingest fails"
    );

    // 3. Raw stdout streaming without `--commit-cursors` emits valid JSONL across all 3 connectors and leaves cursors untouched.
    let stdout_sync = Command::new(bin)
        .arg("--state")
        .arg(&state_path)
        .env("KP_LARK_CLI_BIN", &mock_lark)
        .env(ENV_GH_CLI_BIN, &mock_gh)
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .env_remove(ENV_GITHUB_API_BASE)
        .args([
            "sync",
            "--ingested-by",
            "eng_alice",
            "--identity",
            "feishu:ou_pm_li=pm_li",
            "--now",
            "1795245380000",
        ])
        .output()?;
    assert!(stdout_sync.status.success());
    let stdout_lines = parse_public_jsonl_str(&String::from_utf8_lossy(&stdout_sync.stdout))?;
    assert_eq!(
        stdout_lines.len(),
        6,
        "expected 3 rosters + 3 records across feishu, github, and agent-session"
    );
    assert!(
        ConnectorStateStore::new(&state_path)
            .load()?
            .cursors
            .is_empty(),
        "stdout sync without --commit-cursors must not mutate state.json cursors"
    );

    // 4. Successful `--pipe-kp mock_kp_ok` pipes valid JSONL to `kp --path <lib> --as eng_alice --json ingest -`
    //    and atomically commits all 3 cursors to `0600` `state.json`!
    let ok_sync = Command::new(bin)
        .arg("--state")
        .arg(&state_path)
        .arg("--json")
        .env("KP_LARK_CLI_BIN", &mock_lark)
        .env(ENV_GH_CLI_BIN, &mock_gh)
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .env_remove(ENV_GITHUB_API_BASE)
        .args([
            "sync",
            "--ingested-by",
            "eng_alice",
            "--identity",
            "feishu:ou_pm_li=pm_li",
            "--now",
            "1795245380000",
            "--pipe-kp",
        ])
        .arg(&mock_kp_ok)
        .arg("--library")
        .arg(&lib_dir)
        .output()?;
    assert!(
        ok_sync.status.success(),
        "sync --pipe-kp failed: {}",
        String::from_utf8_lossy(&ok_sync.stderr)
    );
    let sync_summary: Value = serde_json::from_slice(&ok_sync.stdout)?;
    assert_eq!(sync_summary["scopes_polled"], 3);
    assert_eq!(sync_summary["rosters_emitted"], 3);
    assert_eq!(sync_summary["records_emitted"], 3);
    assert_eq!(sync_summary["cursors_committed"], 3);
    assert!(sync_summary["skipped_low_signal"].as_u64().unwrap_or(0) >= 2);
    assert_eq!(
        fs::metadata(&state_path)?.permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        ConnectorStateStore::new(&state_path).load()?.cursors.len(),
        3
    );

    let kp_args_seen = fs::read_to_string(&captured_args)?;
    assert!(kp_args_seen.contains("--as eng_alice"));
    assert!(kp_args_seen.contains("ingest -"));

    let wave1_piped = parse_public_jsonl_str(&fs::read_to_string(&captured_pipe)?)?;
    assert_eq!(wave1_piped.len(), 6);
    let mut saw_feishu_inherit = false;
    let mut saw_gh_endpoint = false;
    let mut saw_session_raw = false;
    for item in &wave1_piped {
        if let PublicIngestLine::Record(rec) = item {
            match rec.source.kind.as_str() {
                "feishu" => {
                    assert_eq!(rec.speaker.id, "pm_li");
                    assert_eq!(rec.op, PublicIngestOp::Upsert);
                    assert_eq!(rec.model_policy, None);
                    saw_feishu_inherit = true;
                }
                "github" => {
                    assert_eq!(rec.op, PublicIngestOp::Upsert);
                    assert_eq!(
                        rec.model_policy,
                        Some(ModelRoutePolicy::NamedEndpoint("corp-gw".into()))
                    );
                    saw_gh_endpoint = true;
                }
                "agent-session" => {
                    assert_eq!(rec.op, PublicIngestOp::Upsert);
                    assert_eq!(rec.model_policy, Some(ModelRoutePolicy::RawOnly));
                    assert!(rec
                        .content
                        .contains("发版冻结窗口禁止手动重启核心队列消费者"));
                    saw_session_raw = true;
                }
                other => panic!("unexpected source.kind {other}"),
            }
        }
    }
    assert!(saw_feishu_inherit && saw_gh_endpoint && saw_session_raw);

    // 5. Wave 2: Feishu message edit (`updated: true` -> `supersede` + narrowed roster) & incremental session append!
    fs::write(&lark_wave_file, "2")?;
    let mut f_append = OpenOptions::new().append(true).open(&session_file)?;
    writeln!(
        f_append,
        r#"{{"type":"event_msg","timestamp":"2026-11-21T07:10:00Z","payload":{{"type":"user_message","message":"补充：灰度阶段观察五分钟延迟指标。"}}}}"#
    )?;
    drop(f_append);

    let wave2_out = Command::new(bin)
        .arg("--state")
        .arg(&state_path)
        .env("KP_LARK_CLI_BIN", &mock_lark)
        .env(ENV_GH_CLI_BIN, &mock_gh)
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .env_remove(ENV_GITHUB_API_BASE)
        .args([
            "sync",
            "--ingested-by",
            "eng_alice",
            "--now",
            "1795245440000",
            "--commit-cursors",
        ])
        .output()?;
    assert!(wave2_out.status.success());
    let wave2_lines = parse_public_jsonl_str(&String::from_utf8_lossy(&wave2_out.stdout))?;
    assert!(wave2_lines.iter().any(|l| matches!(
        l,
        PublicIngestLine::Record(r)
            if r.source.kind == "feishu"
                && r.op == PublicIngestOp::Supersede {
                    previous_external_id: "om_pm_1001".into()
                }
    )));
    assert!(wave2_lines.iter().any(|l| matches!(
        l,
        PublicIngestLine::Record(r)
            if r.source.kind == "agent-session"
                && r.source.uri.contains(&format!("bytes={first_slice_bytes}.."))
                && r.content.contains("灰度阶段观察五分钟延迟指标")
    )));

    // 6. Wave 3: Feishu message recall (`deleted: true` -> `withdraw`).
    fs::write(&lark_wave_file, "3")?;
    let wave3_out = Command::new(bin)
        .arg("--state")
        .arg(&state_path)
        .env("KP_LARK_CLI_BIN", &mock_lark)
        .args([
            "sync",
            "--ingested-by",
            "eng_alice",
            "--kind",
            "feishu",
            "--now",
            "1795245500000",
            "--commit-cursors",
        ])
        .output()?;
    assert!(wave3_out.status.success());
    let wave3_lines = parse_public_jsonl_str(&String::from_utf8_lossy(&wave3_out.stdout))?;
    assert!(wave3_lines.iter().any(|l| matches!(
        l,
        PublicIngestLine::Record(r)
            if r.source.kind == "feishu"
                && r.source.external_id == "om_pm_1001@rev1"
                && r.op == PublicIngestOp::Withdraw
    )));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn github_api_base_override_enforces_readonly_get_and_serves_local_http_mock() -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // 1. Strict URL & read-only method validation holds regardless of `KP_GITHUB_API_BASE`.
    assert_eq!(
        validate_github_api_base_url("http://127.0.0.1:18080/")?,
        "http://127.0.0.1:18080"
    );
    assert_eq!(
        validate_github_api_base_url("https://api.github.com/")?,
        "https://api.github.com"
    );
    assert!(validate_github_api_base_url("ftp://127.0.0.1:8080").is_err());
    assert!(validate_github_api_base_url("http://token@127.0.0.1:8080").is_err());
    assert!(validate_github_api_base_url("http://127.0.0.1:8080?mut=1").is_err());
    assert!(validate_github_readonly_endpoint("/repos/org/repo/issues/1/labels").is_err());
    for forbidden_method in ["POST", "PUT", "PATCH", "DELETE"] {
        assert!(validate_gh_cli_readonly_args(&[
            "api".into(),
            "--method".into(),
            forbidden_method.into(),
            "--include".into(),
            "/repos/org/repo/issues".into(),
        ])
        .is_err());
    }

    // 2. Bind local `127.0.0.1:0` read-only HTTP server and run `kp-connector` with `KP_GITHUB_API_BASE`.
    let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
        Ok(l) => l,
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    let base_url = format!("http://{}", listener.local_addr()?);
    let recorded_req_lines = Arc::new(Mutex::new(Vec::<String>::new()));
    let req_lines_srv = Arc::clone(&recorded_req_lines);

    let srv_task = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let req_log = Arc::clone(&req_lines_srv);
            tokio::spawn(async move {
                let mut buf = vec![0_u8; 8192];
                let mut used = 0_usize;
                loop {
                    let Ok(n) = stream.read(&mut buf[used..]).await else {
                        return;
                    };
                    if n == 0 {
                        break;
                    }
                    used += n;
                    if buf[..used].windows(4).any(|w| w == b"\r\n\r\n") || used == buf.len() {
                        break;
                    }
                }
                let req_str = String::from_utf8_lossy(&buf[..used]);
                let first_line = req_str.lines().next().unwrap_or("").to_owned();
                if let Ok(mut g) = req_log.lock() {
                    g.push(first_line.clone());
                }
                let path = first_line.split_whitespace().nth(1).unwrap_or("");
                let body = if path == "/repos/org-local/http-repo" {
                    r#"{"private":false,"visibility":"public","owner":{"login":"maint_chen"}}"#
                } else if path.starts_with("/repos/org-local/http-repo/issues?") {
                    r#"[{"number":7,"title":"Verify local HTTP GitHub sync","body":"Read-only GET over KP_GITHUB_API_BASE.","updated_at":"2026-11-21T08:00:00Z","user":{"login":"maint_chen"}}]"#
                } else {
                    "[]"
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes()).await;
            });
        }
    });

    let temp = tempfile::tempdir()?;
    let state_path = temp.path().join("gh-http-state.json");
    let bin = env!("CARGO_BIN_EXE_kp-connector");

    let add_out = Command::new(bin)
        .arg("--state")
        .arg(&state_path)
        .args([
            "scope",
            "add",
            "--kind",
            "github",
            "--scope-id",
            "github.repo.http_local",
            "--upstream",
            "org-local/http-repo",
            "--label",
            "本地回环测试仓",
        ])
        .output()?;
    assert!(add_out.status.success());

    let sync_out = Command::new(bin)
        .arg("--state")
        .arg(&state_path)
        .env(ENV_GITHUB_API_BASE, &base_url)
        .args([
            "sync",
            "--ingested-by",
            "eng_alice",
            "--now",
            "1795248000000",
            "--commit-cursors",
        ])
        .output()?;
    srv_task.abort();

    assert!(
        sync_out.status.success(),
        "KP_GITHUB_API_BASE sync failed: {}",
        String::from_utf8_lossy(&sync_out.stderr)
    );
    let seen = recorded_req_lines
        .lock()
        .map_err(|_| ConnectError::Transport("lock".into()))?
        .clone();
    assert!(
        !seen.is_empty(),
        "local GitHub HTTP mock server must receive non-empty GET requests"
    );
    for line in &seen {
        assert!(
            line.starts_with("GET /repos/org-local/http-repo"),
            "unexpected non-GET or out-of-scope request: {line}"
        );
    }

    let parsed = parse_public_jsonl_str(&String::from_utf8_lossy(&sync_out.stdout))?;
    assert_eq!(parsed.len(), 2);
    assert!(matches!(
        &parsed[0],
        PublicIngestLine::ContainerRoster(r) if r.all_library_members && r.container_id == "github.repo.http_local"
    ));
    assert!(matches!(
        &parsed[1],
        PublicIngestLine::Record(rec)
            if rec.source.kind == "github"
                && rec.op == PublicIngestOp::Upsert
                && rec.content.contains("#7 Verify local HTTP GitHub sync")
    ));

    Ok(())
}
