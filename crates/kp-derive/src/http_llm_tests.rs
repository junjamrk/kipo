use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use kp_model::{
    CalendarTable, EntailmentQuery, EntailmentVerdict, EpochMs, ExtractionBatchOutcome,
    ExtractionBatchRequest, ExtractionChunkSlice, FactKind, KpError, LlmClient, ModelRoutePolicy,
    TimeZone,
};
use serde_json::{json, Value};

use super::*;
use crate::llm_runner::{HostCliConfig, HostCliKind, HostCliRunner};

type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

type MockHttpResponder = Box<dyn Fn(&HttpJsonRequest) -> Result<HttpJsonResponse> + Send + Sync>;

struct RecordingMockHttpTransport {
    calls: Mutex<Vec<HttpJsonRequest>>,
    responder: MockHttpResponder,
}

impl RecordingMockHttpTransport {
    fn new(
        responder: impl Fn(&HttpJsonRequest) -> Result<HttpJsonResponse> + Send + Sync + 'static,
    ) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            responder: Box::new(responder),
        }
    }

    fn recorded(&self) -> Result<Vec<HttpJsonRequest>> {
        self.calls
            .lock()
            .map(|g| g.clone())
            .map_err(|_| KpError::invalid("mock_http", "poisoned lock"))
    }
}

impl HttpJsonTransport for RecordingMockHttpTransport {
    fn post_json<'a>(
        &'a self,
        req: &'a HttpJsonRequest,
    ) -> BoxFuture<'a, Result<HttpJsonResponse>> {
        Box::pin(async move {
            self.calls
                .lock()
                .map_err(|_| KpError::invalid("mock_http", "poisoned lock"))?
                .push(req.clone());
            (self.responder)(req)
        })
    }
}

fn sample_batch_request(body: &str) -> Result<ExtractionBatchRequest> {
    let shanghai =
        TimeZone::get("Asia/Shanghai").map_err(|_| KpError::invalid("tz", "Asia/Shanghai"))?;
    let calendar = CalendarTable::from_reference_in_tz(EpochMs(1_795_089_600_000), &shanghai)?;
    Ok(ExtractionBatchRequest {
        source_id: "src_http_test".parse()?,
        speaker: "alice".parse()?,
        reference_calendar: calendar,
        active_concepts: Vec::new(),
        active_decisions: Vec::new(),
        speaker_own_facts: Vec::new(),
        speaker_own_facts_by_actor: BTreeMap::new(),
        chunks: vec![ExtractionChunkSlice {
            chunk_id: "c0".into(),
            heading: "note".into(),
            body: body.into(),
        }],
        min_rate_limit_remaining_pct: 5,
    })
}

fn valid_wire_envelope_json(chunk_id: &str, quote: &str) -> Value {
    json!({
        "skip_summary": false,
        "atoms": [
            {
                "kind": "event",
                "claim_text": "周五下午在阅览室整理书目",
                "value_json": "{\"room\":\"3F\"}",
                "scope": {
                    "scope": "speaker_self",
                    "mentions": null,
                    "group_concept": null
                },
                "ambiguous_scope": null,
                "condition_concept": {
                    "ref_type": "propose_new",
                    "concept_id": null,
                    "kind": "condition",
                    "label": "整理书目",
                    "domain": "club",
                    "polarity": "neutral"
                },
                "action_concept": null,
                "group_concept": null,
                "member_mention": null,
                "valid_dates": {
                    "start_date_iso": "2026-11-20",
                    "end_date_iso": null,
                    "open_ended": false
                },
                "speaker": "alice",
                "assumed_premise_ids": [],
                "quote": {
                    "chunk_id": chunk_id,
                    "verbatim_quote": quote,
                    "turn_ordinal": null,
                    "approx_char_offset": null
                },
                "write_op": {
                    "op": "add",
                    "target_fact_id": null
                }
            }
        ]
    })
}

