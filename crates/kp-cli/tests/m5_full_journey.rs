mod support;

use std::collections::BTreeSet;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use axum::{extract::State, routing::post, Json, Router};
use kp_model::{DerivationMode, FactKind, FactStatus, SkillPackage, READ_ONLY_SKILL};
use kp_store::Store;
use lopdf::{content::Content, content::Operation, dictionary, Document, Object, Stream};
use reqwest::{Client, Method, StatusCode};
use serde_json::{json, Value};
use tokio::net::TcpListener;

use support::{run, Result};

const QUERY: &str = "周五还能自驾去杭州露营吗";
const SECRET: &str = "sk-proj-secret9999999999";
const CANARY: &str = "FINAL_M5_PURGE_CANARY_8842";
const SLUG: &str = "check-api-contract";
const TRIGGER: &str = "constructing a versioned API request";

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Community {
    _child: ChildGuard,
    url: String,
    http: Client,
}

impl Community {
    fn start(path: &Path) -> Result<Self> {
        let mut child = ChildGuard(
            Command::new(env!("CARGO_BIN_EXE_kp"))
                .arg("--path")
                .arg(path)
                .args(["serve", "--community", "--listen", "127.0.0.1:0"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()?,
        );
        let stderr = child.0.stderr.take().ok_or("community stderr")?;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let line = rx.recv_timeout(Duration::from_secs(30))??;
        let address = line.strip_prefix("kp listening on ").ok_or(line.clone())?;
        Ok(Self {
            _child: child,
            url: format!("http://{address}"),
            http: Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(20))
                .build()?,
        })
    }

    async fn request(
        &self,
        token: &str,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<(StatusCode, Vec<u8>)> {
        let mut request = self.http.request(method, format!("{}{path}", self.url));
        if !token.is_empty() {
            request = request.bearer_auth(token);
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().await?;
        Ok((response.status(), response.bytes().await?.to_vec()))
    }

    async fn json(&self, token: &str, path: &str, body: Option<&Value>) -> Result<Value> {
        let method = if body.is_some() {
            Method::POST
        } else {
            Method::GET
        };
        let (status, bytes) = self.request(token, method, path, body).await?;
        assert_eq!(
            status,
            StatusCode::OK,
            "{path}: {}",
            String::from_utf8_lossy(&bytes)
        );
        Ok(serde_json::from_slice(&bytes)?)
    }
}

struct Jev {
    task: tokio::task::JoinHandle<std::io::Result<()>>,
    requests: Arc<Mutex<Vec<Value>>>,
    url: String,
}

impl Drop for Jev {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Jev {
    async fn start() -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}/api/alpha/decisions", listener.local_addr()?);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let app = Router::new()
            .route("/api/alpha/decisions", post(jev_response))
            .with_state(requests.clone());
        let task = tokio::spawn(async move { axum::serve(listener, app).await });
        Ok(Self {
            task,
            requests,
            url,
        })
    }
}

async fn jev_response(
    State(requests): State<Arc<Mutex<Vec<Value>>>>,
    Json(payload): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let Ok(mut requests) = requests.lock() else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":"request log poisoned"})),
        );
    };
    requests.push(payload.clone());
    let Some(questions) = payload["questions"].as_object() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"SystemOne questions required"})),
        );
    };
    let answers: serde_json::Map<String, Value> = questions
        .iter()
        .map(|(id, question)| {
            let candidate = question["instructions"]["candidate"].to_string();
            let probability = if candidate.contains("身份证") {
                0.99
            } else if candidate.contains("改乘高铁") {
                0.96
            } else if candidate.contains("联邦证据") {
                0.90
            } else {
                0.02
            };
            (id.clone(), json!({"type":"noul","noul":probability}))
        })
        .collect();
    (StatusCode::OK, Json(json!({"answers":answers})))
}

struct Journey {
    // Child guards precede the TempDir so the server is reaped before its files disappear.
    community: Community,
    jev: Jev,
    _temp: tempfile::TempDir,
    alice: PathBuf,
    bob: PathBuf,
    q: PathBuf,
    workspace: PathBuf,
    alice_token: String,
    bob_token: String,
    carol_token: String,
}

impl Journey {
    async fn new() -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let alice = temp.path().join("alice_lib");
        let bob = temp.path().join("bob_lib");
        let q = temp.path().join("community_q");
        let workspace = temp.path().join("workspace");
        run(&alice, &["init", "--owner", "alice"])?;
        run(&bob, &["init", "--owner", "bob"])?;
        run(&bob, &["member", "add", "alice"])?;
        run(&q, &["init", "--community", "--owner", "admin"])?;
        let token = |actor| -> Result<String> {
            Ok(string(&run(&q, &["member", "add", actor])?["token"])?.to_owned())
        };
        let alice_token = token("alice")?;
        let bob_token = token("bob")?;
        let carol_token = token("carol")?;
        let community = Community::start(&q)?;
        let jev = Jev::start().await?;
        Ok(Self {
            community,
            jev,
            _temp: temp,
            alice,
            bob,
            q,
            workspace,
            alice_token,
            bob_token,
            carol_token,
        })
    }

    fn publish(&self, library: &Path, key: &str, token: &str, audience: &str) -> Result<Value> {
        let result = run(
            library,
            &[
                "publish",
                key,
                "--to",
                &self.community.url,
                "--token",
                token,
                "--audience",
                audience,
            ],
        )?;
        assert_eq!(
            result["queued"], false,
            "publication must reach the live community"
        );
        Ok(result)
    }

    fn sync(&self, library: &Path, token: &str) -> Result<Value> {
        let result = run(
            library,
            &["sync", "--from", &self.community.url, "--token", token],
        )?;
        assert_eq!(result["pending"], 0);
        Ok(result)
    }
}

