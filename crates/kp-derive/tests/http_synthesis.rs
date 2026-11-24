#![cfg(feature = "http-synthesis")]

use axum::{routing::post, Json, Router};
use kp_derive::{synthesize_scoped, FunnelOptions, HttpSynthesizer, Memory};
use kp_model::{AudienceSpec, EpochMs, FactKind, LibraryMode, ValidityWindow};
use kp_store::{InitOptions, Store};
use serde_json::{json, Value};
use std::collections::BTreeSet;

#[tokio::test]
async fn optional_http_synthesizer_receives_only_redacted_shared_claims(
) -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let mut store = Store::init(
        dir.path(),
        InitOptions {
            library_id: "local".parse()?,
            name: "Local".into(),
            mode: LibraryMode::Personal,
            owner: "alice".parse()?,
            created_at: EpochMs(0),
        },
    )?;
    kp_derive::remember(
        &mut store,
        Memory {
            subject: "shared".into(),
            predicate: "p".into(),
            value: json!("gho_secret"),
            kind: FactKind::FactSlot,
            logical_key: None,
            tags: BTreeSet::new(),
            entities: BTreeSet::new(),
            valid: ValidityWindow {
                valid_from: EpochMs(0),
                valid_until: EpochMs::OPEN_END,
            },
        },
        &FunnelOptions {
            actor: "alice".parse()?,
            audience: AudienceSpec::Explicit(BTreeSet::from(["alice".parse()?, "bob".parse()?])),
            at: EpochMs(0),
        },
    )?;
    kp_derive::add_markdown(
        &mut store,
        "chunk.md",
        "raw-chunk-only-secret",
        &FunnelOptions {
            actor: "alice".parse()?,
            audience: AudienceSpec::Explicit(BTreeSet::from(["alice".parse()?, "bob".parse()?])),
            at: EpochMs(0),
        },
        None,
    )?;
    assert!(store
        .retrieval_snapshot(&"alice".parse()?, EpochMs(1))?
        .documents()
        .iter()
        .any(|d| d.kind == "chunk"));
    let app = Router::new().route("/synthesize",post(|Json(body):Json<Value>| async move {
        assert!(!body.to_string().contains("gho_secret"));
        assert!(!body.to_string().contains("raw-chunk-only-secret"));
        assert!(body.get("chunks").is_none());
        let id = body["facts"][0]["id"].as_str().unwrap_or("");
        Json(json!([{"text":format!("Summary [premise:{id}] [premise:invented]"),"unverified_gaps":[]}]))
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}/synthesize", listener.local_addr()?);
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let ids = synthesize_scoped(
        &mut store,
        &"alice".parse()?,
        &BTreeSet::from(["alice".parse()?, "bob".parse()?]),
        "ghp_query_secret",
        EpochMs(1),
        &HttpSynthesizer::new(&url, false)?,
    )
    .await?;
    assert_eq!(ids.len(), 1);
    let graph = store.derivation_view()?;
    assert!(!graph.facts[&ids[0]].object.to_string().contains("invented"));
    task.abort();
    Ok(())
}

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn empty_store() -> TestResult<(tempfile::TempDir, Store)> {
    let dir = tempfile::tempdir()?;
    let store = Store::init(
        dir.path(),
        InitOptions {
            library_id: "local".parse()?,
            name: "Local".into(),
            mode: LibraryMode::Personal,
            owner: "alice".parse()?,
            created_at: EpochMs(0),
        },
    )?;
    Ok((dir, store))
}

#[test]
fn synthesizer_remote_endpoints_require_explicit_cloud_consent() {
    for endpoint in [
        "https://example.com/synthesize",
        "http://192.168.1.10/synthesize",
        "http://169.254.169.254/",
    ] {
        assert!(HttpSynthesizer::new(endpoint, false).is_err());
        assert!(HttpSynthesizer::new(endpoint, true).is_ok());
    }
    for endpoint in [
        "http://localhost:8000/",
        "http://127.0.0.1:8000/",
        "http://[::1]:8000/",
    ] {
        assert!(HttpSynthesizer::new(endpoint, false).is_ok());
    }
    for endpoint in [
        "http://user:secret@localhost/",
        "http://localhost/?token=secret",
        "http://localhost/#fragment",
        "file:///tmp/model",
    ] {
        assert!(HttpSynthesizer::new(endpoint, true).is_err());
    }
}

#[tokio::test]
async fn synthesis_redirect_does_not_forward_input_to_destination() -> TestResult {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    let hits = Arc::new(AtomicUsize::new(0));
    let observed = hits.clone();
    let destination = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let target = format!("http://{}/leak", destination.local_addr()?);
    let sink = Router::new().route(
        "/leak",
        post(move || {
            observed.fetch_add(1, Ordering::SeqCst);
            async { Json(json!([])) }
        }),
    );
    let sink_task = tokio::spawn(async move {
        let _ = axum::serve(destination, sink).await;
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}/synthesize", listener.local_addr()?);
    let app = Router::new().route(
        "/synthesize",
        post(move || {
            let target = target.clone();
            async move { axum::response::Redirect::temporary(&target) }
        }),
    );
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let (_dir, mut store) = empty_store()?;
    let result = synthesize_scoped(
        &mut store,
        &"alice".parse()?,
        &BTreeSet::from(["alice".parse()?]),
        "private query",
        EpochMs(0),
        &HttpSynthesizer::new(&url, false)?,
    )
    .await;
    assert!(result.is_err());
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    task.abort();
    sink_task.abort();
    Ok(())
}

#[tokio::test]
async fn synthesis_body_limit_handles_content_length_and_chunked_responses() -> TestResult {
    use std::io::{Read, Write};
    for (size, chunked, accepted) in [
        (1_048_576, false, true),
        (1_048_577, false, false),
        (1_048_577, true, false),
    ] {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let url = format!("http://{}/synthesize", listener.local_addr()?);
        let server = std::thread::spawn(move || -> std::io::Result<()> {
            let (mut stream, _) = listener.accept()?;
            stream.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
            let mut request = Vec::new();
            let mut buffer = [0; 4096];
            loop {
                let len = stream.read(&mut buffer)?;
                if len == 0 {
                    return Err(std::io::ErrorKind::UnexpectedEof.into());
                }
                request.extend_from_slice(&buffer[..len]);
                if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let header = String::from_utf8_lossy(&request[..end]).to_lowercase();
                    let length = header
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            let body = format!("[{}]", " ".repeat(size - 2));
            let response = if chunked {
                format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{size:x}\r\n{body}\r\n0\r\n\r\n")
            } else {
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {size}\r\nConnection: close\r\n\r\n{body}"
                )
            };
            stream.write_all(response.as_bytes())
        });
        let (_dir, mut store) = empty_store()?;
        let result = synthesize_scoped(
            &mut store,
            &"alice".parse()?,
            &BTreeSet::from(["alice".parse()?]),
            "query",
            EpochMs(0),
            &HttpSynthesizer::new(&url, false)?,
        )
        .await;
        assert_eq!(result.is_ok(), accepted);
        if let Err(error) = result {
            assert!(error.to_string().contains("response exceeds one MiB"));
        }
        server.join().map_err(|_| "server panicked")??;
    }
    Ok(())
}

#[test]
fn synthesis_ignores_proxy_environment() -> TestResult {
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command.args([
        "--exact",
        "optional_http_synthesizer_receives_only_redacted_shared_claims",
    ]);
    for key in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        command.env(key, "http://127.0.0.1:1");
    }
    command.env("NO_PROXY", "").env("no_proxy", "");
    let output = command.output()?;
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}
