mod support;

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread::JoinHandle;
use std::time::Duration;

use kp_ingest::dream::{dream, LlmDreamAttributor, StructuredDreamAttributor};
use kp_model::{Attribution, ContrastKind, EpochMs, FactKind, RsiDimension, TrustTier};
use kp_store::Episode;
use serde_json::{json, Value};

use support::{library, options, Result};

type MockServerHandle = JoinHandle<std::result::Result<(String, Value), String>>;

fn mock_endpoint(
    path: &str,
    body: String,
    status: &str,
    extra: &str,
) -> Result<(String, MockServerHandle)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let url = format!("http://localhost:{}{path}", listener.local_addr()?.port());
    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n",
        body.len()
    );
    let task = std::thread::spawn(move || -> std::result::Result<(String, Value), String> {
        let (mut socket, _) = listener.accept().map_err(|e| e.to_string())?;
        socket
            .set_read_timeout(Some(Duration::from_secs(10)))
            .map_err(|e| e.to_string())?;
        let mut bytes = Vec::new();
        let (offset, length, headers) = loop {
            let mut chunk = [0_u8; 4096];
            let n = socket.read(&mut chunk).map_err(|e| e.to_string())?;
            if n == 0 {
                return Err("request ended early".into());
            }
            bytes.extend_from_slice(&chunk[..n]);
            if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                let raw_headers = String::from_utf8_lossy(&bytes[..end]).to_lowercase();
                let length = raw_headers
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .ok_or("content length")?
                    .trim()
                    .parse::<usize>()
                    .map_err(|e| e.to_string())?;
                break (end + 4, length, raw_headers);
            }
        };
        while bytes.len() < offset + length {
            let mut chunk = [0_u8; 4096];
            let n = socket.read(&mut chunk).map_err(|e| e.to_string())?;
            if n == 0 {
                return Err("request body ended early".into());
            }
            bytes.extend_from_slice(&chunk[..n]);
        }
        let value =
            serde_json::from_slice(&bytes[offset..offset + length]).map_err(|e| e.to_string())?;
        socket
            .write_all(header.as_bytes())
            .map_err(|e| e.to_string())?;
        if let Err(error) = socket.write_all(body.as_bytes()) {
            if !matches!(
                error.kind(),
                std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
            ) {
                return Err(error.to_string());
            }
        }
        Ok((headers, value))
    });
    Ok((url, task))
}

fn episode() -> Episode {
    Episode {
        signal_kind: ContrastKind::HumanCorrection,
        session_id: "http".into(),
        before_trace: json!("wrong request api_key=SECRET_OUTBOUND_BEFORE"),
        after_trace: json!("No, use the correct request. Bearer SECRET_OUTBOUND_AFTER"),
        step_ref: "step:1 ghp_SECRET_OUTBOUND_STEP".into(),
        environment: json!({"actor":"alice","token":"SECRET_OUTBOUND_ENV"}),
    }
}

#[test]
fn transport_rejects_remote_urls_credentials_and_unapproved_cloud() -> Result {
    for url in [
        "https://example.com/dream",
        "http://10.0.0.1/dream",
        "http://localhost.evil/dream",
        "file:///tmp/model",
        "http://user:pass@localhost/dream",
        "http://localhost/dream?token=secret",
        "http://localhost/dream#fragment",
    ] {
        assert!(
            LlmDreamAttributor::from_http_endpoint(url, "model", false, None).is_err(),
            "{url}"
        );
    }
    assert!(
        LlmDreamAttributor::from_http_endpoint("https://example.com/v1", "model", true, None)
            .is_ok()
    );
    assert!(
        LlmDreamAttributor::from_http_endpoint("http://[::1]/v1", "model", false, None).is_ok()
    );
    assert!(
        LlmDreamAttributor::from_http_endpoint("http://localhost/v1", "", false, None).is_err()
    );
    assert!(LlmDreamAttributor::from_http_endpoint(
        "http://localhost/v1",
        "model",
        false,
        Some("   ".into())
    )
    .is_err());
    Ok(())
}