fn string(value: &Value) -> Result<&str> {
    value
        .as_str()
        .ok_or_else(|| format!("expected string, got {value}").into())
}

fn array(value: &Value) -> Result<&Vec<Value>> {
    value
        .as_array()
        .ok_or_else(|| format!("expected array, got {value}").into())
}

fn path(path: &Path) -> Result<&str> {
    path.to_str().ok_or_else(|| "non-UTF8 fixture path".into())
}

fn fact_id(value: &Value) -> Result<String> {
    Ok(string(&value["facts"][0])?.to_owned())
}

fn piped_cli(library: &Path, args: &[&str], input: &[u8]) -> Result<Vec<u8>> {
    let mut child = ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_kp"))
            .arg("--path")
            .arg(library)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?,
    );
    child.0.stdin.take().ok_or("CLI stdin")?.write_all(input)?;
    let stdout = child.0.stdout.take().ok_or("CLI stdout")?;
    let stderr = child.0.stderr.take().ok_or("CLI stderr")?;
    let read = |mut reader: Box<dyn std::io::Read + Send>| {
        std::thread::spawn(move || -> std::io::Result<Vec<u8>> {
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes)?;
            Ok(bytes)
        })
    };
    let out = read(Box::new(stdout));
    let err = read(Box::new(stderr));
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.0.try_wait()? {
            break status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "CLI timed out: {args:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    let stdout = out.join().map_err(|_| "stdout reader panicked")??;
    let stderr = err.join().map_err(|_| "stderr reader panicked")??;
    assert!(
        status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&stderr)
    );
    assert!(stderr.is_empty(), "{}", String::from_utf8_lossy(&stderr));
    Ok(stdout)
}

fn hook(library: &Path, event: &str, payload: Value) -> Result<Value> {
    Ok(serde_json::from_slice(&piped_cli(
        library,
        &["hook", event],
        &serde_json::to_vec(&payload)?,
    )?)?)
}

fn mcp_tool(name: &str, args: Value) -> Value {
    json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":name,"arguments":args}})
}

fn stdio_search(library: &Path, query: &str) -> Result<Value> {
    let messages = [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"m5","version":"1"}}}),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
        mcp_tool("kp_search", json!({"query":query,"limit":100})),
    ];
    let input = messages
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let output = piped_cli(library, &["mcp"], input.as_bytes())?;
    let responses = String::from_utf8(output)?
        .lines()
        .map(serde_json::from_str)
        .collect::<serde_json::Result<Vec<Value>>>()?;
    assert_eq!(responses.len(), 3);
    assert_eq!(responses[0]["result"]["serverInfo"]["name"], "kp");
    let tools = array(&responses[1]["result"]["tools"])?;
    assert_eq!(tools.len(), 3);
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert_eq!(names, vec!["kp_search", "kp_note", "kp_why"]);
    let tools_json = serde_json::to_string(tools)?;
    let tools_tokens = kp_ingest::hooks::estimate_tokens(&tools_json);
    assert!(
        tools_json.len() <= 700 && tools_tokens <= 360,
        "tools/list bytes={} upper_bound_tokens={tools_tokens}",
        tools_json.len()
    );
    assert_eq!(responses[2]["result"]["isError"], false);
    Ok(responses[2]["result"]["structuredContent"].clone())
}

fn attribution(api: &str) -> Value {
    json!({"skill_slug":SLUG,"trigger":TRIGGER,"wrong_move":"Assume request fields are compatible across versions","verified_fix":"Compare request fields with the selected API contract before sending.","dimension":"DOM","cited_premise_ids":[api]})
}

fn synthetic_pdf(path: &Path) -> Result {
    let mut pdf = Document::with_version("1.5");
    let pages = pdf.new_object_id();
    let font =
        pdf.add_object(dictionary! {"Type"=>"Font","Subtype"=>"Type1","BaseFont"=>"Helvetica"});
    let resources = pdf.add_object(dictionary! {"Font"=>dictionary! {"F1"=>font}});
    let content = Content {
        operations: vec![
            Operation::new("BT", vec![]),
            Operation::new("Tf", vec!["F1".into(), 12.into()]),
            Operation::new("Tj", vec![Object::string_literal("Invoicepdfanchor")]),
            Operation::new("ET", vec![]),
        ],
    };
    let stream = pdf.add_object(Stream::new(dictionary! {}, content.encode()?));
    let page = pdf.add_object(dictionary! {"Type"=>"Page","Parent"=>pages,"Contents"=>stream,"Resources"=>resources,"MediaBox"=>vec![0.into(),0.into(),600.into(),800.into()]});
    pdf.objects.insert(
        pages,
        dictionary! {"Type"=>"Pages","Kids"=>vec![Object::Reference(page)],"Count"=>1}.into(),
    );
    let catalog = pdf.add_object(dictionary! {"Type"=>"Catalog","Pages"=>pages});
    pdf.trailer.set("Root", catalog);
    pdf.save(path)?;
    Ok(())
}

