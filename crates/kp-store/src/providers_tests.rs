use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
    Json, Router,
};
use serde_json::{json, Value};

use super::*;

#[test]
fn registry_covers_all_domestic_international_and_explicit_local_recipes() -> Result<()> {
    let expected_ids = [
        "local",
        "dashscope",
        "volcengine",
        "zhipu",
        "openai",
        "voyage",
        "google",
        "openrouter",
        "ollama",
        "lmstudio",
        "llama-server",
    ];
    for id in expected_ids {
        assert!(
            find_embedding_recipe(id).is_some(),
            "missing embedding recipe `{id}`"
        );
    }

    let dashscope = find_embedding_recipe("dashscope").ok_or(StoreError::NotFound)?;
    assert_eq!(
        dashscope.base_url_default,
        Some("https://dashscope.aliyuncs.com/compatible-mode/v1")
    );
    assert_eq!(dashscope.auth_env, &["DASHSCOPE_API_KEY"]);
    assert_eq!(dashscope.touchpoint.default_model, "text-embedding-v3");
    assert_eq!(dashscope.touchpoint.default_dims, 1024);
    assert_eq!(
        dashscope.touchpoint.dims_options,
        &[64, 128, 256, 512, 768, 1024, 1536, 2048]
    );
    assert_eq!(dashscope.touchpoint.max_batch_items, Some(10));
    assert_eq!(dashscope.touchpoint.max_batch_tokens, Some(8192));
    assert_eq!(dashscope.touchpoint.chars_per_token, 2);

    let volc = find_embedding_recipe("volcengine").ok_or(StoreError::NotFound)?;
    assert_eq!(
        volc.base_url_default,
        Some("https://ark.cn-beijing.volces.com/api/v3")
    );
    assert_eq!(volc.auth_env, &["ARK_API_KEY"]);
    assert!(!volc.send_dimensions_param);
    assert!(volc.user_provided_models);
    assert_eq!(
        volc.touchpoint
            .lookup_model_dims("doubao-embedding-text-240715"),
        Some(2560)
    );
    assert_eq!(
        volc.touchpoint
            .lookup_model_dims("doubao-embedding-text-240515"),
        Some(2048)
    );
    assert_eq!(
        volc.touchpoint
            .lookup_model_dims("doubao-embedding-large-text-240915"),
        Some(4096)
    );

    // Arbitrary Volcengine user inference endpoint `ep-*` parses cleanly with dynamic `0` dims
    // (auto-locked on first vector) or explicit `--embedding-dimensions`.
    let ep_auto = parse_embedding_model_spec("volcengine:ep-20261123-custom", None)?;
    assert_eq!(ep_auto.model_id, "volcengine:ep-20261123-custom");
    assert_eq!(ep_auto.dims, 0);
    let ep_explicit = parse_embedding_model_spec("volcengine:ep-20261123-custom", Some(2048))?;
    assert_eq!(ep_explicit.dims, 2048);

    // Explicit local services (`ollama`, `lmstudio`, `llama-server`) never participate in env auto-detect.
    for local_srv in ["ollama", "lmstudio", "llama-server"] {
        let r = find_embedding_recipe(local_srv).ok_or(StoreError::NotFound)?;
        assert!(!r.auto_detect_env, "`{local_srv}` must not auto-detect env");
        assert!(!r.is_cloud);
    }
    assert!(parse_embedding_model_spec("lmstudio:my-gguf", None).is_err());
    assert!(parse_embedding_model_spec("llama-server:my-gguf", None).is_err());
    let lms = parse_embedding_model_spec("lmstudio:my-gguf", Some(768))?;
    assert_eq!(lms.dims, 768);
    assert_eq!(
        find_embedding_recipe("llama-server")
            .ok_or(StoreError::NotFound)?
            .touchpoint
            .max_batch_items,
        Some(32)
    );
    Ok(())
}

