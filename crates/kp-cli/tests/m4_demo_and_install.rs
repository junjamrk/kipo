mod support;

use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

#[cfg(unix)]
use axum::{http::StatusCode, routing::get, Router};
#[cfg(unix)]
use kp_model::ContentHash;
use kp_model::{EpochMs, FactAccess, FactId, FactStatus, ReviewKind, ReviewStatus};
use kp_store::Store;
use serde_json::Value;

use crate::support::{run, success, Result};

#[cfg(unix)]
fn detect_installer_target_triple() -> Result<String> {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        other => return Err(format!("unsupported test arch: {other}").into()),
    };
    let os = match std::env::consts::OS {
        "macos" => "apple-darwin",
        "linux" => "unknown-linux-gnu",
        other => return Err(format!("unsupported test os: {other}").into()),
    };
    Ok(format!("{arch}-{os}"))
}

#[cfg(unix)]
fn repo_install_script_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/install.sh")
}

#[test]
fn kp_demo_populates_four_columns_writes_client_configs_and_enforces_carol_isolation() -> Result {
    let dir = tempfile::tempdir()?;

    // 1. Ephemeral `kp demo --json` (no `--dir`, no `--keep`) cleans up its temp directory on exit.
    let ephem_out = success(
        Command::new(env!("CARGO_BIN_EXE_kp"))
            .env("KP_OFFLINE", "1")
            .args(["--json", "demo"])
            .output()?,
    )?;
    assert_eq!(ephem_out["kept"], false);
    assert_eq!(ephem_out["prompt_context_verified"], true);
    assert_eq!(ephem_out["pre_tool_claude_ask_verified"], true);
    assert_eq!(ephem_out["pre_tool_codex_deny_and_allow_verified"], true);
    assert_eq!(ephem_out["markdown_roundtrip_verified"], true);
    assert_eq!(ephem_out["carol_isolation_verified"], true);
    let ephem_root = PathBuf::from(ephem_out["root_dir"].as_str().ok_or("ephem root_dir")?);
    assert!(
        !ephem_root.exists(),
        "ephemeral `kp demo` without --dir/--keep must clean up {}",
        ephem_root.display()
    );

    // 2. Persistent `kp demo --dir <target> --json`: inspect generated configs & all 4 library pillars!
    let target_dir = dir.path().join("kept_demo");
    let demo_out = success(
        Command::new(env!("CARGO_BIN_EXE_kp"))
            .env("KP_OFFLINE", "1")
            .arg("--json")
            .arg("demo")
            .arg("--dir")
            .arg(&target_dir)
            .output()?,
    )?;
    assert_eq!(demo_out["kept"], true);
    assert!(
        demo_out["elapsed_ms"].as_u64().unwrap_or(u64::MAX) < 5_000,
        "expected fast offline demo execution, got {}ms",
        demo_out["elapsed_ms"]
    );

    let claude_settings = PathBuf::from(
        demo_out["claude_settings"]
            .as_str()
            .ok_or("claude_settings")?,
    );
    let codex_hooks = PathBuf::from(demo_out["codex_hooks"].as_str().ok_or("codex_hooks")?);
    let codex_config = PathBuf::from(demo_out["codex_config"].as_str().ok_or("codex_config")?);
    assert!(claude_settings.is_file());
    assert!(codex_hooks.is_file());
    assert!(codex_config.is_file());
    let claude_json: Value = serde_json::from_slice(&fs::read(&claude_settings)?)?;
    assert!(claude_json["hooks"]["PreToolUse"].is_array());
    let codex_toml = fs::read_to_string(&codex_config)?;
    assert!(codex_toml.contains("[mcp_servers.kp]"));

    let lib_dir = PathBuf::from(demo_out["library_dir"].as_str().ok_or("library_dir")?);
    let ro = Store::open_readonly(&lib_dir)?;
    assert_eq!(
        ro.daemon_queue_stats()?.1,
        0,
        "demo library must have 0 pending extraction jobs"
    );
    let graph = ro.derivation_view()?;

    // Pillar ①: Old premise `Superseded` -> dependent `AssumedPremise` conclusion `StalePremise`.
    let sup_id: FactId = demo_out["superseded_premise_id"]
        .as_str()
        .ok_or("sup_id")?
        .parse()?;
    let stale_id: FactId = demo_out["stale_assumed_conclusion_id"]
        .as_str()
        .ok_or("stale_id")?
        .parse()?;
    let active_dec_id: FactId = demo_out["active_decision_id"]
        .as_str()
        .ok_or("active_dec_id")?
        .parse()?;
    assert_eq!(
        graph.facts.get(&sup_id).map(|f| f.status),
        Some(FactStatus::Superseded)
    );
    assert_eq!(
        graph.facts.get(&stale_id).map(|f| f.status),
        Some(FactStatus::StalePremise)
    );
    assert_eq!(
        graph.facts.get(&active_dec_id).map(|f| f.status),
        Some(FactStatus::Active)
    );

    // Pillar ②: Active hard ops guard + `alice` private memo.
    let guard_id: FactId = demo_out["guard_fact_id"]
        .as_str()
        .ok_or("guard_id")?
        .parse()?;
    let priv_id: FactId = demo_out["private_memo_id"]
        .as_str()
        .ok_or("priv_id")?
        .parse()?;
    assert_eq!(
        graph.facts.get(&guard_id).map(|f| f.status),
        Some(FactStatus::Active)
    );
    assert_eq!(
        graph.facts.get(&priv_id).map(|f| f.status),
        Some(FactStatus::Active)
    );

    // Pillar ③: Expired maintenance window fact (`valid_until < now`).
    let exp_id: FactId = demo_out["expired_event_id"]
        .as_str()
        .ok_or("exp_id")?
        .parse()?;
    let exp_fact = graph.facts.get(&exp_id).ok_or("missing expired_event_id")?;
    match &exp_fact.access {
        FactAccess::Base { stamp, .. } => {
            assert!(
                stamp.valid.valid_until < EpochMs::OPEN_END,
                "expected finite expired validity window on demo maintenance event"
            );
        }
        FactAccess::Derived { .. } => return Err("expected Base access on expired event".into()),
    }

    // Pillar ④: Open `AmbiguousScope` review item for `alice`.
    let alice_actor = "alice".parse()?;
    let carol_actor = "carol".parse()?;
    let open_revs = ro.list_review_items(&alice_actor, Some(ReviewStatus::Open))?;
    assert!(
        open_revs
            .iter()
            .any(|r| r.kind == ReviewKind::AmbiguousScope),
        "expected open AmbiguousScope review item in demo library"
    );

    // Verify all 4 Web Inspector read-only store APIs on the generated `demo-lib`!
    let eval_at = EpochMs(i64::MAX / 2);
    let alice_cards = ro.inspector_visible_facts(&alice_actor, eval_at, None, 50)?;
    assert!(alice_cards.iter().any(|c| c.fact_id == active_dec_id));
    assert!(alice_cards.iter().any(|c| c.fact_id == guard_id));
    assert!(alice_cards.iter().any(|c| c.fact_id == priv_id));
    let highlights =
        ro.inspector_fact_highlights(&alice_actor, eval_at, std::slice::from_ref(&guard_id))?;
    let guard_hl = highlights.get(&guard_id).ok_or("missing guard highlight")?;
    assert!(guard_hl.excerpt_highlighted.contains("kubectl delete"));
    let alice_tl = ro.inspector_timeline(&alice_actor, eval_at, 50, None)?;
    for expected_cat in ["supersede", "withdraw", "expire", "stale_premise"] {
        assert!(
            alice_tl.iter().any(|e| e.category == expected_cat),
            "expected category `{expected_cat}` in demo-lib timeline, got: {alice_tl:?}"
        );
    }
    assert!(
        ro.inspector_visible_facts(&carol_actor, eval_at, None, 50)?
            .is_empty(),
        "bystander carol must see 0 visible facts in inspector"
    );
    assert!(
        ro.inspector_timeline(&carol_actor, eval_at, 50, None)?
            .is_empty(),
        "bystander carol must see 0 timeline events in inspector"
    );

    // Bystander `carol` CLI search & audit log zero-leakage check:
    let carol_search = run(
        &lib_dir,
        &["--as", "carol", "search", "ALICE_PRIVATE_MEMO_774"],
    )?;
    assert!(
        carol_search["results"]
            .as_array()
            .is_some_and(std::vec::Vec::is_empty),
        "carol must see 0 hits for alice's private memo"
    );
    let carol_log = run(&lib_dir, &["--as", "carol", "log"])?;
    let carol_log_str = serde_json::to_string(&carol_log)?;
    assert!(!carol_log_str.contains("ALICE_PRIVATE_MEMO_774"));
    assert!(!carol_log_str.contains("SQLite WAL"));

    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn install_script_verifies_sha256_installs_atomically_and_rejects_tampered_checksum_with_zero_residue(
) -> Result {
    let dir = tempfile::tempdir()?;
    let script_path = repo_install_script_path();
    assert!(
        script_path.is_file(),
        "missing scripts/install.sh at {}",
        script_path.display()
    );
    assert_ne!(
        fs::metadata(&script_path)?.permissions().mode() & 0o111,
        0,
        "scripts/install.sh must be executable (chmod +x)"
    );

    // Package the compiled `kp` binary into a real `kipo-<target>.tar.gz` archive.
    let target = detect_installer_target_triple()?;
    let asset_name = format!("kipo-{target}.tar.gz");
    let stage_dir = dir.path().join("pkg_stage");
    fs::create_dir_all(&stage_dir)?;
    fs::copy(env!("CARGO_BIN_EXE_kp"), stage_dir.join("kp"))?;
    fs::set_permissions(stage_dir.join("kp"), fs::Permissions::from_mode(0o755))?;

    let archive_path = dir.path().join(&asset_name);
    let tar_status = Command::new("tar")
        .arg("-czf")
        .arg(&archive_path)
        .arg("-C")
        .arg(&stage_dir)
        .arg(".")
        .status()?;
    assert!(tar_status.success(), "tar -czf failed");

    let archive_bytes = fs::read(&archive_path)?;
    let real_sha = ContentHash::digest(&archive_bytes).to_string();

    let valid_sums = dir.path().join("SHA256SUMS.valid");
    fs::write(&valid_sums, format!("{real_sha}  {asset_name}\n"))?;

    let tampered_sums = dir.path().join("SHA256SUMS.tampered");
    let bad_sha = format!("{}0", &real_sha[..real_sha.len().saturating_sub(1)]);
    let bad_sha = if bad_sha == real_sha {
        format!("{}1", &real_sha[..real_sha.len().saturating_sub(1)])
    } else {
        bad_sha
    };
    fs::write(&tampered_sums, format!("{bad_sha}  {asset_name}\n"))?;

    let missing_entry_sums = dir.path().join("SHA256SUMS.missing");
    fs::write(
        &missing_entry_sums,
        format!("{real_sha}  kipo-other-target.tar.gz\n"),
    )?;

    // 1. Tampered SHA256SUMS MUST fail non-zero and leave ZERO residue in target install directory!
    let tampered_install_dir = dir.path().join("tampered_target_bin");
    let tampered_run = Command::new("/bin/sh")
        .arg(&script_path)
        .env("KP_INSTALL_DIR", &tampered_install_dir)
        .env("KP_INSTALL_ARCHIVE", &archive_path)
        .env("KP_INSTALL_SHA256SUMS", &tampered_sums)
        .output()?;
    assert!(
        !tampered_run.status.success(),
        "tampered SHA256SUMS must be rejected!"
    );
    let tampered_stderr = String::from_utf8_lossy(&tampered_run.stderr);
    assert!(
        tampered_stderr.contains("checksum mismatch"),
        "expected checksum mismatch message on stderr, got: {tampered_stderr}"
    );
    assert!(
        !tampered_install_dir.exists(),
        "failed checksum install must leave zero directory/binary residue at {}",
        tampered_install_dir.display()
    );

    // 2. Missing archive entry in SHA256SUMS MUST also fail non-zero with zero residue!
    let missing_install_dir = dir.path().join("missing_entry_bin");
    let missing_run = Command::new("/bin/sh")
        .arg(&script_path)
        .env("KP_INSTALL_DIR", &missing_install_dir)
        .env("KP_INSTALL_ARCHIVE", &archive_path)
        .env("KP_INSTALL_SHA256SUMS", &missing_entry_sums)
        .output()?;
    assert!(!missing_run.status.success());
    assert!(!missing_install_dir.exists());

    // 3. Valid offline archive + SHA256SUMS installs `kp` (`0755`), passes `--version` self-check,
    //    and the newly installed binary runs `kp demo --json` end-to-end!
    let ok_install_dir = dir.path().join("installed_bin");
    let ok_run = Command::new("/bin/sh")
        .arg(&script_path)
        .env("KP_INSTALL_DIR", &ok_install_dir)
        .env("KP_INSTALL_ARCHIVE", &archive_path)
        .env("KP_INSTALL_SHA256SUMS", &valid_sums)
        .output()?;
    assert!(
        ok_run.status.success(),
        "valid install.sh failed: stdout={}, stderr={}",
        String::from_utf8_lossy(&ok_run.stdout),
        String::from_utf8_lossy(&ok_run.stderr)
    );

    let installed_kp = ok_install_dir.join("kp");
    assert!(installed_kp.is_file());
    assert_eq!(
        fs::metadata(&installed_kp)?.permissions().mode() & 0o777,
        0o755
    );

    let ver_out = Command::new(&installed_kp).arg("--version").output()?;
    assert!(ver_out.status.success());

    let installed_demo = success(
        Command::new(&installed_kp)
            .env("KP_OFFLINE", "1")
            .args(["--json", "demo"])
            .output()?,
    )?;
    assert_eq!(installed_demo["carol_isolation_verified"], true);

    // 4. Also exercise live HTTP download path (`KP_RELEASE_BASE_URL`) when loopback bind is permitted.
    if let Ok(listener) = tokio::net::TcpListener::bind("127.0.0.1:0").await {
        let addr = listener.local_addr()?;
        let base_url = format!("http://{addr}");
        let sums_body = fs::read(&valid_sums)?;
        let archive_body = archive_bytes.clone();
        let asset_route = format!("/latest/download/{asset_name}");

        let app = Router::new()
            .route(
                &asset_route,
                get(move || {
                    let b = archive_body.clone();
                    async move { (StatusCode::OK, b) }
                }),
            )
            .route(
                "/latest/download/SHA256SUMS",
                get(move || {
                    let s = sums_body.clone();
                    async move { (StatusCode::OK, s) }
                }),
            );
        let srv = tokio::spawn(async move { axum::serve(listener, app).await });

        let http_install_dir = dir.path().join("http_installed_bin");
        let http_run = Command::new("/bin/sh")
            .arg(&script_path)
            .env("KP_INSTALL_DIR", &http_install_dir)
            .env("KP_RELEASE_BASE_URL", &base_url)
            .env_remove("KP_INSTALL_ARCHIVE")
            .env_remove("KP_INSTALL_SHA256SUMS")
            .output()?;
        srv.abort();

        assert!(
            http_run.status.success(),
            "HTTP release install failed: stderr={}",
            String::from_utf8_lossy(&http_run.stderr)
        );
        assert!(http_install_dir.join("kp").is_file());
    }

    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fresh_env_cli_demo_and_live_daemon_web_inspector_journey() -> Result {
    let dir = tempfile::tempdir()?;
    let runtime_root = tempfile::Builder::new().prefix("kp-m4-rt-").tempdir()?;
    let demo_dir = dir.path().join("walkthrough");

    // 1. Fresh environment: `kp demo --dir <walkthrough> --json` (< 2s)
    let demo_res = success(
        Command::new(env!("CARGO_BIN_EXE_kp"))
            .env("KP_OFFLINE", "1")
            .env("TMPDIR", runtime_root.path())
            .args([
                "--json",
                "demo",
                "--dir",
                demo_dir.to_str().ok_or("utf8 demo_dir")?,
            ])
            .output()?,
    )?;
    let lib_dir = PathBuf::from(demo_res["library_dir"].as_str().ok_or("library_dir")?);

    // 2. One-command `kp --as alice inspect` spawns `kp daemon` and opens `127.0.0.1` Web Inspector!
    let inspect_alice = success(
        Command::new(env!("CARGO_BIN_EXE_kp"))
            .env("KP_OFFLINE", "1")
            .env("TMPDIR", runtime_root.path())
            .arg("--path")
            .arg(&lib_dir)
            .args(["--as", "alice", "--json", "inspect"])
            .output()?,
    )?;
    let alice_url = inspect_alice["url"].as_str().ok_or("alice url")?.to_owned();
    let bind_addr = inspect_alice["bind_addr"]
        .as_str()
        .ok_or("bind_addr")?
        .to_owned();
    assert!(
        alice_url.starts_with("http://127.0.0.1:") && alice_url.contains("/?token="),
        "unexpected inspector URL: {alice_url}"
    );
    assert_eq!(inspect_alice["viewer"], "alice");

    // 3. `kp --as carol daemon start --web` attaches to the ALREADY-RUNNING warm daemon on the same
    //    `127.0.0.1` listener and mints an independent viewer-locked capability token for `carol`!
    let start_carol = success(
        Command::new(env!("CARGO_BIN_EXE_kp"))
            .env("KP_OFFLINE", "1")
            .env("TMPDIR", runtime_root.path())
            .arg("--path")
            .arg(&lib_dir)
            .args(["--as", "carol", "--json", "daemon", "start", "--web"])
            .output()?,
    )?;
    assert_eq!(start_carol["already_running"], true);
    assert_eq!(start_carol["web_inspector_bind"], bind_addr);
    assert_eq!(start_carol["web_inspector_viewer"], "carol");
    let carol_url = start_carol["web_inspector_url"]
        .as_str()
        .ok_or("carol url")?
        .to_owned();
    assert_ne!(alice_url, carol_url);

    let (_, alice_tok) = alice_url.split_once("?token=").ok_or("alice token")?;
    let (_, carol_tok) = carol_url.split_once("?token=").ok_or("carol token")?;
    let base_origin = format!("http://{bind_addr}");

    let http = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;

    // 4. Unauthenticated request is rejected with 401; `?token=` exchanges for 303 + HttpOnly cookie + strict CSP!
    let unauth = http
        .get(format!("{base_origin}/api/v1/overview"))
        .send()
        .await?;
    assert_eq!(unauth.status().as_u16(), 401);

    let login_resp = http.get(&alice_url).send().await?;
    assert_eq!(login_resp.status().as_u16(), 303);
    let set_cookie = login_resp
        .headers()
        .get("set-cookie")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(set_cookie.contains("HttpOnly") && set_cookie.contains("SameSite=Strict"));
    let csp = login_resp
        .headers()
        .get("content-security-policy")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(csp.contains("default-src 'none'") && !csp.contains("unsafe-inline"));

    // 5. `alice` fetches all 4 columns via `/api/v1/overview` and `/api/v1/why/{fact_id}`:
    let alice_overview: Value = http
        .get(format!("{base_origin}/api/v1/overview"))
        .header("Authorization", format!("Bearer {alice_tok}"))
        .header("Origin", &base_origin)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(alice_overview["viewer"], "alice");
    let alice_dump = serde_json::to_string(&alice_overview)?;
    assert!(alice_dump.contains("ALICE_PRIVATE_MEMO_774"));
    let alice_facts = alice_overview["facts"].as_array().ok_or("alice facts")?;
    assert!(!alice_facts.is_empty());
    let alice_timeline = alice_overview["timeline"]
        .as_array()
        .ok_or("alice timeline")?;
    for cat in ["supersede", "withdraw", "expire", "stale_premise"] {
        assert!(
            alice_timeline.iter().any(|e| e["category"] == cat),
            "expected timeline category `{cat}` in Web Inspector overview"
        );
    }
    assert!(!alice_overview["reviews"]
        .as_array()
        .ok_or("alice reviews")?
        .is_empty());

    let first_fid = alice_facts[0]["fact_id"].as_str().ok_or("first fact_id")?;
    let why_detail: Value = http
        .get(format!("{base_origin}/api/v1/why/{first_fid}"))
        .header("Authorization", format!("Bearer {alice_tok}"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(why_detail["fact"]["fact_id"], first_fid);
    assert!(why_detail["highlights"].is_object());

    // 6. `carol` token (even with `?viewer=alice` spoof!) remains strictly isolated to `carol`:
    let carol_overview: Value = http
        .get(format!("{base_origin}/api/v1/overview?viewer=alice"))
        .header("Authorization", format!("Bearer {carol_tok}"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(carol_overview["viewer"], "carol");
    assert!(carol_overview["reviews"]
        .as_array()
        .ok_or("carol reviews")?
        .is_empty());
    let carol_dump = serde_json::to_string(&carol_overview)?;
    assert!(!carol_dump.contains("ALICE_PRIVATE_MEMO_774"));
    let priv_fid = demo_res["private_memo_id"]
        .as_str()
        .ok_or("private_memo_id")?;
    let carol_why_priv = http
        .get(format!("{base_origin}/api/v1/why/{priv_fid}"))
        .header("Authorization", format!("Bearer {carol_tok}"))
        .send()
        .await?;
    assert_eq!(carol_why_priv.status().as_u16(), 404);

    // 7. `kp daemon status` reports `web_inspector_url` isolated per `--as <actor>` (never leaking
    //    `alice`'s 256-bit token to `carol`, `outsider`, or revoked `dave`), and `kp daemon stop` shuts down cleanly!
    let st_out = success(
        Command::new(env!("CARGO_BIN_EXE_kp"))
            .env("KP_OFFLINE", "1")
            .env("TMPDIR", runtime_root.path())
            .arg("--path")
            .arg(&lib_dir)
            .args(["--json", "daemon", "status"])
            .output()?,
    )?;
    assert_eq!(st_out["web_inspector_viewer"], "alice");
    assert_eq!(st_out["web_inspector_url"], alice_url);

    let st_carol = success(
        Command::new(env!("CARGO_BIN_EXE_kp"))
            .env("KP_OFFLINE", "1")
            .env("TMPDIR", runtime_root.path())
            .arg("--path")
            .arg(&lib_dir)
            .args(["--as", "carol", "--json", "daemon", "status"])
            .output()?,
    )?;
    assert_eq!(st_carol["web_inspector_viewer"], "carol");
    let st_carol_url = st_carol["web_inspector_url"]
        .as_str()
        .ok_or("st_carol web_inspector_url")?;
    assert_eq!(st_carol_url, carol_url);
    assert!(
        !st_carol_url.contains(alice_tok),
        "`kp --as carol daemon status` must never leak alice's capability token"
    );

    let outsider_st = Command::new(env!("CARGO_BIN_EXE_kp"))
        .env("KP_OFFLINE", "1")
        .env("TMPDIR", runtime_root.path())
        .arg("--path")
        .arg(&lib_dir)
        .args(["--as", "outsider", "--json", "daemon", "status"])
        .output()?;
    assert!(
        !outsider_st.status.success(),
        "`kp --as outsider daemon status` must reject non-members"
    );
    assert!(!String::from_utf8_lossy(&outsider_st.stdout).contains(alice_tok));
    assert!(!String::from_utf8_lossy(&outsider_st.stderr).contains(alice_tok));

    {
        let mut store_rw = Store::open(&lib_dir)?;
        let dave_actor: kp_model::ActorId = "dave".parse()?;
        let _ =
            store_rw.add_member(dave_actor.clone(), kp_model::MemberRole::Member, EpochMs(0))?;
        store_rw.revoke_member(&dave_actor, EpochMs(1_000))?;
    }
    let revoked_st = Command::new(env!("CARGO_BIN_EXE_kp"))
        .env("KP_OFFLINE", "1")
        .env("TMPDIR", runtime_root.path())
        .arg("--path")
        .arg(&lib_dir)
        .args(["--as", "dave", "--json", "daemon", "status"])
        .output()?;
    assert!(
        !revoked_st.status.success(),
        "`kp --as dave daemon status` must reject revoked members"
    );
    assert!(!String::from_utf8_lossy(&revoked_st.stdout).contains(alice_tok));
    assert!(!String::from_utf8_lossy(&revoked_st.stderr).contains(alice_tok));

    let stop_out = success(
        Command::new(env!("CARGO_BIN_EXE_kp"))
            .env("KP_OFFLINE", "1")
            .env("TMPDIR", runtime_root.path())
            .arg("--path")
            .arg(&lib_dir)
            .args(["--json", "daemon", "stop"])
            .output()?,
    )?;
    assert_eq!(stop_out["status"], "stopped");

    Ok(())
}