fn steps_1_2_hooks_and_funnel(j: &Journey) -> Result<String> {
    eprintln!("① Client hooks; ② multimodal L0/L1, self-echo and daytime capture");
    fs::create_dir_all(j.workspace.join("src"))?;
    fs::write(
        j.workspace.join("src/billing.rs"),
        "pub fn settle_invoice(cents: u64) -> Result<Receipt, Error> { todo!() }\n",
    )?;
    fs::write(j.workspace.join("notes.md"), "# Billing notes\nfact: kp.code.api_contract = Validate the invoice contract before settlement.\n")?;
    synthetic_pdf(&j.workspace.join("contract.pdf"))?;
    for client in ["claude-code", "codex"] {
        let result = run(
            &j.alice,
            &["connect", client, "--project", path(&j.workspace)?],
        )?;
        let settings = fs::read_to_string(string(&result["settings"])?)?;
        assert!(
            settings.contains("session-start")
                && settings.contains("pre-tool-use")
                && settings.contains("stop")
                && settings.contains("session-end")
        );
        if client == "claude-code" {
            assert!(settings.contains("\"async\": true"));
            assert!(settings.contains("mcp__kp__kp_note"));
        } else {
            let codex_toml = fs::read_to_string(j.workspace.join(".codex/config.toml"))?;
            assert!(codex_toml.contains("default_tools_approval_mode = \"approve\""));
            assert!(codex_toml.contains("\"kp_note\""));
        }
    }
    let added = run(&j.alice, &["add", path(&j.workspace)?, "--extract"])?;
    assert_eq!(array(&added)?.len(), 3);
    let search = run(
        &j.alice,
        &[
            "search",
            "Invoicepdfanchor settle_invoice",
            "--limit",
            "100",
        ],
    )?;
    let results = array(&search["results"])?;
    assert!(results.iter().any(|r| r["anchor"]["page"] == 1));
    assert!(results
        .iter()
        .any(|r| r["anchor"]["symbol"] == "settle_invoice"));
    let api = fact_id(&run(
        &j.alice,
        &[
            "remember",
            "--subject",
            "settle_invoice",
            "--predicate",
            "kp.code.api_contract",
            "--value",
            "Check the selected API contract before settlement",
            "--logical-key",
            "F_api",
            "--repo",
            path(&j.workspace)?,
            "--file",
            "src/billing.rs",
            "--symbol",
            "settle_invoice",
        ],
    )?)?;
    run(
        &j.alice,
        &[
            "dead-end",
            "--subject",
            "settle_invoice",
            "--hypothesis",
            "blind retry",
            "--reason",
            "duplicate settlement",
            "--repo",
            path(&j.workspace)?,
            "--file",
            "src/billing.rs",
            "--symbol",
            "settle_invoice",
        ],
    )?;
    let session = hook(
        &j.alice,
        "session-start",
        json!({"prompt":"settle_invoice"}),
    )?;
    assert_eq!(session, json!({}));
    let prompt_injected = hook(
        &j.alice,
        "user-prompt-submit",
        json!({"prompt":"settle_invoice"}),
    )?;
    let ctx_str = string(&prompt_injected["hookSpecificOutput"]["additionalContext"])?;
    assert!(ctx_str.contains("kp-ref:fact_"));
    let prompt_tokens = kp_ingest::hooks::estimate_tokens(ctx_str);
    assert!(prompt_tokens <= kp_ingest::hooks::MAX_PROMPT_TOKENS);
    eprintln!(
        "M5_TOKEN_OVERHEAD: session_start_tokens=0 user_prompt_submit_tokens={prompt_tokens}"
    );
    let advice = hook(
        &j.alice,
        "pre-tool-use",
        json!({"tool_name":"Edit","tool_input":{"file_path":j.workspace.join("src/billing.rs")}}),
    )?;
    assert_eq!(advice, json!({}));
    let stopped = hook(
        &j.alice,
        "stop",
        json!({"session_id":"invoice-task-1","messages":[
            {"role":"assistant","content":"[kp-ref:already-seen] ECHO_MUST_DISAPPEAR"},
            {"role":"assistant","content":format!("Failed invoice request 密钥{SECRET}"),"target":"settle_invoice","success":false,"fact_id":api},
            {"role":"assistant","content":"Verified the corrected invoice request","target":"settle_invoice","success":true,"attribution":attribution(&api)}
        ]}),
    )?;
    assert_eq!(stopped["self_echo_filtered"], true);
    assert_eq!(stopped["episodes"], 1);
    let ro_alice = Store::open_readonly(&j.alice)?;
    assert_eq!(ro_alice.pending_episodes(&"alice".parse()?)?.len(), 1);
    assert_eq!(
        ro_alice
            .derivation_view()?
            .facts
            .values()
            .filter(|f| matches!(f.kind, FactKind::PlaybookBullet | FactKind::SkillVersion))
            .count(),
        0
    );
    assert_eq!(kp_store::count_self_echo_filtered_sources(&j.alice)?, 1);
    assert_no_residue(&j.alice, SECRET)?;
    assert_no_residue(&j.alice, "ECHO_MUST_DISAPPEAR")?;
    assert!(
        kp_store::find_sqlite_table_containing_bytes(&j.alice, b"[REDACTED]")?.is_some(),
        "expected [REDACTED] in SQLite raw_blobs"
    );
    assert!(j.jev.requests.lock().map_err(|_| "Jev log")?.is_empty());
    assert!(stdio_search(&j.alice, "settle_invoice")?
        .to_string()
        .contains("settle_invoice"));
    Ok(api)
}

