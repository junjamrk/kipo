use std::sync::{Arc, Mutex};

use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
use tokio::net::TcpListener;

use super::*;
use crate::test_support::{add, library, TestResult, AT};

#[derive(Clone)]
struct Mock {
    requests: Arc<Mutex<Vec<Value>>>,
    predicate: bool,
    reranker: bool,
    halve: bool,
}

async fn respond(
    State(mock): State<Mock>,
    Json(payload): Json<Value>,
) -> (StatusCode, Json<Value>) {
    if let Ok(mut requests) = mock.requests.lock() {
        requests.push(payload.clone());
    }
    if mock.reranker {
        return (
            StatusCode::OK,
            Json(
                json!({"results":[{"index":1,"relevance_score":0.8},{"index":0,"relevance_score":0.2}]}),
            ),
        );
    }
    let names: Vec<String> = if mock.predicate {
        payload["questions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|q| q["name"].as_str().map(str::to_owned))
            .collect()
    } else {
        payload["questions"]
            .as_object()
            .map(|q| q.keys().cloned().collect())
            .unwrap_or_default()
    };
    if mock.halve && names.len() > 1 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"too many questions: batch_limit_error"})),
        );
    }
    let answers = if mock.predicate {
        Value::Array(
            names
                .iter()
                .rev()
                .map(|name| json!({"type":"predicate","name":name,"probability":0.7}))
                .collect(),
        )
    } else {
        Value::Object(
            names
                .iter()
                .map(|name| (name.clone(), json!({"type":"noul","noul":0.7})))
                .collect(),
        )
    };
    (StatusCode::OK, Json(json!({"answers":answers})))
}

#[tokio::test]
async fn all_http_tiers_wire_formats_redaction_and_batch_halving() -> TestResult {
    for (tier, path, predicate, reranker, halve) in [
        (JudgeTier::LocalReranker, "/v1/rerank", false, true, false),
        (
            JudgeTier::LocalDecisionModel,
            "/decisions",
            false,
            false,
            true,
        ),
        (
            JudgeTier::CloudJev,
            "/api/alpha/decisions",
            false,
            false,
            true,
        ),
        (JudgeTier::CloudJev, "/v1/decisions", true, false, true),
    ] {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let mock = Mock {
            requests: requests.clone(),
            predicate,
            reranker,
            halve,
        };
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}{path}", listener.local_addr()?);
        let app = Router::new().route(path, post(respond)).with_state(mock);
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        let (_dir, mut store) = library()?;
        let secrets = "sk-123abc ghp_secret github_pat_abc AKIA1234567890123456 xoxb-secret Bearer aaa.bbb.ccc api_key=abc\n-----BEGIN PRIVATE KEY-----\nprivate\n-----END PRIVATE KEY-----";
        add(&mut store, "a", secrets, "carol")?;
        add(&mut store, "b", "ordinary evidence", "carol")?;
        add(&mut store, "private", "NEVER_TRANSMIT", "bob")?;
        let snapshot = store.retrieval_snapshot(&"carol".parse()?, AT)?;
        let judge = HttpJudge::new(JudgeConfig {
            tier,
            url: Some(url),
            allow_cloud_jev: tier == JudgeTier::CloudJev,
            ..JudgeConfig::default()
        })?;
        let result = judge
            .score(EvidenceBatch {
                query: secrets,
                candidates: snapshot.documents().iter().collect(),
            })
            .await?;
        assert_eq!(
            result,
            Some(if reranker {
                vec![0.2, 0.8]
            } else {
                vec![0.7, 0.7]
            })
        );
        let requests = requests.lock().map_err(|_| "lock")?;
        assert_eq!(requests.len(), if halve { 3 } else { 1 });
        for request in requests.iter() {
            let serialized = request.to_string();
            for secret in [
                "sk-123abc",
                "ghp_secret",
                "github_pat_abc",
                "AKIA1234567890123456",
                "xoxb-secret",
                "aaa.bbb.ccc",
                "api_key=abc",
                "BEGIN PRIVATE KEY",
                "NEVER_TRANSMIT",
            ] {
                assert!(!serialized.contains(secret), "unredacted {secret}");
            }
            assert!(serialized.contains("[REDACTED]"));
            if predicate {
                assert!(request["questions"].is_array());
                assert_eq!(request["questions"][0]["type"], "predicate");
                assert!(request["input"].is_string());
            } else if !reranker {
                assert!(request["questions"].is_object());
                let state: Value = serde_json::from_str(request["state"].as_str().ok_or("state")?)?;
                assert!(state["original_question"]
                    .as_str()
                    .ok_or("question")?
                    .contains("[REDACTED]"));
            }
        }
        server.abort();
    }
    Ok(())
}