#[test]
fn three_tier_auto_detection_covers_host_cli_api_keys_secure_config_gateways_and_raw_only_fallback(
) -> TestResult {
    let dir = tempfile::tempdir()?;
    let fake_home = dir.path().join("home");
    let fake_bin = dir.path().join("bin");
    std::fs::create_dir_all(&fake_home)?;
    std::fs::create_dir_all(&fake_bin)?;

    // 1) Tier ③: Empty environment & empty search_dirs -> seamless `RawOnly` fallback, zero errors!
    let empty_env = ModelDetectEnv::from_env_map([("HOME", fake_home.to_string_lossy().as_ref())])
        .with_search_dirs(vec![fake_bin.clone()]);
    let detected_empty = detect_global_generative_model(&empty_env);
    assert_eq!(detected_empty, DetectedGenerativeModel::RawOnly);
    assert_eq!(detected_empty.display_label(), "Raw-Only (local FTS5)");
    assert!(detected_empty.build_llm_client()?.is_raw_only());

    // 2) Unauthenticated `codex` and `claude` binaries on PATH do NOT falsely match Tier ①:
    let codex_bin = fake_bin.join("codex");
    let claude_bin = fake_bin.join("claude");
    std::fs::write(&codex_bin, b"#!/bin/sh\nexit 0\n")?;
    std::fs::write(&claude_bin, b"#!/bin/sh\nexit 0\n")?;
    assert_eq!(
        detect_global_generative_model(&empty_env),
        DetectedGenerativeModel::RawOnly
    );

    // 3) Tier ② via domestic & cloud env keys when host CLIs are not logged in:
    for (env_key, expected_provider, expected_label, expected_endpoint_prefix) in [
        (
            "DASHSCOPE_API_KEY",
            GenerativeApiProvider::DashScope,
            "DashScope (Qwen)",
            "https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions",
        ),
        (
            "ARK_API_KEY",
            GenerativeApiProvider::Volcengine,
            "Volcengine Ark (Doubao)",
            "https://ark.cn-beijing.volces.com/api/v3/chat/completions",
        ),
        (
            "ZHIPUAI_API_KEY",
            GenerativeApiProvider::Zhipu,
            "Zhipu AI (GLM)",
            "https://open.bigmodel.cn/api/paas/v4/chat/completions",
        ),
        (
            "OPENROUTER_API_KEY",
            GenerativeApiProvider::OpenRouter,
            "OpenRouter",
            "https://openrouter.ai/api/v1/chat/completions",
        ),
    ] {
        let env = ModelDetectEnv::from_env_map([
            ("HOME", fake_home.to_string_lossy().as_ref()),
            (env_key, "test-provider-key-01"),
        ])
        .with_search_dirs(vec![fake_bin.clone()]);
        let det = detect_global_generative_model(&env);
        assert_eq!(det.display_label(), expected_label);
        match det {
            DetectedGenerativeModel::HttpApi(cfg) => {
                assert_eq!(cfg.provider, expected_provider);
                assert_eq!(cfg.resolved_endpoint_url()?, expected_endpoint_prefix);
                assert!(!format!("{cfg:?}").contains("test-provider-key-01"));
            }
            other => return Err(format!("expected HttpApi, got {other:?}").into()),
        }
    }

    // 4) Tier ①: Logged-in `claude` (`~/.claude.json` with `oauthAccount`) wins when no Codex auth exists:
    std::fs::write(
        fake_home.join(".claude.json"),
        r#"{"oauthAccount":{"accountUuid":"u1"}}"#,
    )?;
    let det_claude = detect_global_generative_model(&empty_env);
    assert_eq!(det_claude.display_label(), "Claude Code");
    assert!(matches!(
        det_claude,
        DetectedGenerativeModel::HostCli(DetectedHostCli {
            kind: HostCliKind::ClaudeCode,
            ..
        })
    ));

    // 5) Tier ①: Logged-in `codex` (`~/.codex/auth.json`) wins cleanly:
    let codex_dir = fake_home.join(".codex");
    std::fs::create_dir_all(&codex_dir)?;
    std::fs::write(codex_dir.join("auth.json"), r#"{"access_token":"tok"}"#)?;
    let det_codex = detect_global_generative_model(&empty_env);
    assert_eq!(det_codex.display_label(), "Codex / OpenAI");
    assert!(matches!(
        det_codex,
        DetectedGenerativeModel::HostCli(DetectedHostCli {
            kind: HostCliKind::Codex,
            ..
        })
    ));

    // 6) Enterprise gateway takeover (`OPENAI_BASE_URL` + `OPENAI_API_KEY` and `ANTHROPIC_BASE_URL` + `ANTHROPIC_API_KEY`):
    let corp_oai_env = ModelDetectEnv::from_env_map([
        ("HOME", fake_home.to_string_lossy().as_ref()),
        ("OPENAI_BASE_URL", "https://llm-gateway.internal.example/v1"),
        ("OPENAI_API_KEY", "sk-corp-gw-token"),
    ])
    .with_search_dirs(vec![fake_bin.clone()]);
    let det_corp_oai = detect_global_generative_model(&corp_oai_env);
    assert_eq!(det_corp_oai.display_label(), "OpenAI Compatible Gateway");
    match &det_corp_oai {
        DetectedGenerativeModel::HttpApi(cfg) => {
            assert!(cfg.is_custom_gateway);
            assert_eq!(
                cfg.resolved_endpoint_url()?,
                "https://llm-gateway.internal.example/v1/chat/completions"
            );
        }
        other => return Err(format!("expected OpenAI gateway HttpApi, got {other:?}").into()),
    }

    let corp_ant_env = ModelDetectEnv::from_env_map([
        ("HOME", fake_home.to_string_lossy().as_ref()),
        ("ANTHROPIC_BASE_URL", "https://claude-gw.internal.example"),
        ("ANTHROPIC_API_KEY", "sk-ant-corp-token"),
    ])
    .with_search_dirs(vec![fake_bin]);
    let det_corp_ant = detect_global_generative_model(&corp_ant_env);
    assert_eq!(det_corp_ant.display_label(), "Anthropic Compatible Gateway");
    match &det_corp_ant {
        DetectedGenerativeModel::HttpApi(cfg) => {
            assert_eq!(
                cfg.resolved_endpoint_url()?,
                "https://claude-gw.internal.example/v1/messages"
            );
        }
        other => return Err(format!("expected Anthropic gateway HttpApi, got {other:?}").into()),
    }

    // 7) `0600` `~/.kp/config.json` (`$KP_CONFIG_PATH`) reading & Unix `0644` permission rejection:
    let no_cli_home = dir.path().join("clean_home");
    let empty_bin = dir.path().join("empty_bin");
    std::fs::create_dir_all(&no_cli_home)?;
    std::fs::create_dir_all(&empty_bin)?;
    let cfg_file = dir.path().join("kp_config.json");
    std::fs::write(
        &cfg_file,
        serde_json::to_vec(&json!({
            "generative_model": "dashscope:qwen-max",
            "api_keys": {
                "dashscope": "ds-cfg-secret"
            },
            "provider_base_urls": {
                "dashscope": "https://dashscope-gw.internal.example/compatible-mode/v1"
            }
        }))?,
    )?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // World-readable `0644` must be ignored -> falls back safely to `RawOnly`!
        std::fs::set_permissions(&cfg_file, std::fs::Permissions::from_mode(0o644))?;
        let insecure_env = ModelDetectEnv::from_env_map([
            ("HOME", no_cli_home.to_string_lossy().as_ref()),
            ("KP_CONFIG_PATH", cfg_file.to_string_lossy().as_ref()),
        ])
        .with_search_dirs(vec![empty_bin.clone()]);
        assert_eq!(
            detect_global_generative_model(&insecure_env),
            DetectedGenerativeModel::RawOnly
        );

        // Strict `0600` owner-only permissions -> loaded cleanly!
        std::fs::set_permissions(&cfg_file, std::fs::Permissions::from_mode(0o600))?;
    }

    let secure_env = ModelDetectEnv::from_env_map([
        ("HOME", no_cli_home.to_string_lossy().as_ref()),
        ("KP_CONFIG_PATH", cfg_file.to_string_lossy().as_ref()),
    ])
    .with_search_dirs(vec![empty_bin]);
    let det_from_cfg = detect_global_generative_model(&secure_env);
    assert_eq!(det_from_cfg.display_label(), "DashScope (Qwen)");
    match det_from_cfg {
        DetectedGenerativeModel::HttpApi(cfg) => {
            assert_eq!(cfg.model, "qwen-max");
            assert_eq!(
                cfg.resolved_endpoint_url()?,
                "https://dashscope-gw.internal.example/compatible-mode/v1/chat/completions"
            );
        }
        other => {
            return Err(
                format!("expected DashScope HttpApi from 0600 config, got {other:?}").into(),
            )
        }
    }
    Ok(())
}