async fn step_3_retrieval(j: &Journey) -> Result {
    eprintln!("③ CJK multi-hop Jev decisions and federated probability ranking");
    let mut facts: Vec<_> = (0..40).map(|i| json!({"subject":format!("周五杭州露营资料{i}"),"predicate":"kp.core.note","value":"周五还能自驾去杭州露营吗：关键词资料，浓雾预警。","entities":["浓雾"]})).collect();
    facts.push(json!({"subject":"雾天交通","predicate":"kp.core.note","value":"浓雾期间禁止自驾出行，改乘高铁。密钥sk-proj-judge8888888888","entities":["浓雾","高铁"]}));
    facts.push(json!({"subject":"乘车证件","predicate":"kp.core.note","value":"高铁订票需要身份证。","entities":["高铁"]}));
    let evidence = j.workspace.join("retrieval.json");
    fs::write(&evidence, serde_json::to_vec(&facts)?)?;
    run(&j.alice, &["add", path(&evidence)?, "--extract"])?;
    run(
        &j.bob,
        &[
            "remember",
            "--subject",
            "杭州",
            "--predicate",
            "kp.core.note",
            "--value",
            "联邦证据：周五杭州出行备忘",
            "--audience",
            "alice,bob",
        ],
    )?;
    run(
        &j.bob,
        &[
            "remember",
            "--subject",
            "杭州",
            "--predicate",
            "kp.core.note",
            "--value",
            "BOB_PRIVATE_JUDGE_CANARY",
            "--audience",
            "bob",
        ],
    )?;
    // An unserved socket proves the opt-in gate closes before even establishing TCP.
    let denied_socket = TcpListener::bind("127.0.0.1:0").await?;
    let denied_url = format!("http://{}/api/alpha/decisions", denied_socket.local_addr()?);
    let denied = Command::new(env!("CARGO_BIN_EXE_kp"))
        .arg("--path")
        .arg(&j.alice)
        .args([
            "--json",
            "search",
            QUERY,
            "--deep",
            "--judge-tier",
            "cloud-jev",
            "--judge-url",
            &denied_url,
        ])
        .output()?;
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stderr).contains("--allow-cloud-jev"));
    assert!(
        tokio::time::timeout(Duration::from_millis(100), denied_socket.accept())
            .await
            .is_err()
    );
    let result = run(
        &j.alice,
        &[
            "search",
            QUERY,
            "--deep",
            "--judge-tier",
            "cloud-jev",
            "--judge-url",
            &j.jev.url,
            "--judge-model",
            "typesafe/jev-1.14",
            "--allow-cloud-jev",
            "--also-library",
            path(&j.bob)?,
            "--limit",
            "100",
        ],
    )?;
    let results = array(&result["results"])?;
    assert!(results.iter().all(|r| r["score"]
        .as_f64()
        .is_some_and(|s| (0.0..=1.0).contains(&s))));
    assert!(results
        .windows(2)
        .all(|pair| pair[0]["score"].as_f64() >= pair[1]["score"].as_f64()));
    let bridge = results
        .iter()
        .position(|r| r["heading"] == "雾天交通")
        .ok_or_else(|| format!("missing bridge evidence: {result}"))?;
    let distractor = results
        .iter()
        .position(|r| {
            r["heading"]
                .as_str()
                .is_some_and(|s| s.starts_with("周五杭州露营资料"))
                && r["hop"] == 0
                && r["score"] == 0.02
        })
        .ok_or("missing hop-0 distractor")?;
    assert!(bridge < distractor);
    assert!(
        results[bridge]["hop"].as_u64().is_some_and(|hop| hop > 0),
        "bridge was not discovered by a hop: {}",
        results[bridge]
    );
    assert_eq!(results[bridge]["score"], 0.96);
    assert!(results.iter().any(
        |r| r["passage"].as_str().is_some_and(|s| s.contains("身份证")) && r["score"] == 0.99
    ));
    let bob_id = Store::open(&j.bob)?.library_id().to_string();
    assert!(results
        .iter()
        .any(|r| r["library_id"] == bob_id && r["score"] == 0.90));
    assert!(!result.to_string().contains("BOB_PRIVATE_JUDGE_CANARY"));
    let requests = j.jev.requests.lock().map_err(|_| "Jev log")?;
    assert_eq!(
        requests.len(),
        2,
        "both libraries must be judged independently"
    );
    for request in requests.iter() {
        assert_eq!(
            request
                .as_object()
                .ok_or("request")?
                .keys()
                .map(String::as_str)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["model", "questions", "state"])
        );
        assert_eq!(request["model"], "typesafe/jev-1.14");
        let state: Value = serde_json::from_str(string(&request["state"])?)?;
        assert_eq!(state["original_question"], QUERY);
        let candidates = array(&state["previously_retrieved_evidence"])?;
        let questions = request["questions"].as_object().ok_or("noul questions")?;
        assert_eq!(questions.len(), candidates.len());
        for (i, candidate) in candidates.iter().enumerate() {
            let q = &questions[&format!("candidate_{i}")];
            assert_eq!(q["type"], "noul");
            assert_eq!(&q["instructions"]["candidate"], candidate);
            assert!(string(&q["instructions"]["question"])?.contains("original_question"));
            assert_eq!(
                q["criteria"],
                json!({"true":"Necessary evidence.","false":"Unnecessary evidence."})
            );
        }
        assert!(!request.to_string().contains("sk-proj-judge8888888888"));
        assert!(!request.to_string().contains("BOB_PRIVATE_JUDGE_CANARY"));
    }
    assert!(requests
        .iter()
        .any(|r| r.to_string().contains("[REDACTED]")));
    Ok(())
}

#[derive(Debug, PartialEq)]
struct Projection {
    search: Vec<u8>,
    changes: Vec<u8>,
    mcp: Vec<u8>,
    a2a: Vec<u8>,
}

async fn carol_projection(j: &Journey) -> Result<Projection> {
    let c = &j.community;
    let token = &j.carol_token;
    let get = |route| c.request(token, Method::GET, route, None);
    let (s, search) = get("/v1/search?q=hangzhou_rain&deep=true&limit=100").await?;
    assert_eq!(s, StatusCode::OK);
    let (s, changes) = get("/v1/changes").await?;
    assert_eq!(s, StatusCode::OK);
    let (s, mcp) = c
        .request(
            token,
            Method::POST,
            "/mcp",
            Some(&mcp_tool(
                "kp_search",
                json!({"q":"hangzhou_rain","deep":true,"limit":100}),
            )),
        )
        .await?;
    assert_eq!(s, StatusCode::OK);
    let (s, a2a) = c
        .request(
            token,
            Method::POST,
            "/a2a",
            Some(&json!({"task":"search","input":{"q":"hangzhou_rain","deep":true,"limit":100}})),
        )
        .await?;
    assert_eq!(s, StatusCode::OK);
    Ok(Projection {
        search,
        changes,
        mcp,
        a2a,
    })
}