#[test]
fn init_embedding_choice_respects_priority_ignores_local_daemons_and_reads_0600_config(
) -> Result<()> {
    let dir = tempfile::tempdir()?;
    let cfg_path = dir.path().join("config.json");

    // 1. No env keys, no config -> defaults to `local:bge-small-zh-v1.5` (`512d`), even if OLLAMA_API_KEY is set!
    let env_with_ollama = BTreeMap::from([
        ("OLLAMA_API_KEY".to_owned(), "local-token".to_owned()),
        ("LLAMA_API_KEY".to_owned(), "local-token".to_owned()),
    ]);
    let default_choice = resolve_init_embedding_choice(
        false,
        None,
        None,
        |k| env_with_ollama.get(k).cloned(),
        None,
    )?;
    assert_eq!(default_choice.selected.model_id, "local:bge-small-zh-v1.5");
    assert_eq!(default_choice.selected.dims, 512);
    assert!(default_choice.detected_cloud_providers.is_empty());
    assert!(default_choice.cloud_privacy_notice.is_none());

    // 2. `--no-embedding` wins unconditionally.
    let env_cloud = BTreeMap::from([("DASHSCOPE_API_KEY".to_owned(), "sk-dash".to_owned())]);
    let none_choice =
        resolve_init_embedding_choice(true, None, None, |k| env_cloud.get(k).cloned(), None)?;
    assert_eq!(none_choice.selected.model_id, "none");
    assert_eq!(none_choice.selected.dims, 0);

    // 3. Multiple cloud keys detected across env and `0600` config -> lists all candidates + privacy notice.
    let mut cfg = KpGlobalConfig::default();
    cfg.api_keys
        .insert("ARK_API_KEY".into(), "ark-secret-key".into());
    cfg.provider_base_urls.insert(
        "volcengine".into(),
        "https://ark-internal.example/api/v3/".into(),
    );
    save_global_config_0600(&cfg_path, &cfg)?;
    let loaded_cfg = load_global_config_from_path(&cfg_path)?;
    assert_eq!(loaded_cfg, cfg);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let bad_path = dir.path().join("insecure.json");
        std::fs::write(&bad_path, "{}")?;
        std::fs::set_permissions(&bad_path, std::fs::Permissions::from_mode(0o644))?;
        assert!(load_global_config_from_path(&bad_path).is_err());
    }

    let multi = resolve_init_embedding_choice(
        false,
        None,
        None,
        |k| env_cloud.get(k).cloned(),
        Some(&loaded_cfg),
    )?;
    assert_eq!(multi.detected_cloud_providers.len(), 2);
    assert_eq!(multi.detected_cloud_providers[0].provider_id, "dashscope");
    assert_eq!(multi.detected_cloud_providers[1].provider_id, "volcengine");
    assert!(multi.cloud_privacy_notice.is_some());

    // 4. Explicit `--embedding-model ollama:bge-m3` overrides detected cloud keys.
    let explicit_ollama = resolve_init_embedding_choice(
        false,
        Some("ollama:bge-m3"),
        None,
        |k| env_cloud.get(k).cloned(),
        Some(&loaded_cfg),
    )?;
    assert_eq!(explicit_ollama.selected.model_id, "ollama:bge-m3");
    assert_eq!(explicit_ollama.selected.dims, 1024);
    assert!(explicit_ollama.cloud_privacy_notice.is_none());

    // 5. OpenAI `OPENAI_BASE_URL` override and `provider_base_urls` override.
    let openai_spec = parse_embedding_model_spec("openai:text-embedding-3-small", Some(512))?;
    let openai_emb = ProviderEmbedder::from_lookup(
        ProviderEmbedderConfig::new(openai_spec),
        |k| match k {
            "OPENAI_API_KEY" => Some("sk-test".into()),
            "OPENAI_BASE_URL" => Some("https://gateway.internal.example/v1/".into()),
            _ => None,
        },
        None,
    )?;
    assert_eq!(
        openai_emb.endpoint_url(),
        "https://gateway.internal.example/v1/embeddings"
    );
    assert!(!openai_emb.allows_raw_only());

    let volc_spec = parse_embedding_model_spec("volcengine:doubao-embedding-text-240715", None)?;
    let volc_emb = ProviderEmbedder::from_lookup(
        ProviderEmbedderConfig::new(volc_spec),
        |_| None,
        Some(&loaded_cfg),
    )?;
    assert_eq!(
        volc_emb.endpoint_url(),
        "https://ark-internal.example/api/v3/embeddings"
    );
    Ok(())
}