#[tokio::test]
async fn http_llm_raw_only_ack_skip_429_and_gateway_fallback_obey_zero_leak_contracts() -> TestResult
{
    // 1) `RawOnly` guarantees ZERO HTTP requests on both `extract_batch` and `judge_entailment`:
    let raw_hits = Arc::new(AtomicUsize::new(0));
    let hits_clone = Arc::clone(&raw_hits);
    let mut raw_cfg = HttpApiConfig::for_provider(GenerativeApiProvider::OpenAi, String::new());
    raw_cfg.route_policy = ModelRoutePolicy::RawOnly;
    let raw_client = HttpApiLlmClient::with_transport(
        raw_cfg,
        RecordingMockHttpTransport::new(move |_| {
            hits_clone.fetch_add(1, Ordering::SeqCst);
            Err(KpError::invalid("raw_only", "must never send HTTP request"))
        }),
    )?;
    assert!(raw_client.is_raw_only());
    let req = sample_batch_request("周五下午在阅览室整理书目。")?;
    assert_eq!(
        raw_client.extract_batch(&req).await?,
        ExtractionBatchOutcome::SkippedEmpty
    );
    assert_eq!(
        raw_client
            .judge_entailment(&EntailmentQuery {
                left_label: "a".into(),
                left_domain: "d".into(),
                right_label: "b".into(),
                right_domain: "d".into(),
                left_quote: None,
                right_quote: None,
            })
            .await?,
        EntailmentVerdict::Unrelated
    );
    assert_eq!(raw_hits.load(Ordering::SeqCst), 0);

    // 2) OpenAI custom gateway: redacts secrets before wire, falls back from HTTP 400 `json_schema`
    //    to `json_object` cleanly, and skips ack-only chunks with zero network requests!
    let attempt = Arc::new(AtomicUsize::new(0));
    let attempt_c = Arc::clone(&attempt);
    let oai_transport = RecordingMockHttpTransport::new(move |http_req| {
        let body_str = http_req.body.to_string();
        if body_str.contains("ghp_topsecret_token_12345") {
            return Err(KpError::invalid("redactor", "unredacted secret leaked"));
        }
        let n = attempt_c.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            // Simulate enterprise vLLM/proxy rejecting `json_schema` on first attempt:
            return Ok(HttpJsonResponse {
                status: 400,
                body: r#"{"error":{"message":"response_format json_schema is not supported"}}"#
                    .into(),
            });
        }
        let env_json = valid_wire_envelope_json("c0", "周五下午在阅览室整理书目。");
        Ok(HttpJsonResponse {
            status: 200,
            body: json!({
                "choices": [{
                    "message": {
                        "content": serde_json::to_string(&env_json).unwrap_or_default()
                    }
                }]
            })
            .to_string(),
        })
    });
    let oai_cfg = HttpApiConfig::for_provider(GenerativeApiProvider::OpenAi, "sk-gw-key")
        .with_base_url("https://gw.internal.example/v1")
        .with_model("qwen2.5-72b-instruct");
    let oai_client = HttpApiLlmClient::with_transport(oai_cfg, oai_transport)?;

    // Ack-only turn sends 0 HTTP calls:
    let ack_req = sample_batch_request("好的，收到！")?;
    assert_eq!(
        oai_client.extract_batch(&ack_req).await?,
        ExtractionBatchOutcome::SkippedEmpty
    );
    assert_eq!(oai_client.transport.recorded()?.len(), 0);

    // Real turn with embedded secret triggers redaction + 400->json_object retry -> Completed:
    let secret_req =
        sample_batch_request("周五下午在阅览室整理书目。备用凭据 ghp_topsecret_token_12345")?;
    let outcome = oai_client.extract_batch(&secret_req).await?;
    match outcome {
        ExtractionBatchOutcome::Completed { atoms } => {
            assert_eq!(atoms.len(), 1);
            assert_eq!(atoms[0].kind, FactKind::Event);
        }
        other => return Err(format!("expected Completed, got {other:?}").into()),
    }
    let recorded = oai_client.transport.recorded()?;
    assert_eq!(recorded.len(), 2);
    assert_eq!(
        recorded[0].headers.get("authorization").map(String::as_str),
        Some("Bearer sk-gw-key")
    );
    assert_eq!(recorded[0].body["response_format"]["type"], "json_schema");
    assert_eq!(recorded[1].body["response_format"]["type"], "json_object");

    // 3) Anthropic `/v1/messages` wire format (`x-api-key`, `anthropic-version`, `tool_use` + 429):
    let ant_step = Arc::new(AtomicUsize::new(0));
    let ant_step_c = Arc::clone(&ant_step);
    let ant_transport = RecordingMockHttpTransport::new(move |http_req| {
        if http_req.headers.get("x-api-key").map(String::as_str) != Some("sk-ant-gw")
            || http_req
                .headers
                .get("anthropic-version")
                .map(String::as_str)
                != Some("2023-06-01")
        {
            return Err(KpError::invalid(
                "headers",
                "missing Anthropic auth headers",
            ));
        }
        let step = ant_step_c.fetch_add(1, Ordering::SeqCst);
        if step == 0 {
            return Ok(HttpJsonResponse {
                status: 429,
                body: r#"{"type":"error","error":{"type":"rate_limit_error"}}"#.into(),
            });
        }
        Ok(HttpJsonResponse {
            status: 200,
            body: json!({
                "content": [{
                    "type": "tool_use",
                    "id": "toolu_01",
                    "name": "kp_entailment_verdict",
                    "input": {
                        "verdict": "entails",
                        "question": null
                    }
                }]
            })
            .to_string(),
        })
    });
    let ant_cfg = HttpApiConfig::for_provider(GenerativeApiProvider::Anthropic, "sk-ant-gw")
        .with_base_url("https://claude-proxy.internal.example/v1");
    let ant_client = HttpApiLlmClient::with_transport(ant_cfg, ant_transport)?;
    assert_eq!(
        ant_client.config().resolved_endpoint_url()?,
        "https://claude-proxy.internal.example/v1/messages"
    );

    // First call hits HTTP 429 -> `SkippedLowQuota`:
    assert!(matches!(
        ant_client.extract_batch(&req).await?,
        ExtractionBatchOutcome::SkippedLowQuota { .. }
    ));

    // Second call parses Anthropic `tool_use` block into `EntailmentVerdict::Entails`:
    let verdict = ant_client
        .judge_entailment(&EntailmentQuery {
            left_label: "配电室停电".into(),
            left_domain: "facility".into(),
            right_label: "断电维护".into(),
            right_domain: "facility".into(),
            left_quote: Some("周五配电室停电".into()),
            right_quote: Some("断电维护时关闭仪器".into()),
        })
        .await?;
    assert_eq!(verdict, EntailmentVerdict::Entails);
    Ok(())
}