struct Shared {
    event: String,
    constraint: String,
    collision: String,
    synthesis: String,
    baseline: Projection,
}

async fn steps_4_5_publish_and_synthesize(j: &Journey) -> Result<Shared> {
    eprintln!("④ Avatar publication; ⑤ automatic hard rules and AirGap synthesis");
    run(
        &j.alice,
        &[
            "remember",
            "--subject",
            "杭州团建",
            "--predicate",
            "kp.core.schedule",
            "--value",
            "周五杭州户外团建，气象台发布浓雾预警",
            "--kind",
            "event",
            "--tags",
            "hangzhou_rain,outdoor",
            "--logical-key",
            "friday-offsite",
        ],
    )?;
    j.publish(
        &j.alice,
        "friday-offsite",
        &j.alice_token,
        "alice,bob,carol",
    )?;
    let baseline = carol_projection(j).await?;
    assert!(String::from_utf8_lossy(&baseline.search).contains("浓雾预警"));
    let transcript = j.workspace.join("bob-private.md");
    fs::write(&transcript, "BOB_RAW_TRANSCRIPT_MUST_STAY_LOCAL\n")?;
    run(&j.bob, &["add", path(&transcript)?])?;
    assert!(
        kp_store::find_sqlite_table_containing_bytes(
            &j.bob,
            b"BOB_RAW_TRANSCRIPT_MUST_STAY_LOCAL",
        )?
        .is_some(),
        "expected BOB_RAW_TRANSCRIPT_MUST_STAY_LOCAL in SQLite raw_blobs"
    );
    run(
        &j.bob,
        &[
            "remember",
            "--subject",
            "杭州出行",
            "--predicate",
            "kp.core.preference",
            "--value",
            "浓雾天绝不自驾出行，必须改乘高铁",
            "--kind",
            "constraint",
            "--tags",
            "hangzhou_rain",
            "--logical-key",
            "rain-constraint",
        ],
    )?;
    j.publish(&j.bob, "rain-constraint", &j.bob_token, "alice,bob")?;
    assert_no_residue(&j.q, "BOB_RAW_TRANSCRIPT_MUST_STAY_LOCAL")?;
    assert_eq!(Store::open(&j.q)?.status()?.sources, 0);
    let graph = Store::open(&j.q)?.derivation_view()?;
    let event = graph
        .facts
        .values()
        .find(|f| f.kind == FactKind::Event)
        .ok_or("published event")?
        .id
        .to_string();
    let constraint = graph
        .facts
        .values()
        .find(|f| f.kind == FactKind::Constraint)
        .ok_or("published constraint")?
        .id
        .to_string();
    let collision = graph
        .facts
        .values()
        .find(|f| f.kind == FactKind::CollisionWarning)
        .ok_or("automatic collision")?
        .id
        .to_string();
    let proof = graph
        .justifications
        .values()
        .find(|p| p.conclusion.as_str() == collision)
        .ok_or("collision proof")?;
    assert_eq!(proof.rule_id, "R2_EVENT_CONSTRAINT_COLLISION");
    assert_eq!(
        proof
            .premises
            .iter()
            .map(|p| p.fact_id.as_str())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([event.as_str(), constraint.as_str()])
    );
    let result = j
        .community
        .json(
            &j.alice_token,
            "/v1/synthesize",
            Some(&json!({"query":"hangzhou_rain travel","target_audience":["alice","bob"]})),
        )
        .await?;
    let synthesis = fact_id(&result)?;
    let graph = Store::open(&j.q)?.derivation_view()?;
    let fact = graph
        .facts
        .get(&synthesis.parse()?)
        .ok_or("synthesis fact")?;
    assert_eq!(fact.kind, FactKind::Synthesis);
    assert!(!fact.unverified_gaps.is_empty());
    let proof = graph
        .justifications
        .values()
        .find(|p| p.conclusion == fact.id)
        .ok_or("synthesis audit")?;
    assert_eq!(proof.mode, DerivationMode::ScopedSynthesis);
    assert_eq!(
        proof
            .premises
            .iter()
            .map(|p| p.fact_id.as_str())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([event.as_str(), constraint.as_str(), collision.as_str()])
    );
    assert!(!fact.object.to_string().contains("BOB_RAW_TRANSCRIPT"));
    Ok(Shared {
        event,
        constraint,
        collision,
        synthesis,
        baseline,
    })
}