#[tokio::test]
async fn opt_in_gate_fails_before_connect_and_local_endpoints_cannot_escape() -> TestResult {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    assert!(HttpJudge::new(JudgeConfig {
        tier: JudgeTier::CloudJev,
        url: Some(format!("http://{}/decisions", listener.local_addr()?)),
        ..JudgeConfig::default()
    })
    .is_err());
    assert!(
        tokio::time::timeout(Duration::from_millis(50), listener.accept())
            .await
            .is_err()
    );
    for url in [
        "https://example.com/decisions",
        "http://192.168.1.1/decisions",
        "http://127.0.0.1.example.com/decisions",
        "http://user:pass@127.0.0.1/decisions",
    ] {
        assert!(HttpJudge::new(JudgeConfig {
            tier: JudgeTier::LocalDecisionModel,
            url: Some(url.into()),
            ..JudgeConfig::default()
        })
        .is_err());
    }
    assert!(HttpJudge::new(JudgeConfig {
        tier: JudgeTier::LocalReranker,
        url: Some("https://example.com/rerank".into()),
        ..JudgeConfig::default()
    })
    .is_err());
    assert!(HttpJudge::new(JudgeConfig {
        tier: JudgeTier::LocalReranker,
        url: Some("http://192.168.1.1/v1/rerank".into()),
        ..JudgeConfig::default()
    })
    .is_ok());
    Ok(())
}

#[tokio::test]
async fn redirects_do_not_connect_to_another_endpoint() -> TestResult {
    let destination = TcpListener::bind("127.0.0.1:0").await?;
    let target = format!("http://{}/leak", destination.local_addr()?);
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}/decisions", listener.local_addr()?);
    let app = Router::new().route(
        "/decisions",
        post(move || {
            let target = target.clone();
            async move { axum::response::Redirect::temporary(&target) }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let judge = HttpJudge::new(JudgeConfig {
        tier: JudgeTier::LocalDecisionModel,
        url: Some(url),
        ..JudgeConfig::default()
    })?;
    let (status, _) = judge.request(&json!({"query":"test"})).await?;
    assert_eq!(status, 307);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), destination.accept())
            .await
            .is_err()
    );
    server.abort();
    Ok(())
}

#[test]
fn invalid_incomplete_refused_duplicate_and_out_of_range_answers_fail() {
    for data in [
        json!({"answers":{}}),
        json!({"answers":{"candidate_0":{"type":"choice","noul":0.2}}}),
        json!({"answers":{"candidate_0":{"type":"noul","noul":1.1}}}),
        json!({"answers":{"other":{"type":"noul","noul":0.2}}}),
    ] {
        assert!(decisions_scores(&data, 0..1, false).is_err());
    }
    let answer = json!({"type":"predicate","name":"candidate_0","probability":0.2});
    assert!(decisions_scores(&json!({"answers":[answer.clone(),answer]}), 0..2, true).is_err());
    assert!(decisions_scores(
        &json!({"answers":[{"type":"refusal","name":"candidate_0","probability":0.0}]}),
        0..1,
        true
    )
    .is_err());
    assert!(rerank_scores(&json!({"scores":[0.1]}), 2).is_err());
    assert!(rerank_scores(
        &json!({"results":[{"index":0,"relevance_score":0.2},{"index":0,"relevance_score":0.3}]}),
        2
    )
    .is_err());
}