#[tokio::test]
async fn loopback_axum_gateway_end_to_end_covers_openai_and_anthropic_base_url_overrides(
) -> TestResult {
    use axum::{extract::State, http::HeaderMap, routing::post, Json, Router};

    let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
        Ok(l) => l,
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            // macOS Seatbelt sandbox blocks loopback bind; full loopback verification runs outside sandbox.
            return Ok(());
        }
        Err(err) => return Err(err.into()),
    };
    let addr = listener.local_addr()?;

    let seen_paths: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let app = Router::new()
        .route(
            "/gw-oai/v1/chat/completions",
            post(
                |State(paths): State<Arc<Mutex<Vec<String>>>>,
                 headers: HeaderMap,
                 Json(body): Json<Value>| async move {
                    if let Ok(mut guard) = paths.lock() {
                        guard.push(format!(
                            "oai:{}",
                            headers
                                .get("authorization")
                                .and_then(|v| v.to_str().ok())
                                .unwrap_or("")
                        ));
                    }
                    let is_entail = body["response_format"]["json_schema"]["name"]
                        .as_str()
                        .is_some_and(|n| n.contains("entailment"));
                    let content = if is_entail {
                        json!({"verdict": "entails", "question": null}).to_string()
                    } else {
                        valid_wire_envelope_json("c0", "周五下午在阅览室整理书目。").to_string()
                    };
                    Json(json!({
                        "choices": [{ "message": { "content": content } }]
                    }))
                },
            ),
        )
        .route(
            "/gw-ant/v1/messages",
            post(
                |State(paths): State<Arc<Mutex<Vec<String>>>>,
                 headers: HeaderMap,
                 Json(_body): Json<Value>| async move {
                    if let Ok(mut guard) = paths.lock() {
                        guard.push(format!(
                            "ant:{}",
                            headers
                                .get("x-api-key")
                                .and_then(|v| v.to_str().ok())
                                .unwrap_or("")
                        ));
                    }
                    Json(json!({
                        "content": [{
                            "type": "tool_use",
                            "id": "toolu_gw",
                            "name": "kp_entailment_verdict",
                            "input": { "verdict": "contradicts", "question": null }
                        }]
                    }))
                },
            ),
        )
        .with_state(Arc::clone(&seen_paths));

    let server_task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let oai_env = ModelDetectEnv::from_env_map([
        (
            "OPENAI_BASE_URL",
            format!("http://{addr}/gw-oai/v1").as_str(),
        ),
        ("OPENAI_API_KEY", "sk-loopback-oai"),
    ])
    .with_search_dirs(Vec::new());
    let det_oai = detect_global_generative_model(&oai_env);
    assert_eq!(det_oai.display_label(), "OpenAI Compatible Gateway");
    let oai_llm = det_oai.build_llm_client()?;

    let req = sample_batch_request("周五下午在阅览室整理书目。")?;
    let ext = oai_llm.extract_batch(&req).await?;
    assert!(matches!(ext, ExtractionBatchOutcome::Completed { .. }));

    let ant_env = ModelDetectEnv::from_env_map([
        (
            "ANTHROPIC_BASE_URL",
            format!("http://{addr}/gw-ant").as_str(),
        ),
        ("ANTHROPIC_API_KEY", "sk-loopback-ant"),
    ])
    .with_search_dirs(Vec::new());
    let det_ant = detect_global_generative_model(&ant_env);
    assert_eq!(det_ant.display_label(), "Anthropic Compatible Gateway");
    let ant_llm = det_ant.build_llm_client()?;
    let verdict = ant_llm
        .judge_entailment(&EntailmentQuery {
            left_label: "活动恢复".into(),
            left_domain: "club".into(),
            right_label: "活动取消".into(),
            right_domain: "club".into(),
            left_quote: None,
            right_quote: None,
        })
        .await?;
    assert_eq!(verdict, EntailmentVerdict::Contradicts);

    server_task.abort();
    let recorded = seen_paths
        .lock()
        .map_err(|_| KpError::invalid("lock", "poisoned"))?
        .clone();
    assert_eq!(
        recorded,
        vec![
            "oai:Bearer sk-loopback-oai".to_owned(),
            "ant:sk-loopback-ant".to_owned()
        ]
    );
    Ok(())
}