async fn step_6_semiring_isolation(j: &Journey, shared: &Shared) -> Result {
    eprintln!("⑥ Semiring proofs and byte-identical Carol projections on REST/MCP/A2A");
    for (library, token) in [(&j.alice, &j.alice_token), (&j.bob, &j.bob_token)] {
        j.sync(library, token)?;
        let result = run(
            library,
            &["search", "hangzhou_rain", "--deep", "--limit", "100"],
        )?;
        for id in [&shared.collision, &shared.synthesis] {
            assert!(array(&result["results"])?
                .iter()
                .any(|r| r["id"].as_str().is_some_and(|s| s.ends_with(id))));
        }
        let proof = run(library, &["why", &shared.collision])?;
        assert_eq!(proof["fact_id"], shared.collision);
        assert_eq!(
            proof["justifications"][0]["rule_id"],
            "R2_EVENT_CONSTRAINT_COLLISION"
        );
        let premises = array(&proof["justifications"][0]["premises"])?;
        assert_eq!(
            premises
                .iter()
                .map(|p| string(&p["fact_id"]))
                .collect::<Result<BTreeSet<_>>>()?,
            BTreeSet::from([shared.event.as_str(), shared.constraint.as_str()])
        );
        for premise in premises {
            assert!(array(&premise["justifications"])?.is_empty());
            assert!(premise["reference"]["content_sha256"].is_string());
            assert_eq!(premise["cycle"], false);
        }
        assert!(stdio_search(library, "hangzhou_rain")?
            .to_string()
            .contains(&shared.collision));
        let mcp = j
            .community
            .json(
                token,
                "/mcp",
                Some(&mcp_tool("kp_why", json!({"fact_id":shared.collision}))),
            )
            .await?;
        assert_eq!(
            mcp["result"]["structuredContent"]["fact_id"],
            shared.collision
        );
        let a2a = j
            .community
            .json(
                token,
                "/a2a",
                Some(&json!({"task":"why","input":{"fact_id":shared.collision}})),
            )
            .await?;
        assert_eq!(a2a["output"]["fact_id"], shared.collision);
    }
    let projection = carol_projection(j).await?;
    assert_eq!(projection, shared.baseline);
    let search: Value = serde_json::from_slice(&projection.search)?;
    assert!(array(&search["results"])?
        .iter()
        .any(|r| r["kind"] == "event"));
    assert!(array(&search["results"])?
        .iter()
        .all(|r| r["kind"] != "collision_warning" && r["kind"] != "synthesis"));
    let missing = j
        .community
        .request(
            &j.carol_token,
            Method::GET,
            "/v1/why/fact-nonexistent",
            None,
        )
        .await?;
    assert_eq!(missing.0, StatusCode::NOT_FOUND);
    let mcp_missing = j
        .community
        .json(
            &j.carol_token,
            "/mcp",
            Some(&mcp_tool("kp_why", json!({"fact_id":"fact-nonexistent"}))),
        )
        .await?;
    let a2a_missing = j
        .community
        .request(
            &j.carol_token,
            Method::POST,
            "/a2a",
            Some(&json!({"task":"why","input":{"fact_id":"fact-nonexistent"}})),
        )
        .await?;
    for hidden in [&shared.collision, &shared.synthesis] {
        assert_eq!(
            j.community
                .request(
                    &j.carol_token,
                    Method::GET,
                    &format!("/v1/why/{hidden}"),
                    None
                )
                .await?,
            missing
        );
        assert_eq!(
            j.community
                .json(
                    &j.carol_token,
                    "/mcp",
                    Some(&mcp_tool("kp_why", json!({"fact_id":hidden})))
                )
                .await?,
            mcp_missing
        );
        assert_eq!(
            j.community
                .request(
                    &j.carol_token,
                    Method::POST,
                    "/a2a",
                    Some(&json!({"task":"why","input":{"fact_id":hidden}}))
                )
                .await?,
            a2a_missing
        );
    }
    let card = j
        .community
        .json("", "/.well-known/agent.json", None)
        .await?;
    assert_eq!(card["authentication"]["schemes"], json!(["Bearer"]));
    assert!(!card.to_string().contains(&shared.collision));
    for route in ["/mcp", "/a2a"] {
        assert_eq!(
            j.community
                .request("", Method::POST, route, Some(&json!({})))
                .await?
                .0,
            StatusCode::UNAUTHORIZED
        );
    }
    Ok(())
}

async fn step_7_drift_and_withdrawal(j: &Journey, shared: &Shared) -> Result {
    eprintln!("⑦ Code drift, JTMS invalidation and synthesis quarantine");
    fs::write(
        j.workspace.join("src/billing.rs"),
        "pub fn settle_invoice(cents: u64, currency: &str) -> Result<Receipt, Error> { todo!() }\n",
    )?;
    let verified = run(&j.alice, &["verify", "--repo", path(&j.workspace)?])?;
    assert!(!array(&verified)?.is_empty());
    assert!(array(&verified)?
        .iter()
        .all(|a| a["verify_status"] == "sig_changed"));
    let graph = Store::open(&j.alice)?.derivation_view()?;
    assert!(graph
        .justifications
        .values()
        .any(|p| p.rule_id == "R4_ANCHOR_STALENESS"
            && p.active
            && graph.facts.get(&p.conclusion).is_some_and(|f| f.kind
                == FactKind::CollisionWarning
                && f.status == FactStatus::Active)));
    for id in [&shared.collision, &shared.synthesis] {
        assert_eq!(
            graph
                .facts
                .get(&id.parse()?)
                .ok_or("imported conclusion after verify")?
                .status,
            FactStatus::Active
        );
        assert_eq!(run(&j.alice, &["why", id])?["fact_id"], *id);
    }
    kp_derive::verify_hard_rules_match_full_recompute(&Store::open(&j.alice)?)?;
    run(&j.bob, &["forget", "rain-constraint"])?;
    j.sync(&j.bob, &j.bob_token)?;
    for library in [&j.q, &j.bob] {
        let graph = Store::open(library)?.derivation_view()?;
        assert_eq!(
            graph
                .facts
                .get(&shared.collision.parse()?)
                .ok_or("collision")?
                .status,
            FactStatus::Invalidated
        );
        assert_eq!(
            graph
                .facts
                .get(&shared.synthesis.parse()?)
                .ok_or("synthesis")?
                .status,
            FactStatus::Quarantined
        );
    }
    j.sync(&j.alice, &j.alice_token)?;
    assert_eq!(run(&j.alice, &["why", &shared.collision])?, Value::Null);
    assert_eq!(carol_projection(j).await?, shared.baseline);
    Ok(())
}