#[derive(Clone)]
struct MockEmbedServerState {
    recorded_bodies: Arc<Mutex<Vec<Value>>>,
    recorded_auth: Arc<Mutex<Vec<String>>>,
    dims: usize,
}

async fn mock_embeddings_handler(
    State(state): State<MockEmbedServerState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    if let Some(auth) = headers.get("authorization").and_then(|v| v.to_str().ok()) {
        if let Ok(mut guard) = state.recorded_auth.lock() {
            guard.push(auth.to_owned());
        }
    }
    if let Ok(mut guard) = state.recorded_bodies.lock() {
        guard.push(body.clone());
    }
    let count = body["input"].as_array().map_or(0, Vec::len);
    let data: Vec<Value> = (0..count)
        .rev()
        .map(|idx| {
            let mut vec = vec![0.0_f64; state.dims];
            vec[idx % state.dims] = 3.0;
            vec[(idx + 1) % state.dims] = 4.0;
            json!({
                "object": "embedding",
                "index": idx,
                "embedding": vec,
            })
        })
        .collect();
    (
        StatusCode::OK,
        Json(json!({
            "object": "list",
            "data": data,
        })),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provider_embedder_batches_dashscope_omits_volcengine_dims_redacts_secrets_and_tests(
) -> Result<()> {
    let mock_bodies = Arc::new(Mutex::new(Vec::<Value>::new()));
    let mock_auths = Arc::new(Mutex::new(Vec::<String>::new()));
    let mb = mock_bodies.clone();
    let ma = mock_auths.clone();
    let inproc_transport: MockEmbedTransport = Arc::new(move |_url, api_key, body| {
        if let Some(k) = api_key {
            if let Ok(mut guard) = ma.lock() {
                guard.push(format!("Bearer {k}"));
            }
        }
        if let Ok(mut guard) = mb.lock() {
            guard.push(body.clone());
        }
        let count = body["input"].as_array().map_or(0, Vec::len);
        let data: Vec<Value> = (0..count)
            .rev()
            .map(|idx| {
                let mut vec = vec![0.0_f64; 64];
                vec[idx % 64] = 3.0;
                vec[(idx + 1) % 64] = 4.0;
                json!({ "object": "embedding", "index": idx, "embedding": vec })
            })
            .collect();
        let bytes = serde_json::to_vec(&json!({ "object": "list", "data": data }))?;
        Ok((200, bytes))
    });

    // Deterministic in-process verification (always runs even inside loopback-restricted sandboxes).
    let dash_spec = parse_embedding_model_spec("dashscope:text-embedding-v3", Some(64))?;
    let dash_inproc = ProviderEmbedder::from_lookup(
        ProviderEmbedderConfig::new(dash_spec.clone()),
        |k| (k == "DASHSCOPE_API_KEY").then(|| "dash-token-123".into()),
        None,
    )?
    .with_mock_transport(inproc_transport.clone());

    let mut inputs: Vec<String> = (0..11).map(|i| format!("文档分块 {i}")).collect();
    inputs.push("sensitive api_key=topsecret999".into());
    let inproc_vecs = dash_inproc.embed_batch(&inputs)?;
    assert_eq!(inproc_vecs.len(), 12);
    assert!(inproc_vecs.iter().all(|v| v.len() == 64));
    // Verify out-of-order `index` sorting restored original order in both batch 1 (`0..10`) and batch 2 (`0..2`).
    assert!((inproc_vecs[0][0] - 0.6).abs() < 1e-5);
    assert!((inproc_vecs[9][9] - 0.6).abs() < 1e-5);
    assert!((inproc_vecs[11][1] - 0.6).abs() < 1e-5);
    assert_eq!(dash_inproc.test_connectivity().await?.returned_dims, 64);

    let volc_spec_inproc = parse_embedding_model_spec("volcengine:ep-20261123-demo", None)?;
    let volc_inproc = ProviderEmbedder::from_lookup(
        ProviderEmbedderConfig::new(volc_spec_inproc),
        |k| (k == "ARK_API_KEY").then(|| "ark-token-456".into()),
        None,
    )?
    .with_mock_transport(inproc_transport.clone());
    assert_eq!(volc_inproc.embed("火山方舟豆包向量连通测试")?.len(), 64);

    // Dimension mismatch guard suggests `kp embed --stale --to ...`.
    let mismatch_spec = parse_embedding_model_spec("dashscope:text-embedding-v3", Some(128))?;
    let mismatch_emb = ProviderEmbedder::from_lookup(
        ProviderEmbedderConfig::new(mismatch_spec),
        |k| (k == "DASHSCOPE_API_KEY").then(|| "dash-token-123".into()),
        None,
    )?
    .with_mock_transport(inproc_transport);
    let mismatch_err = mismatch_emb
        .embed("dim check")
        .err()
        .ok_or(StoreError::NotFound)?
        .to_string();
    assert!(mismatch_err.contains("kp embed --stale --to dashscope:text-embedding-v3 --dim 64"));

    {
        let bodies = mock_bodies
            .lock()
            .map_err(|_| StoreError::Embedding("lock".into()))?;
        assert_eq!(bodies.len(), 5);
        assert_eq!(bodies[0]["dimensions"], json!(64));
        assert!(!bodies[1].to_string().contains("topsecret999"));
        assert!(bodies[1].to_string().contains("[REDACTED]"));
        assert!(bodies[3].get("dimensions").is_none());
    }
    {
        let auths = mock_auths
            .lock()
            .map_err(|_| StoreError::Embedding("lock".into()))?;
        assert_eq!(auths[0], "Bearer dash-token-123");
        assert_eq!(auths[3], "Bearer ark-token-456");
    }

    // Live `axum` + `reqwest` loopback verification when TCP bind is permitted.
    let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
        Ok(l) => l,
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    let addr = listener.local_addr()?;
    let base_url = format!("http://{addr}/v1");

    let recorded_bodies = Arc::new(Mutex::new(Vec::new()));
    let recorded_auth = Arc::new(Mutex::new(Vec::new()));
    let state = MockEmbedServerState {
        recorded_bodies: recorded_bodies.clone(),
        recorded_auth: recorded_auth.clone(),
        dims: 64,
    };
    let app = Router::new()
        .route("/v1/embeddings", post(mock_embeddings_handler))
        .with_state(state);
    let server = tokio::spawn(async move { axum::serve(listener, app).await });

    // 1. DashScope sends `dimensions: 64`, splits 12 items into `10 + 2` batches, and redacts secrets.
    let mut cfg = ProviderEmbedderConfig::new(dash_spec);
    cfg.base_url_override = Some(base_url.clone());
    cfg.api_key_override = Some("dash-token-123".into());
    let dash_emb = ProviderEmbedder::from_lookup(cfg, |_| None, None)?;

    let mut inputs: Vec<String> = (0..11).map(|i| format!("文档分块 {i}")).collect();
    inputs.push("sensitive api_key=topsecret999".into());
    let vectors = dash_emb.embed_batch(&inputs)?;
    assert_eq!(vectors.len(), 12);
    assert!(vectors.iter().all(|v| v.len() == 64));

    let probe_report = dash_emb.test_connectivity().await?;
    assert_eq!(probe_report.returned_dims, 64);

    // 2. Volcengine Ark NEVER sends `"dimensions"` by default!
    let volc_spec = parse_embedding_model_spec("volcengine:ep-20261123-demo", None)?;
    let mut volc_cfg = ProviderEmbedderConfig::new(volc_spec);
    volc_cfg.base_url_override = Some(base_url);
    volc_cfg.api_key_override = Some("ark-token-456".into());
    let volc_emb = ProviderEmbedder::from_lookup(volc_cfg, |_| None, None)?;
    let volc_vec = volc_emb.embed("火山方舟豆包向量连通测试")?;
    assert_eq!(volc_vec.len(), 64);

    {
        let bodies = recorded_bodies
            .lock()
            .map_err(|_| StoreError::Embedding("lock".into()))?;
        // 2 batches for DashScope + 1 connectivity probe + 1 Volcengine call = 4 calls
        assert_eq!(bodies.len(), 4);
        assert_eq!(bodies[0]["dimensions"], json!(64));
        assert!(!bodies[1].to_string().contains("topsecret999"));
        assert!(bodies[1].to_string().contains("[REDACTED]"));
        assert!(
            bodies[3].get("dimensions").is_none(),
            "Volcengine must omit `dimensions` parameter by default: {}",
            bodies[3]
        );
    }
    {
        let auths = recorded_auth
            .lock()
            .map_err(|_| StoreError::Embedding("lock".into()))?;
        assert_eq!(auths[0], "Bearer dash-token-123");
        assert_eq!(auths[3], "Bearer ark-token-456");
    }

    server.abort();
    Ok(())
}

#[test]
fn m2_r1_enterprise_gateway_openai_base_url_overrides_all_cloud_recipes_and_blocks_invalid_urls(
) -> Result<()> {
    // 1. Universal `OPENAI_BASE_URL` overrides EVERY `is_cloud && openai-compatible` recipe,
    //    without hijacking explicit local `ollama`.
    for (spec_str, key_var) in [
        ("dashscope:text-embedding-v3", "DASHSCOPE_API_KEY"),
        ("volcengine:doubao-embedding-text-240715", "ARK_API_KEY"),
        ("zhipu:embedding-3", "ZHIPUAI_API_KEY"),
        ("openai:text-embedding-3-small", "OPENAI_API_KEY"),
        ("voyage:voyage-3", "VOYAGE_API_KEY"),
        ("google:gemini-embedding-001", "GEMINI_API_KEY"),
        (
            "openrouter:openai/text-embedding-3-small",
            "OPENROUTER_API_KEY",
        ),
    ] {
        let spec = parse_embedding_model_spec(spec_str, None)?;
        let emb = ProviderEmbedder::from_lookup(
            ProviderEmbedderConfig::new(spec),
            |k| match k {
                "OPENAI_BASE_URL" => Some("https://enterprise-gw.internal.example/v1/".into()),
                v if v == key_var => Some("sk-enterprise".into()),
                _ => None,
            },
            None,
        )?;
        assert_eq!(
            emb.endpoint_url(),
            "https://enterprise-gw.internal.example/v1/embeddings",
            "expected OPENAI_BASE_URL to override `{spec_str}`"
        );
    }

    let ollama_spec = parse_embedding_model_spec("ollama:bge-m3", None)?;
    let ollama_emb = ProviderEmbedder::from_lookup(
        ProviderEmbedderConfig::new(ollama_spec),
        |k| (k == "OPENAI_BASE_URL").then(|| "https://enterprise-gw.internal.example/v1".into()),
        None,
    )?;
    assert_eq!(
        ollama_emb.endpoint_url(),
        "http://127.0.0.1:11434/v1/embeddings"
    );

    // 2. Exact priority order:
    //    `cfg.base_url_override > provider_base_urls.<id> > <PROVIDER>_BASE_URL > OPENAI_BASE_URL > default`.
    let dash_spec = parse_embedding_model_spec("dashscope:text-embedding-v3", None)?;
    let emb_prov_env = ProviderEmbedder::from_lookup(
        ProviderEmbedderConfig::new(dash_spec.clone()),
        |k| match k {
            "DASHSCOPE_API_KEY" => Some("sk-dash".into()),
            "DASHSCOPE_BASE_URL" => Some("https://dash-env.internal.example/v1".into()),
            "OPENAI_BASE_URL" => Some("https://openai-env.internal.example/v1".into()),
            _ => None,
        },
        None,
    )?;
    assert_eq!(
        emb_prov_env.endpoint_url(),
        "https://dash-env.internal.example/v1/embeddings"
    );

    let mut global_cfg = KpGlobalConfig::default();
    global_cfg.provider_base_urls.insert(
        "dashscope".into(),
        "https://cfg-priority.internal.example/v1".into(),
    );
    let emb_cfg = ProviderEmbedder::from_lookup(
        ProviderEmbedderConfig::new(dash_spec.clone()),
        |k| match k {
            "DASHSCOPE_API_KEY" => Some("sk-dash".into()),
            "DASHSCOPE_BASE_URL" => Some("https://dash-env.internal.example/v1".into()),
            "OPENAI_BASE_URL" => Some("https://openai-env.internal.example/v1".into()),
            _ => None,
        },
        Some(&global_cfg),
    )?;
    assert_eq!(
        emb_cfg.endpoint_url(),
        "https://cfg-priority.internal.example/v1/embeddings"
    );

    // 3. Invalid explicit gateway URLs fail closed immediately and NEVER fall back to public defaults!
    for bad_url in [
        "not-a-valid-url",
        ":///",
        "///",
        "ftp://gateway.internal.example/v1",
        "https://user:secret@gateway.internal.example/v1",
        "https://gateway.internal.example/v1?leak=1",
    ] {
        let res = ProviderEmbedder::from_lookup(
            ProviderEmbedderConfig::new(dash_spec.clone()),
            |k| match k {
                "DASHSCOPE_API_KEY" => Some("sk-dash".into()),
                "OPENAI_BASE_URL" => Some(bad_url.into()),
                _ => None,
            },
            None,
        );
        assert!(
            matches!(res, Err(StoreError::Embedding(_))),
            "expected malformed OPENAI_BASE_URL `{bad_url}` to fail closed"
        );
    }

    let mut bad_cfg = KpGlobalConfig::default();
    bad_cfg
        .provider_base_urls
        .insert("dashscope".into(), "///".into());
    assert!(matches!(
        ProviderEmbedder::from_lookup(
            ProviderEmbedderConfig::new(dash_spec),
            |k| (k == "DASHSCOPE_API_KEY").then(|| "sk-dash".into()),
            Some(&bad_cfg),
        ),
        Err(StoreError::Embedding(_))
    ));

    // 4. R2 Finding 9: `diagnose_embedder_spec_issue_with_lookup` & `try_build_embedder_with_reason`
    //    surface clear diagnostics for missing API keys or invalid gateway URLs without leaking secrets!
    let volc_spec = parse_embedding_model_spec("volcengine:doubao-embedding-text-240715", None)?;
    let missing_key_diag = diagnose_embedder_spec_issue_with_lookup(&volc_spec, |_| None, None)
        .ok_or(StoreError::NotFound)?;
    assert!(missing_key_diag.contains("ARK_API_KEY"));

    let bad_gw_diag = diagnose_embedder_spec_issue_with_lookup(
        &volc_spec,
        |k| match k {
            "ARK_API_KEY" => Some("sk-1234567890abcdefghijklmnop".into()),
            "OPENAI_BASE_URL" => {
                Some("https://admin:sk-1234567890abcdefghijklmnop@gw.example/v1".into())
            }
            _ => None,
        },
        None,
    )
    .ok_or(StoreError::NotFound)?;
    assert!(bad_gw_diag.contains("invalid gateway base URL"));
    assert!(!bad_gw_diag.contains("sk-1234567890abcdefghijklmnop"));
    assert!(bad_gw_diag.contains("[REDACTED]"));

    let ok_diag = diagnose_embedder_spec_issue_with_lookup(
        &volc_spec,
        |k| (k == "ARK_API_KEY").then(|| "ark-valid".into()),
        None,
    );
    assert!(ok_diag.is_none());

    let none_spec = parse_embedding_model_spec("none", None)?;
    let (none_emb, none_issue) = try_build_embedder_with_reason(&none_spec, None);
    assert!(!none_emb.is_enabled());
    assert!(none_issue.is_none());
    Ok(())
}