#[test]
fn invalid_explicit_gateway_base_url_fails_closed_without_public_cloud_fallthrough() -> TestResult {
    let dir = tempfile::tempdir()?;
    let fake_home = dir.path().join("home");
    let fake_bin = dir.path().join("bin");
    std::fs::create_dir_all(fake_home.join(".codex"))?;
    std::fs::create_dir_all(&fake_bin)?;
    std::fs::write(fake_bin.join("codex"), b"#!/bin/sh\nexit 0\n")?;
    std::fs::write(
        fake_home.join(".codex").join("auth.json"),
        r#"{"access_token":"tok"}"#,
    )?;

    // Even when a logged-in CLI AND multiple public cloud API keys are present, an invalid
    // explicit `OPENAI_BASE_URL` or `ANTHROPIC_BASE_URL` MUST NOT fall through to public cloud!
    for (bad_env_key, bad_url) in [
        ("OPENAI_BASE_URL", "not-a-valid-url"),
        ("OPENAI_BASE_URL", "ftp://gateway.internal.example/v1"),
        (
            "OPENAI_BASE_URL",
            "https://user:secret@gateway.internal.example/v1",
        ),
        (
            "ANTHROPIC_BASE_URL",
            "https://claude-gw.internal.example?leak=1",
        ),
        (
            "DASHSCOPE_BASE_URL",
            "https://dashscope-gw.internal.example#frag",
        ),
    ] {
        let env = ModelDetectEnv::from_env_map([
            ("HOME", fake_home.to_string_lossy().as_ref()),
            (bad_env_key, bad_url),
            ("OPENAI_API_KEY", "sk-public-cloud-oai"),
            ("ANTHROPIC_API_KEY", "sk-ant-public-cloud"),
            ("DASHSCOPE_API_KEY", "ds-public-cloud"),
        ])
        .with_search_dirs(vec![fake_bin.clone()]);

        let det = detect_global_generative_model(&env);
        assert!(
            det.is_invalid_gateway(),
            "expected invalid gateway for {bad_env_key}={bad_url}, got {det:?}"
        );
        assert!(
            det.invalid_gateway_reason()
                .is_some_and(|r| r.contains(bad_env_key)),
            "expected invalid_gateway_reason to mention {bad_env_key}, got {:?}",
            det.invalid_gateway_reason()
        );
        // Passive ingest / hooks stay unblocked on local `RawOnly`:
        assert_eq!(det.route_policy(), ModelRoutePolicy::RawOnly);
        // Explicit LLM client build / `kp extract` fails immediately with a clear `base_url` error:
        let err = match det.build_llm_client() {
            Ok(_) => {
                return Err(
                    format!("expected build_llm_client Err for {bad_env_key}={bad_url}").into(),
                )
            }
            Err(err) => err,
        };
        assert!(matches!(
            err,
            KpError::InvalidInput { field, .. } if field == "base_url"
        ));
    }

    // Invalid `provider_base_urls` inside `0600` config.json also fails closed:
    let cfg_file = dir.path().join("bad_gw_config.json");
    std::fs::write(
        &cfg_file,
        serde_json::to_vec(&json!({
            "api_keys": { "openai": "sk-public-cloud" },
            "provider_base_urls": { "openai": "http://bad:creds@gw.internal.example/v1" }
        }))?,
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&cfg_file, std::fs::Permissions::from_mode(0o600))?;
    }
    let cfg_env = ModelDetectEnv::from_env_map([
        ("HOME", fake_home.to_string_lossy().as_ref()),
        ("KP_CONFIG_PATH", cfg_file.to_string_lossy().as_ref()),
    ])
    .with_search_dirs(vec![fake_bin]);
    let det_cfg = detect_global_generative_model(&cfg_env);
    assert!(det_cfg.is_invalid_gateway());
    assert_eq!(det_cfg.route_policy(), ModelRoutePolicy::RawOnly);
    assert!(matches!(
        det_cfg.build_llm_client(),
        Err(KpError::InvalidInput { field, .. }) if field == "base_url"
    ));

    Ok(())
}