fn step_8_rsi(j: &Journey, api: &str) -> Result {
    eprintln!("⑧ Contrastive RSI dedup, promotion, imported approval and automatic rollback");
    let after = json!({"content":"Verified invoice contract","success":true,"target":"settle_invoice","attribution":attribution(api)}).to_string();
    let second = run(
        &j.alice,
        &[
            "episode",
            "record",
            "--kind",
            "failure-to-recovery",
            "--before",
            "Failed invoice contract check",
            "--after",
            &after,
            "--session",
            "invoice-task-2",
            "--step-ref",
            "tool:2",
            "--premise",
            api,
        ],
    )?;
    assert_eq!(second["episodes"], 1);
    let dreamed = run(&j.alice, &["dream"])?;
    assert_eq!(dreamed["processed"], 2);
    assert_eq!(dreamed["bullets_created"], 1);
    assert_eq!(array(&dreamed["promoted"])?.len(), 1);
    let ro_alice = Store::open_readonly(&j.alice)?;
    let graph = ro_alice.derivation_view()?;
    let bullets = graph
        .facts
        .values()
        .filter(|f| f.kind == FactKind::PlaybookBullet)
        .collect::<Vec<_>>();
    assert_eq!(bullets.len(), 1);
    let now_ms = kp_model::EpochMs(
        i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_millis(),
        )? + 1_000,
    );
    assert_eq!(
        ro_alice
            .feedback_counts(&bullets[0].id, &"alice".parse()?, now_ms)?
            .helped,
        2
    );
    let skills = run(&j.alice, &["skill", "list"])?;
    assert_eq!(array(&skills)?.len(), 1);
    assert_eq!(skills[0]["name"], SLUG);
    assert_eq!(skills[0]["exec_approved"], true);
    let skill_id = string(&skills[0]["fact_id"])?;
    let package: SkillPackage = serde_json::from_value(
        graph
            .facts
            .get(&skill_id.parse()?)
            .ok_or("skill fact")?
            .object
            .clone(),
    )?;
    package.validate()?;
    let alice_file = j.alice.join(format!("skills/{SLUG}/SKILL.md"));
    let skill_text = fs::read_to_string(&alice_file)?;
    package.validate_markdown(&skill_text, true)?;
    assert!(skill_text.starts_with("---\n"));
    assert!(skill_text.contains(&format!("When {TRIGGER}, load the `{SLUG}` skill.")));
    assert!(skill_text.contains("exec_approved: true"));
    assert!(!skill_text.contains(SECRET));
    j.publish(&j.alice, skill_id, &j.alice_token, "alice,bob")?;
    j.sync(&j.bob, &j.bob_token)?;
    let imported = run(&j.bob, &["skill", "list"])?;
    assert_eq!(array(&imported)?.len(), 1);
    assert_eq!(imported[0]["exec_approved"], false);
    assert_eq!(imported[0]["notice"], READ_ONLY_SKILL);
    let imported_id = string(&imported[0]["fact_id"])?;
    let bob_file = j.bob.join(format!("skills/{SLUG}/SKILL.md"));
    assert!(fs::read_to_string(&bob_file)?.contains("exec_approved: false"));
    assert_skill_search(&j.bob, imported_id, false)?;
    run(&j.bob, &["skill", "approve", SLUG])?;
    assert_eq!(run(&j.bob, &["skill", "list"])?[0]["exec_approved"], true);
    assert_skill_search(&j.bob, imported_id, true)?;
    j.sync(&j.bob, &j.bob_token)?;
    assert_eq!(run(&j.bob, &["skill", "list"])?[0]["exec_approved"], true);
    // Imported versions have no local verification history; establish two successful tasks
    // so rollback exercises the two-hurt threshold instead of hurt >= helped on the first.
    for task in ["helped-task-1", "helped-task-2"] {
        run(
            &j.bob,
            &["feedback", SLUG, "--outcome", "helped", "--task", task],
        )?;
    }
    for task in ["hurt-task-1", "hurt-task-2"] {
        let feedback = run(
            &j.bob,
            &["feedback", SLUG, "--outcome", "hurt", "--task", task],
        )?;
        assert_eq!(feedback["rolled_back"], task == "hurt-task-2");
    }
    assert!(!bob_file.exists());
    assert!(array(&run(&j.bob, &["skill", "list"])?)?.is_empty());
    assert_eq!(
        Store::open(&j.bob)?
            .derivation_view()?
            .facts
            .get(&imported_id.parse()?)
            .ok_or("rolled back imported skill")?
            .status,
        FactStatus::Invalidated
    );
    run(&j.alice, &["forget", api])?;
    assert!(!alice_file.exists());
    assert!(array(&run(&j.alice, &["skill", "list"])?)?.is_empty());
    let graph = Store::open(&j.alice)?.derivation_view()?;
    assert_eq!(
        graph
            .facts
            .get(&skill_id.parse()?)
            .ok_or("invalidated skill")?
            .status,
        FactStatus::Invalidated
    );
    assert_eq!(
        graph
            .facts
            .get(&bullets[0].id)
            .ok_or("invalidated bullet")?
            .status,
        FactStatus::Invalidated
    );
    j.sync(&j.alice, &j.alice_token)?;
    j.sync(&j.bob, &j.bob_token)?;
    assert!(!bob_file.exists());
    assert_eq!(run(&j.alice, &["dream"])?["processed"], 0);
    Ok(())
}

fn assert_skill_search(library: &Path, id: &str, approved: bool) -> Result {
    let result = run(library, &["search", SLUG, "--limit", "100"])?;
    let skill = array(&result["results"])?
        .iter()
        .find(|r| r["id"].as_str().is_some_and(|s| s.ends_with(id)))
        .ok_or("skill search result")?;
    let passage = string(&skill["passage"])?;
    assert!(passage.contains(&format!("\"exec_approved\":{approved}")));
    assert_eq!(passage.contains(READ_ONLY_SKILL), !approved);
    Ok(())
}

