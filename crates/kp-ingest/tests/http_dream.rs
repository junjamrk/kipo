#![cfg(feature = "http-dream")]

mod support;

use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread::JoinHandle;
use std::time::Duration;

use kp_ingest::dream::{dream, HttpDreamAttributor};
use kp_model::{ContrastKind, EpochMs, FactKind};
use kp_store::Episode;
use serde_json::{json, Value};

use support::{library, Result};

fn mock(
    body: String,
    status: &str,
    extra: &str,
) -> Result<(String, JoinHandle<std::result::Result<Value, String>>)> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let url = format!(
        "http://localhost:{}/v1/chat/completions",
        listener.local_addr()?.port()
    );
    let header=format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n",body.len());
    let task = std::thread::spawn(move || -> std::result::Result<Value, String> {
        let (mut socket, _) = listener.accept().map_err(|e| e.to_string())?;
        socket
            .set_read_timeout(Some(Duration::from_secs(10)))
            .map_err(|e| e.to_string())?;
        let mut bytes = Vec::new();
        let (offset, length) = loop {
            let mut chunk = [0_u8; 4096];
            let n = socket.read(&mut chunk).map_err(|e| e.to_string())?;
            if n == 0 {
                return Err("request ended early".into());
            }
            bytes.extend_from_slice(&chunk[..n]);
            if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&bytes[..end]).to_lowercase();
                let length = headers
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .ok_or("content length")?
                    .trim()
                    .parse::<usize>()
                    .map_err(|e| e.to_string())?;
                break (end + 4, length);
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
        Ok(value)
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
            HttpDreamAttributor::new(url, "model", false).is_err(),
            "{url}"
        );
    }
    assert!(HttpDreamAttributor::new("https://example.com/dream", "model", true).is_ok());
    assert!(HttpDreamAttributor::new("http://[::1]/dream", "model", false).is_ok());
    assert!(HttpDreamAttributor::new("http://localhost/dream", "", false).is_err());
    Ok(())
}

#[tokio::test]
async fn http_parses_inv_dom_and_keep_base_with_redacted_payloads() -> Result {
    let actor = support::options()?.actor;
    for dimension in ["INV", "DOM", "KEEP_BASE"] {
        let (_dir, mut store) = library()?;
        store.record_episodes(&[episode()], EpochMs(10))?;
        let decision = if dimension == "KEEP_BASE" {
            json!({"decision":"KEEP_BASE"})
        } else {
            json!({"decision":"UPDATE","attribution":{"skill_slug":"checked-request","trigger":"constructing an API request","wrong_move":"Assume the request shape is unchanged","verified_fix":"Compare request fields against the selected API version before sending.","dimension":dimension,"cited_premise_ids":[]}})
        };
        let body = if dimension == "INV" {
            json!({"choices":[{"message":{"content":decision.to_string()}}]}).to_string()
        } else {
            decision.to_string()
        };
        let (url, task) = mock(body, "200 OK", "")?;
        let report = dream(
            &mut store,
            &actor,
            EpochMs(20),
            &HttpDreamAttributor::new(&url, "local-model", false)?,
        )
        .await?;
        let payload = task.join().map_err(|_| "mock panicked")??;
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
async fn oversized_redirect_and_malformed_responses_leave_episode_pending() -> Result {
    let trap = TcpListener::bind("127.0.0.1:0")?;
    trap.set_nonblocking(true)?;
    for (body,status,extra) in [
        ("x".repeat(1_048_577),"200 OK",String::new()),
        ("{}".into(),"302 Found",format!("Location: http://{}/stolen\r\n",trap.local_addr()?)),
        ("{\"decision\":\"invented\"}".into(),"200 OK",String::new()),
        (json!({"decision":"UPDATE","attribution":{"skill_slug":"../escape","trigger":"x","wrong_move":"a","verified_fix":"b","dimension":"INV","cited_premise_ids":[]}}).to_string(),"200 OK",String::new()),
    ] {
        let (_dir,mut store)=library()?;
        store.record_episodes(&[episode()],EpochMs(10))?;
        let (url,task)=mock(body,status,&extra)?;
        assert!(dream(&mut store,&"alice".parse()?,EpochMs(20),&HttpDreamAttributor::new(&url,"model",false)?).await.is_err());
        task.join().map_err(|_|"mock panicked")??;
        assert_eq!(store.pending_episodes(&"alice".parse()?)?.len(),1);
        assert_eq!(store.status()?.facts,0);
    }
    assert!(trap.accept().is_err());
    Ok(())
}