#[test]
fn structural_cli_auth_probing_rejects_false_positive_substrings_and_booleans() -> TestResult {
    let dir = tempfile::tempdir()?;
    let fake_home = dir.path().join("home");
    let codex_dir = fake_home.join(".codex");
    let claude_dir = fake_home.join(".claude");
    std::fs::create_dir_all(&codex_dir)?;
    std::fs::create_dir_all(&claude_dir)?;

    let base_env = ModelDetectEnv::from_env_map([("HOME", fake_home.to_string_lossy().as_ref())]);
    let empty_cfg = KpGlobalModelConfig::default();

    // --- Codex structural checks ---
    // 1. Empty or blank-token `auth.json` and comment-only `config.toml` must NOT count as logged in:
    std::fs::write(
        codex_dir.join("auth.json"),
        r#"{"OPENAI_API_KEY":"   ","tokens":{"access_token":""}}"#,
    )?;
    std::fs::write(
        codex_dir.join("config.toml"),
        "# model_provider = \"openai\"\n[notice]\nnote = \"model_provider\"\n",
    )?;
    assert!(!probe_codex_logged_in(&base_env, &empty_cfg));

    // 2. `model_provider` pointing to a remote provider whose `env_key` is unset must NOT count:
    std::fs::write(
        codex_dir.join("config.toml"),
        "model_provider = \"corp_gw\"\n[model_providers.corp_gw]\nbase_url = \"https://gw.internal.example/v1\"\nenv_key = \"CORP_GW_KEY\"\n",
    )?;
    assert!(!probe_codex_logged_in(&base_env, &empty_cfg));

    // 3. Providing `CORP_GW_KEY` in env makes `corp_gw` valid:
    let keyed_codex_env = ModelDetectEnv::from_env_map([
        ("HOME", fake_home.to_string_lossy().as_ref()),
        ("CORP_GW_KEY", "corp-token-123"),
    ]);
    assert!(probe_codex_logged_in(&keyed_codex_env, &empty_cfg));

    // 4. Keyless loopback `model_provider` (`127.0.0.1` / `localhost`) is valid without `env_key`:
    std::fs::write(
        codex_dir.join("config.toml"),
        "model_provider = \"local_srv\"\n[model_providers.local_srv]\nbase_url = \"http://127.0.0.1:11434/v1\"\n",
    )?;
    assert!(probe_codex_logged_in(&base_env, &empty_cfg));

    // --- Claude Code structural checks ---
    // 1. `hasCompletedOnboarding: false` (even with `oauthAccount`), `hasCompletedOnboarding: true`
    //    without credentials, or substring mentions must ALL return `false`:
    std::fs::write(
        claude_dir.join(".credentials.json"),
        r#"{"claudeAiOauth":{"accessToken":"  "}}"#,
    )?;
    std::fs::write(
        claude_dir.join("settings.json"),
        r#"{"note":"ANTHROPIC_AUTH_TOKEN and apiKeyHelper mentioned as text"}"#,
    )?;
    for false_positive_claude_json in [
        r#"{"hasCompletedOnboarding":false,"oauthAccount":{"accountUuid":"u1"}}"#,
        r#"{"hasCompletedOnboarding":true}"#,
        r#"{"oauthAccount":{}}"#,
        r#"{"memo":"hasCompletedOnboarding primaryApiKey oauthAccount"}"#,
    ] {
        std::fs::write(fake_home.join(".claude.json"), false_positive_claude_json)?;
        assert!(
            !probe_claude_logged_in(&base_env, &empty_cfg),
            "expected false for .claude.json = {false_positive_claude_json}"
        );
    }

    // 2. Valid `.credentials.json` OAuth token returns `true`:
    std::fs::write(
        claude_dir.join(".credentials.json"),
        r#"{"claudeAiOauth":{"accessToken":"oat-valid-token"}}"#,
    )?;
    assert!(probe_claude_logged_in(&base_env, &empty_cfg));

    Ok(())
}