async fn step_9_purge(j: &Journey) -> Result {
    eprintln!("⑨ Zero-residue purge, packed Git erasure and cross-machine anti-resurrection");
    let note = j.workspace.join("purge.md");
    fs::write(
        &note,
        format!(
            "# Purge fixture\n```json\n{}\n```\n",
            json!({"subject":"purge-fixture","predicate":"kp.core.note","value":CANARY})
        ),
    )?;
    run(&j.alice, &["add", path(&note)?, "--extract"])?;
    let graph = Store::open(&j.alice)?.derivation_view()?;
    let canary = graph
        .facts
        .values()
        .find(|f| f.object == json!(CANARY))
        .ok_or("canary extracted fact")?;
    let published = j.publish(
        &j.alice,
        canary.id.as_str(),
        &j.alice_token,
        "alice,bob,carol",
    )?;
    j.sync(&j.bob, &j.bob_token)?;
    let payload = kp_store::outbox_payload_by_idempotency_key(
        &j.alice,
        string(&published["idempotency_key"])?,
    )?
    .ok_or("missing outbox payload")?;
    let replay: Value = serde_json::from_str(&payload)?;
    assert!(kp_store::count_chunks_fts_matches(&j.alice, CANARY)? > 0);
    assert!(payload.contains(CANARY));
    for library in [&j.alice, &j.q, &j.bob] {
        assert!(run(
            library,
            &["search", CANARY, "--as", "alice", "--limit", "100"]
        )?
        .to_string()
        .contains(CANARY));
    }
    let after = json!({"success":true,"target":"purge-fixture","attribution":{"skill_slug":"purge-fixture-skill","trigger":"validating a purge fixture","wrong_move":"Skip the fixture check","verified_fix":format!("Check {CANARY} before proceeding."),"dimension":"DOM","cited_premise_ids":[canary.id]}}).to_string();
    for session in ["purge-task-1", "purge-task-2"] {
        run(
            &j.alice,
            &[
                "episode",
                "record",
                "--kind",
                "failure-to-recovery",
                "--before",
                "Fixture validation failed",
                "--after",
                &after,
                "--premise",
                canary.id.as_str(),
                "--session",
                session,
                "--step-ref",
                "tool:1",
            ],
        )?;
    }
    assert_eq!(array(&run(&j.alice, &["dream"])?["promoted"])?.len(), 1);
    let skill_file = j.alice.join("skills/purge-fixture-skill/SKILL.md");
    assert!(fs::read_to_string(&skill_file)?.contains(CANARY));
    assert!(!j.alice.join("raw").exists());
    assert_eq!(
        kp_store::find_sqlite_table_containing_bytes(&j.alice, CANARY.as_bytes())?.as_deref(),
        Some("chunks")
    );
    run(&j.alice, &["purge", canary.logical_key.as_str()])?;
    assert!(!skill_file.exists());
    j.sync(&j.alice, &j.alice_token)?;
    j.sync(&j.bob, &j.bob_token)?;
    for library in [&j.alice, &j.q, &j.bob] {
        assert_no_residue(library, CANARY)?;
        assert_eq!(kp_store::count_chunks_fts_matches(library, CANARY)?, 0);
        assert_eq!(kp_store::count_orphaned_chunk_vectors(library)?, 0);
        assert!(Store::open(library)?.is_tombstoned(&canary.origin_library, &canary.logical_key)?);
        assert!(!run(
            library,
            &["search", CANARY, "--as", "alice", "--limit", "100"]
        )?
        .to_string()
        .contains(CANARY));
    }
    let rejected = j
        .community
        .request(&j.alice_token, Method::POST, "/v1/publish", Some(&replay))
        .await?;
    assert_eq!(rejected.0, StatusCode::NOT_FOUND);
    let local_replay = Command::new(env!("CARGO_BIN_EXE_kp"))
        .arg("--path")
        .arg(&j.alice)
        .args(["--json", "add", path(&note)?, "--extract"])
        .output()?;
    assert!(!local_replay.status.success());
    assert!(String::from_utf8_lossy(&local_replay.stderr).contains("tombstone"));
    j.sync(&j.alice, &j.alice_token)?;
    j.sync(&j.bob, &j.bob_token)?;
    for library in [&j.alice, &j.q, &j.bob] {
        assert_no_residue(library, CANARY)?;
    }
    assert!(
        run(&j.alice, &["search", "Invoicepdfanchor", "--limit", "100"])?
            .to_string()
            .contains("Invoicepdfanchor")
    );
    Ok(())
}

fn assert_no_residue(library: &Path, needle: &str) -> Result {
    fn absent(bytes: &[u8], needle: &str, location: &str) {
        assert!(
            !bytes.windows(needle.len()).any(|w| w == needle.as_bytes()),
            "residue in {location}"
        );
    }
    fn visit(root: &Path, needle: &str) -> Result {
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            let path = entry.path();
            absent(path.as_os_str().as_encoded_bytes(), needle, "file name");
            if entry.file_type()?.is_dir() {
                visit(&path, needle)?;
            } else {
                absent(&fs::read(&path)?, needle, &path.display().to_string());
            }
        }
        Ok(())
    }
    visit(library, needle)?;
    assert!(!library.join("raw").exists());
    if let Some(table) = kp_store::find_sqlite_table_containing_bytes(library, needle.as_bytes())? {
        panic!("found forbidden residue in SQLite table `{table}`");
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_nine_step_multi_avatar_journey() -> Result {
    let journey = Journey::new().await?;
    let api = steps_1_2_hooks_and_funnel(&journey)?;
    step_3_retrieval(&journey).await?;
    let shared = steps_4_5_publish_and_synthesize(&journey).await?;
    step_6_semiring_isolation(&journey, &shared).await?;
    step_7_drift_and_withdrawal(&journey, &shared).await?;
    step_8_rsi(&journey, &api)?;
    step_9_purge(&journey).await?;
    Ok(())
}