#[tokio::test]
async fn http_openai_and_anthropic_parse_inv_dom_and_keep_base_with_redacted_payloads() -> Result {
    let actor = options()?.actor;
    for dimension in ["INV", "DOM", "KEEP_BASE"] {
        let (_dir, mut store) = library()?;
        store.record_episodes(&[episode()], EpochMs(10))?;
        let decision = if dimension == "KEEP_BASE" {
            json!({
                "decision": "KEEP_BASE",
                "rejection_class": "one_off_task_redirect",
                "attribution": null,
                "extra_rules": []
            })
        } else {
            json!({
                "decision": "UPDATE",
                "rejection_class": null,
                "attribution": {
                    "skill_slug": "checked-request",
                    "trigger": "constructing an API request",
                    "wrong_move": "Assume the request shape is unchanged",
                    "verified_fix": "Compare request fields against the selected API version before sending.",
                    "dimension": dimension,
                    "cited_premise_ids": []
                },
                "extra_rules": []
            })
        };
        let is_anthropic = dimension == "DOM";
        let (path, wire_body, key) = if is_anthropic {
            (
                "/v1/messages",
                json!({
                    "content": [{
                        "type": "tool_use",
                        "name": "kp_dream_distill",
                        "input": decision
                    }]
                })
                .to_string(),
                Some("sk-ant-test-key".to_owned()),
            )
        } else {
            (
                "/v1/chat/completions",
                json!({"choices": [{"message": {"content": decision.to_string()}}]}).to_string(),
                Some("sk-oai-test-key".to_owned()),
            )
        };
        let (url, task) = mock_endpoint(path, wire_body, "200 OK", "")?;
        let attributor = LlmDreamAttributor::from_http_endpoint(&url, "local-model", false, key)?;
        let report = dream(&mut store, &actor, EpochMs(20), &attributor).await?;
        let (headers, payload) = task.join().map_err(|_| "mock panicked")??;
        if is_anthropic {
            assert!(headers.contains("x-api-key: sk-ant-test-key"));
            assert!(headers.contains("anthropic-version: 2023-06-01"));
        } else {
            assert!(headers.contains("authorization: bearer sk-oai-test-key"));
        }
        let wire = payload.to_string();
        assert!(!wire.contains("SECRET_OUTBOUND"));
        assert!(
            wire.contains("[REDACTED]")
                && wire.contains("EVIDENCE_BOUNDARY")
                && wire.contains("KEEP_BASE")
        );
        assert_eq!(payload["model"], "local-model");
        assert_eq!(report.kept_base, usize::from(dimension == "KEEP_BASE"));
        if dimension != "KEEP_BASE" {
            let bullet = store
                .visible_facts(&"alice".parse()?, EpochMs(20))?
                .into_iter()
                .find(|f| f.kind == FactKind::PlaybookBullet)
                .ok_or("bullet")?;
            assert_eq!(bullet.object["attribution"]["dimension"], dimension);
        }
    }
    Ok(())
}

#[tokio::test]
async fn redirect_non_2xx_and_malformed_responses_leave_episode_pending() -> Result {
    let trap = TcpListener::bind("127.0.0.1:0")?;
    trap.set_nonblocking(true)?;
    for (body, status, extra) in [
        ("not-json-at-all".into(), "200 OK", String::new()),
        ("{}".into(), "502 Bad Gateway", String::new()),
        (
            "{}".into(),
            "302 Found",
            format!("Location: http://{}/stolen\r\n", trap.local_addr()?),
        ),
        (
            json!({"choices":[{"message":{"content":"{\"decision\":\"invented\"}"}}]}).to_string(),
            "200 OK",
            String::new(),
        ),
        (
            json!({
                "choices": [{
                    "message": {
                        "content": json!({
                            "decision": "UPDATE",
                            "attribution": {
                                "skill_slug": "../escape",
                                "trigger": "x",
                                "wrong_move": "a",
                                "verified_fix": "b",
                                "dimension": "INV",
                                "cited_premise_ids": []
                            }
                        }).to_string()
                    }
                }]
            })
            .to_string(),
            "200 OK",
            String::new(),
        ),
    ] {
        let (_dir, mut store) = library()?;
        store.record_episodes(&[episode()], EpochMs(10))?;
        let (url, task) = mock_endpoint("/v1/chat/completions", body, status, &extra)?;
        let attributor = LlmDreamAttributor::from_http_endpoint(&url, "model", false, None)?;
        assert!(
            dream(&mut store, &"alice".parse()?, EpochMs(20), &attributor)
                .await
                .is_err()
        );
        task.join().map_err(|_| "mock panicked")??;
        assert_eq!(store.pending_episodes(&"alice".parse()?)?.len(), 1);
        assert_eq!(store.status()?.facts, 0);
    }
    assert!(trap.accept().is_err());
    Ok(())
}