#[tokio::test]
async fn fallback_llm_client_fails_over_on_cli_auth_or_spawn_but_never_on_429_quota() -> TestResult
{
    use crate::llm_runner::{RecordingMockTransport, SubprocessRawOutput};

    // 1) Auto-detection wires `fallback_http` when both logged-in Host CLI and backup API key exist:
    let dir = tempfile::tempdir()?;
    let fake_home = dir.path().join("home");
    let fake_bin = dir.path().join("bin");
    std::fs::create_dir_all(fake_home.join(".codex"))?;
    std::fs::create_dir_all(&fake_bin)?;
    std::fs::write(fake_bin.join("codex"), b"#!/bin/sh\nexit 0\n")?;
    std::fs::write(
        fake_home.join(".codex").join("auth.json"),
        r#"{"tokens":{"access_token":"cli-oauth-tok"}}"#,
    )?;
    let dual_env = ModelDetectEnv::from_env_map([
        ("HOME", fake_home.to_string_lossy().as_ref()),
        ("DASHSCOPE_API_KEY", "ds-backup-paid-key"),
    ])
    .with_search_dirs(vec![fake_bin]);
    match detect_global_generative_model(&dual_env) {
        DetectedGenerativeModel::HostCli(cli) => {
            let backup = cli
                .fallback_http
                .ok_or_else(|| KpError::invalid("test", "expected fallback_http to be wired"))?;
            assert_eq!(backup.provider, GenerativeApiProvider::DashScope);
        }
        other => return Err(format!("expected HostCli with fallback_http, got {other:?}").into()),
    }

    // 2) Runtime failover verification:
    //    - Call 0 (`extract_batch`): CLI exits 1 with `401 Unauthorized` -> fails over to HTTP secondary!
    //    - Call 1 (`judge_entailment`): CLI fails to spawn (`host_cli_spawn`) -> fails over to HTTP secondary!
    //    - Call 2 (`extract_batch`): CLI exits 1 with `429 Too Many Requests` -> returns `SkippedLowQuota`, ZERO secondary HTTP calls!
    //    - Call 3 (`judge_entailment`): CLI exits 1 with `rate_limit_exceeded` -> returns `Err(host_cli_quota)`, ZERO secondary HTTP calls!
    let cli_step = Arc::new(AtomicUsize::new(0));
    let cli_step_c = Arc::clone(&cli_step);
    let cli_mock = RecordingMockTransport::new(move |_spec| {
        let step = cli_step_c.fetch_add(1, Ordering::SeqCst);
        match step {
            0 => Ok(SubprocessRawOutput {
                exit_code: Some(1),
                stdout: String::new(),
                stderr: "Error: 401 Unauthorized - session token expired, please run codex login"
                    .into(),
                output_file_content: None,
            }),
            1 => Err(KpError::invalid(
                "host_cli_spawn",
                "failed to spawn host CLI subprocess",
            )),
            2 => Ok(SubprocessRawOutput {
                exit_code: Some(1),
                stdout: String::new(),
                stderr: "HTTP 429 Too Many Requests: usage limit reached".into(),
                output_file_content: None,
            }),
            _ => Ok(SubprocessRawOutput {
                exit_code: Some(1),
                stdout: String::new(),
                stderr: "rate_limit_exceeded: quota exceeded on host plan".into(),
                output_file_content: None,
            }),
        }
    });

    let secondary_hits = Arc::new(AtomicUsize::new(0));
    let secondary_hits_c = Arc::clone(&secondary_hits);
    let http_mock = RecordingMockHttpTransport::new(move |http_req| {
        secondary_hits_c.fetch_add(1, Ordering::SeqCst);
        let is_entail = http_req.body["response_format"]["json_schema"]["name"]
            .as_str()
            .is_some_and(|n| n.contains("entailment"));
        let content = if is_entail {
            json!({"verdict": "entails", "question": null}).to_string()
        } else {
            valid_wire_envelope_json("c0", "周五下午在阅览室整理书目。").to_string()
        };
        Ok(HttpJsonResponse {
            status: 200,
            body: json!({ "choices": [{ "message": { "content": content } }] }).to_string(),
        })
    });

    let primary = HostCliRunner::with_transport(HostCliConfig::codex_default(), cli_mock)?;
    let secondary = HttpApiLlmClient::with_transport(
        HttpApiConfig::for_provider(GenerativeApiProvider::OpenAi, "sk-backup-paid"),
        http_mock,
    )?;
    let fallback = FallbackLlmClient::new(Arc::new(primary), Arc::new(secondary));

    let req = sample_batch_request("周五下午在阅览室整理书目。")?;
    let query = EntailmentQuery {
        left_label: "馆内修缮闭馆".into(),
        left_domain: "club".into(),
        right_label: "暂停借阅".into(),
        right_domain: "club".into(),
        left_quote: None,
        right_quote: None,
    };

    // Step 0: CLI 401 auth error -> secondary HTTP succeeds (secondary_hits == 1):
    assert!(matches!(
        fallback.extract_batch(&req).await?,
        ExtractionBatchOutcome::Completed { .. }
    ));
    assert_eq!(secondary_hits.load(Ordering::SeqCst), 1);

    // Step 1: CLI spawn error -> secondary HTTP entailment succeeds (secondary_hits == 2):
    assert_eq!(
        fallback.judge_entailment(&query).await?,
        EntailmentVerdict::Entails
    );
    assert_eq!(secondary_hits.load(Ordering::SeqCst), 2);

    // Step 2: CLI 429 quota exhausted on `extract_batch` -> returns `SkippedLowQuota`, secondary_hits stays 2!
    assert!(matches!(
        fallback.extract_batch(&req).await?,
        ExtractionBatchOutcome::SkippedLowQuota { .. }
    ));
    assert_eq!(secondary_hits.load(Ordering::SeqCst), 2);

    // Step 3: CLI rate-limit on `judge_entailment` -> returns quota `Err`, secondary_hits stays 2!
    let quota_err = match fallback.judge_entailment(&query).await {
        Ok(v) => return Err(format!("expected quota Err on entailment, got {v:?}").into()),
        Err(e) => e,
    };
    assert!(is_quota_exhausted_error(&quota_err));
    assert_eq!(secondary_hits.load(Ordering::SeqCst), 2);

    Ok(())
}

#[test]
fn named_endpoint_resolver_builds_configured_endpoints_and_rejects_unknown_without_cloud_fallback(
) -> TestResult {
    let dir = tempfile::tempdir()?;
    let fake_home = dir.path().join("home");
    std::fs::create_dir_all(fake_home.join(".codex"))?;
    std::fs::write(
        fake_home.join(".codex").join("auth.json"),
        r#"{"access_token":"tok"}"#,
    )?;

    let cfg_file = dir.path().join("named_ep_config.json");
    std::fs::write(
        &cfg_file,
        serde_json::to_vec(&json!({
            "named_endpoints": {
                "private_vllm": {
                    "provider": "openai",
                    "model": "qwen2.5-32b-instruct",
                    "base_url": "https://vllm.internal.example/v1",
                    "api_key_env": "PRIVATE_VLLM_TOKEN"
                },
                "inline_secret_ep": {
                    "provider": "anthropic",
                    "model": "claude-sonnet-4-6",
                    "base_url": "https://claude-gw.internal.example",
                    "api_key": "sk-ant-inline-secret"
                },
                "bad_url_ep": {
                    "provider": "openai",
                    "base_url": "ftp://invalid.internal.example",
                    "api_key": "tok"
                }
            }
        }))?,
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&cfg_file, std::fs::Permissions::from_mode(0o600))?;
    }

    let env = ModelDetectEnv::from_env_map([
        ("HOME", fake_home.to_string_lossy().as_ref()),
        ("KP_CONFIG_PATH", cfg_file.to_string_lossy().as_ref()),
        ("PRIVATE_VLLM_TOKEN", "vllm-gw-secret-99"),
        // Populate global OpenAI key to prove unknown endpoints NEVER fall back to it!
        ("OPENAI_API_KEY", "sk-global-cloud-key"),
    ])
    .with_search_dirs(Vec::new());

    // 1. Configured `private_vllm` resolves and builds cleanly:
    let vllm_cfg = resolve_named_endpoint_config("private_vllm", &env)?;
    assert_eq!(
        vllm_cfg.route_policy,
        ModelRoutePolicy::NamedEndpoint("private_vllm".into())
    );
    assert_eq!(vllm_cfg.model, "qwen2.5-32b-instruct");
    assert_eq!(
        vllm_cfg.resolved_endpoint_url()?,
        "https://vllm.internal.example/v1/chat/completions"
    );
    let vllm_client = build_named_endpoint_llm_client("private_vllm", &env)?;
    assert!(!vllm_client.is_raw_only());
    assert_eq!(
        vllm_client.route_policy(),
        ModelRoutePolicy::NamedEndpoint("private_vllm".into())
    );

    // 2. Inline `api_key` in `0600` config works and `Debug` redacts it:
    let inline_cfg = resolve_named_endpoint_config("inline_secret_ep", &env)?;
    assert_eq!(inline_cfg.provider, GenerativeApiProvider::Anthropic);
    assert!(!format!("{inline_cfg:?}").contains("sk-ant-inline-secret"));
    let _ = build_named_endpoint_llm_client("inline_secret_ep", &env)?;

    // 3. Explicit `<provider>:<model>` works ONLY when that provider's key is present:
    let oai_explicit = resolve_named_endpoint_config("openai:gpt-4o", &env)?;
    assert_eq!(oai_explicit.model, "gpt-4o");
    assert!(build_named_endpoint_llm_client("dashscope:qwen-max", &env).is_err());

    // 4. Unknown or unconfigured endpoint MUST error immediately — NEVER fall back to `OPENAI_API_KEY`!
    assert!(matches!(
        build_named_endpoint_llm_client("unconfigured_vault_gw", &env),
        Err(KpError::InvalidInput { field, .. }) if field == "model_route_policy"
    ));
    assert!(matches!(
        build_named_endpoint_llm_client("bad_url_ep", &env),
        Err(KpError::InvalidInput { field, .. }) if field == "base_url"
    ));

    // 5. Setting `KP_MODEL_POLICY="unconfigured_vault_gw"` in global detection also fails closed:
    let mut bad_pol_env = env;
    bad_pol_env.env_vars.insert(
        "KP_MODEL_POLICY".to_owned(),
        "unconfigured_vault_gw".to_owned(),
    );
    let det_bad_pol = detect_global_generative_model(&bad_pol_env);
    assert_eq!(det_bad_pol.route_policy(), ModelRoutePolicy::RawOnly);
    assert!(det_bad_pol.build_llm_client().is_err());

    Ok(())
}

#[tokio::test]
async fn build_llm_client_with_overrides_preserves_backup_failover_and_invalid_gateway_fail_closed(
) -> TestResult {
    // 1. Invalid gateway MUST still fail closed even when `--model` / `--remaining-quota-pct` overrides are passed:
    let bad_gw = DetectedGenerativeModel::invalid_gateway(
        GenerativeApiProvider::OpenAi,
        "OPENAI_BASE_URL malformed",
    );
    assert!(matches!(
        bad_gw.build_llm_client_with_overrides(Some("gpt-4o"), Some(50)),
        Err(KpError::InvalidInput { field, .. }) if field == "base_url"
    ));

    // 2. `HostCli` with `fallback_http`: passing `--model` and `--remaining-quota-pct` applies
    //    both overrides on primary AND keeps `FallbackLlmClient` backup failover intact!
    let mut req = sample_batch_request("周五下午在阅览室整理书目。")?;
    req.min_rate_limit_remaining_pct = 20;

    let backup_cfg = HttpApiConfig::for_provider(GenerativeApiProvider::OpenAi, "sk-backup")
        .with_base_url("http://127.0.0.1:9/v1");
    let host_with_backup = DetectedGenerativeModel::HostCli(DetectedHostCli {
        kind: HostCliKind::Codex,
        program: PathBuf::from("./nonexistent-codex-binary-for-override-test"),
        model: None,
        codex_config_path: None,
        gateway_base_url: None,
        extra_env: BTreeMap::new(),
        fallback_http: Some(backup_cfg),
    });

    // When `remaining_quota_pct_override = Some(10) < 20`, primary returns `SkippedLowQuota` immediately
    // and `FallbackLlmClient` preserves `SkippedLowQuota` without touching secondary `127.0.0.1:9`!
    let overridden_low_quota =
        host_with_backup.build_llm_client_with_overrides(Some("o3-mini"), Some(10))?;
    assert_eq!(
        overridden_low_quota.route_policy(),
        ModelRoutePolicy::HostCli
    );
    assert!(matches!(
        overridden_low_quota.extract_batch(&req).await?,
        ExtractionBatchOutcome::SkippedLowQuota {
            remaining_pct: Some(10),
            ..
        }
    ));

    // When `remaining_quota_pct_override = None` and primary fails to spawn (`nonexistent-codex-binary`),
    // `FallbackLlmClient` fails over to secondary `HttpApiLlmClient` (surfacing `http_llm` transport
    // error from `127.0.0.1:9` instead of `host_cli_spawn` — proving `FallbackLlmClient` was preserved!).
    let overridden_failover =
        host_with_backup.build_llm_client_with_overrides(Some("o3-mini"), None)?;
    let err = match overridden_failover.extract_batch(&req).await {
        Ok(out) => return Err(format!("expected secondary http_llm Err, got {out:?}").into()),
        Err(e) => e,
    };
    assert!(
        matches!(err, KpError::InvalidInput { field, .. } if field == "http_llm"),
        "expected failover to secondary HttpApiLlmClient (`http_llm`), got {err:?}"
    );

    Ok(())
}