#[tokio::test]
async fn external_untrusted_premise_forces_keep_base_in_dream_and_store_defense_in_depth() -> Result
{
    let (_dir, mut store) = library()?;
    let actor = options()?.actor;
    let mut untrusted_input = kp_derive::base_fact(
        &store,
        kp_derive::Memory {
            subject: "untrusted-web-note".into(),
            predicate: "kp.core.observation".into(),
            value: json!("External blog post observation"),
            kind: FactKind::Observation,
            logical_key: Some("untrusted-web-obs".parse()?),
            tags: Default::default(),
            entities: Default::default(),
            valid: kp_model::ValidityWindow {
                valid_from: EpochMs(0),
                valid_until: EpochMs::OPEN_END,
            },
        },
        &options()?,
    )?;
    untrusted_input.fact.trust_tier = TrustTier::ExternalUntrusted;
    let untrusted_fid = untrusted_input.fact.id.clone();
    store.remember(std::slice::from_ref(&untrusted_input))?;

    let ep_1 = Episode {
        signal_kind: ContrastKind::HumanCorrection,
        session_id: "untrusted-1".into(),
        before_trace: json!("tried direct command"),
        after_trace: json!("No, use the verified wrapper instead."),
        step_ref: "step:1".into(),
        environment: json!({"actor": actor, "premise_ids": [untrusted_fid]}),
    };
    store.record_episodes(&[ep_1], EpochMs(10))?;
    let report = dream(&mut store, &actor, EpochMs(20), &StructuredDreamAttributor).await?;
    assert_eq!(report.processed, 1);
    assert_eq!(report.kept_base, 1);
    assert_eq!(report.bullets_created, 0);

    // Direct store-layer defense-in-depth check (`apply_attribution_with_rules`).
    let ep_2 = Episode {
        signal_kind: ContrastKind::HumanCorrection,
        session_id: "untrusted-2".into(),
        before_trace: json!("tried direct command again"),
        after_trace: json!("No, always run the verified wrapper."),
        step_ref: "step:2".into(),
        environment: json!({"actor": actor, "premise_ids": [untrusted_fid]}),
    };
    store.record_episodes(&[ep_2], EpochMs(25))?;
    let pending = store.pending_episodes(&actor)?.pop().ok_or("pending")?;
    let attr = Attribution {
        skill_slug: "untrusted-attempt".parse()?,
        trigger: "running wrapper".into(),
        wrong_move: "Run direct command".into(),
        verified_fix: "Run the verified wrapper.".into(),
        dimension: RsiDimension::Inv,
        cited_premise_ids: vec![untrusted_fid],
    };
    let applied = store.apply_attribution_with_rules(
        &pending,
        &actor,
        Some(&attr),
        &["Extra injected rule.".into()],
        EpochMs(30),
    )?;
    assert!(applied.processed);
    assert!(!applied.created);
    assert_eq!(applied.bullet, None);
    assert!(store
        .visible_facts(&actor, EpochMs(40))?
        .into_iter()
        .all(|f| f.kind != FactKind::PlaybookBullet));
    Ok(())
}
