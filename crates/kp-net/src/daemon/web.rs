//! Local `127.0.0.1` read-only 4-column Web Inspector hosted inside `kp daemon` (`M4`).
//!
//! Security & architectural invariants:
//! - Strictly locked to `127.0.0.1` (`Ipv4Addr::LOCALHOST`); non-loopback or IPv6 addresses are
//!   rejected prior to socket `bind` with [`NetError::Policy`].
//! - 256-bit unguessable capability tokens derived from the OS CSPRNG (cross-platform), mapped server-side to an
//!   immutable `ActorId`, verified in constant time, and exchanged via `GET /?token=<tok>` ->
//!   `303 See Other` (`HttpOnly; SameSite=Strict; Path=/` cookie) so tokens never linger in URLs.
//! - Zero query-parameter viewer switching (`?viewer=` is ignored); every API and asset request
//!   re-verifies active unrevoked library membership on the `Store` actor thread.
//! - Read-only `GET` / `HEAD` methods only (`405` on mutating verbs), strict `Origin` pinning
//!   (`http://127.0.0.1:<port>`), strict CSP (`default-src 'none'; script-src 'self'; style-src 'self'; ...`,
//!   zero inline script/style directives), and 100% `createElement` + `textContent` DOM rendering (zero XSS).

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;
#[cfg(any(test, feature = "test-inproc-uds"))]
use std::sync::atomic::{AtomicU16, Ordering};
#[cfg(any(test, feature = "test-inproc-uds"))]
use std::sync::OnceLock;
use std::sync::{mpsc as std_mpsc, Arc, Mutex as StdMutex, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::header::{
    ALLOW, CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, COOKIE, LOCATION, ORIGIN,
    REFERRER_POLICY, SET_COOKIE, X_CONTENT_TYPE_OPTIONS, X_FRAME_OPTIONS,
};
use axum::http::{HeaderValue, Method, StatusCode};
use axum::response::Response;
use axum::routing::any;
use axum::Router;
use kp_derive::why::ProofTree;
use kp_model::{ActorId, EpochMs, FactId, ReviewStatus};
use kp_store::Store;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot, watch, Mutex as TokioMutex};

use super::protocol::{DaemonEmbedderRuntimeStatus, DaemonHandler, WebInspectorSession};
use super::scheduler::{
    ActorCommand, IdleScheduleSignal, InspectorOverviewParams, InspectorQueryError,
};
use crate::{NetError, Result};

pub const INSPECTOR_COOKIE_NAME: &str = "kp_inspector_session";

pub const INSPECTOR_CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'none'";

const DEFAULT_OVERVIEW_LIMIT: usize = 100;
const MAX_OVERVIEW_LIMIT: usize = 500;

/// Enforces that a Web Inspector listener binds strictly to IPv4 loopback `127.0.0.1`.
pub fn validate_loopback_inspector_addr(bind_addr: SocketAddr) -> Result<()> {
    if bind_addr.ip() != IpAddr::V4(Ipv4Addr::LOCALHOST) {
        return Err(NetError::Policy(format!(
            "web inspector must bind strictly to 127.0.0.1, got {}",
            bind_addr.ip()
        )));
    }
    Ok(())
}

fn generate_256bit_hex_token() -> Result<String> {
    Ok(kp_store::random_256bit_hex_token())
}

fn constant_time_lookup_viewer(
    tokens: &RwLock<BTreeMap<String, ActorId>>,
    candidate: &str,
) -> Option<ActorId> {
    let trimmed = candidate.trim();
    if trimmed.len() != 64 || !trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let guard = tokens.read().ok()?;
    let cand_bytes = trimmed.as_bytes();
    let mut matched: Option<ActorId> = None;
    for (stored_tok, actor) in guard.iter() {
        let tok_bytes = stored_tok.as_bytes();
        if tok_bytes.len() == 64 {
            let mut diff = 0_u8;
            for (&a, &b) in tok_bytes.iter().zip(cand_bytes.iter()) {
                diff |= a ^ b;
            }
            if diff == 0 {
                matched = Some(actor.clone());
            }
        }
    }
    matched
}

fn issue_or_get_viewer_token(
    tokens: &RwLock<BTreeMap<String, ActorId>>,
    viewer: &ActorId,
) -> Result<String> {
    if let Ok(guard) = tokens.read() {
        for (tok, act) in guard.iter() {
            if act == viewer {
                return Ok(tok.clone());
            }
        }
    }
    let fresh = generate_256bit_hex_token()?;
    let mut guard = tokens
        .write()
        .map_err(|_| NetError::Daemon("web inspector token table poisoned".into()))?;
    for (tok, act) in guard.iter() {
        if act == viewer {
            return Ok(tok.clone());
        }
    }
    guard.insert(fresh.clone(), viewer.clone());
    Ok(fresh)
}

#[derive(Clone)]
pub(crate) struct WebInspectorHttpState {
    pub(crate) actual_port: u16,
    pub(crate) tokens: Arc<RwLock<BTreeMap<String, ActorId>>>,
    pub(crate) actor_tx: std_mpsc::Sender<ActorCommand>,
}

#[cfg(any(test, feature = "test-inproc-uds"))]
type InprocInspectorRegistry = StdMutex<BTreeMap<u16, Arc<WebInspectorHttpState>>>;

#[cfg(any(test, feature = "test-inproc-uds"))]
pub(crate) fn inproc_inspector_registry() -> &'static InprocInspectorRegistry {
    static REGISTRY: OnceLock<InprocInspectorRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| StdMutex::new(BTreeMap::new()))
}

#[cfg(any(test, feature = "test-inproc-uds"))]
fn allocate_virtual_inspector_port(requested_port: u16) -> Result<u16> {
    static NEXT_VPORT: AtomicU16 = AtomicU16::new(49_152);
    let guard = inproc_inspector_registry()
        .lock()
        .map_err(|_| NetError::Unavailable)?;
    if requested_port != 0 {
        if guard.contains_key(&requested_port) {
            return Err(NetError::Io(std::io::Error::new(
                std::io::ErrorKind::AddrInUse,
                "virtual inspector port already in use",
            )));
        }
        return Ok(requested_port);
    }
    for _ in 0..10_000 {
        let candidate = NEXT_VPORT.fetch_add(1, Ordering::Relaxed);
        let port = if candidate < 49_152 {
            49_152
        } else {
            candidate
        };
        if !guard.contains_key(&port) {
            return Ok(port);
        }
    }
    Err(NetError::Unavailable)
}

struct RunningInspectorSlot {
    bind_addr: SocketAddr,
    default_viewer: ActorId,
    default_url: String,
    http_state: Arc<WebInspectorHttpState>,
    task: Option<tokio::task::JoinHandle<()>>,
}

/// Manages the lifecycle and multi-viewer capability tokens of a daemon's `127.0.0.1` Web Inspector.
pub(super) struct WebInspectorManager {
    start_lock: TokioMutex<()>,
    slot: StdMutex<Option<RunningInspectorSlot>>,
    actor_tx: std_mpsc::Sender<ActorCommand>,
    sched_tx: mpsc::UnboundedSender<IdleScheduleSignal>,
    shutdown_rx: watch::Receiver<bool>,
}

impl WebInspectorManager {
    pub(super) fn new(
        actor_tx: std_mpsc::Sender<ActorCommand>,
        sched_tx: mpsc::UnboundedSender<IdleScheduleSignal>,
        shutdown_rx: watch::Receiver<bool>,
    ) -> Self {
        Self {
            start_lock: TokioMutex::new(()),
            slot: StdMutex::new(None),
            actor_tx,
            sched_tx,
            shutdown_rx,
        }
    }

    pub(super) fn web_inspector_url(&self) -> Option<String> {
        self.slot
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|s| s.default_url.clone()))
    }

    pub(super) fn web_inspector_addr(&self) -> Option<SocketAddr> {
        self.slot
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(|s| s.bind_addr))
    }

    pub(super) fn status_fields(&self) -> (Option<String>, Option<ActorId>) {
        let Ok(guard) = self.slot.lock() else {
            return (None, None);
        };
        match guard.as_ref() {
            Some(s) => (Some(s.default_url.clone()), Some(s.default_viewer.clone())),
            None => (None, None),
        }
    }

    pub(super) async fn ensure_started_for_viewer(
        &self,
        requested_addr: SocketAddr,
        viewer: ActorId,
    ) -> Result<WebInspectorSession> {
        validate_loopback_inspector_addr(requested_addr)?;
        let _start_guard = self.start_lock.lock().await;

        if let Ok(guard) = self.slot.lock() {
            if let Some(existing) = guard.as_ref() {
                let tok = issue_or_get_viewer_token(&existing.http_state.tokens, &viewer)?;
                let url = format!(
                    "http://127.0.0.1:{}/?token={tok}",
                    existing.bind_addr.port()
                );
                return Ok(WebInspectorSession {
                    url,
                    bind_addr: existing.bind_addr.to_string(),
                    viewer,
                });
            }
        }

        let tokens = Arc::new(RwLock::new(BTreeMap::new()));
        let initial_token = issue_or_get_viewer_token(&tokens, &viewer)?;

        let (actual_addr, http_state, task) =
            match tokio::net::TcpListener::bind(requested_addr).await {
                Ok(listener) => {
                    let actual_addr = listener.local_addr()?;
                    let http_state = Arc::new(WebInspectorHttpState {
                        actual_port: actual_addr.port(),
                        tokens,
                        actor_tx: self.actor_tx.clone(),
                    });
                    let router = build_web_inspector_router(Arc::clone(&http_state));
                    let mut stop_rx = self.shutdown_rx.clone();
                    let task = tokio::spawn(async move {
                        let _ = axum::serve(listener, router)
                            .with_graceful_shutdown(async move {
                                while !*stop_rx.borrow_and_update() {
                                    if stop_rx.changed().await.is_err() {
                                        break;
                                    }
                                }
                            })
                            .await;
                    });
                    (actual_addr, http_state, Some(task))
                }
                #[cfg(any(test, feature = "test-inproc-uds"))]
                Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
                    let vport = allocate_virtual_inspector_port(requested_addr.port())?;
                    let actual_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), vport);
                    let http_state = Arc::new(WebInspectorHttpState {
                        actual_port: vport,
                        tokens,
                        actor_tx: self.actor_tx.clone(),
                    });
                    if let Ok(mut reg) = inproc_inspector_registry().lock() {
                        reg.insert(vport, Arc::clone(&http_state));
                    }
                    (actual_addr, http_state, None)
                }
                Err(err) => return Err(NetError::Io(err)),
            };

        let _ = self.sched_tx.send(IdleScheduleSignal::WebInspectorActive);
        let default_url = format!(
            "http://127.0.0.1:{}/?token={initial_token}",
            actual_addr.port()
        );
        let session = WebInspectorSession {
            url: default_url.clone(),
            bind_addr: actual_addr.to_string(),
            viewer: viewer.clone(),
        };

        if let Ok(mut guard) = self.slot.lock() {
            *guard = Some(RunningInspectorSlot {
                bind_addr: actual_addr,
                default_viewer: viewer,
                default_url,
                http_state,
                task,
            });
        }

        Ok(session)
    }

    pub(super) async fn shutdown_and_join(&self) {
        let taken = self.slot.lock().ok().and_then(|mut g| g.take());
        if let Some(mut slot) = taken {
            #[cfg(any(test, feature = "test-inproc-uds"))]
            if let Ok(mut reg) = inproc_inspector_registry().lock() {
                reg.remove(&slot.bind_addr.port());
            }
            if let Some(task) = slot.task.take() {
                let _ = task.await;
            }
        }
    }

    pub(super) fn abort(&self) {
        let taken = self.slot.lock().ok().and_then(|mut g| g.take());
        if let Some(mut slot) = taken {
            #[cfg(any(test, feature = "test-inproc-uds"))]
            if let Ok(mut reg) = inproc_inspector_registry().lock() {
                reg.remove(&slot.bind_addr.port());
            }
            if let Some(task) = slot.task.take() {
                task.abort();
            }
        }
    }
}

pub(crate) fn build_web_inspector_router(state: Arc<WebInspectorHttpState>) -> Router {
    Router::new()
        .fallback(any(
            |State(st): State<Arc<WebInspectorHttpState>>, req: Request| async move {
                handle_web_inspector_request(st, req).await
            },
        ))
        .with_state(state)
}

pub(crate) async fn handle_web_inspector_request(
    state: Arc<WebInspectorHttpState>,
    req: Request,
) -> Response {
    let is_head = *req.method() == Method::HEAD;
    let mut response = dispatch_inspector_inner(&state, req).await;
    if is_head {
        let (parts, _) = response.into_parts();
        response = Response::from_parts(parts, Body::empty());
    }
    attach_security_headers(&mut response);
    response
}

fn attach_security_headers(resp: &mut Response) {
    let headers = resp.headers_mut();
    headers.insert(
        CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(INSPECTOR_CSP),
    );
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
}

fn plain_response(status: StatusCode, content_type: &'static str, body: &'static str) -> Response {
    let mut resp = Response::new(Body::from(body));
    *resp.status_mut() = status;
    resp.headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    resp
}

fn json_response(status: StatusCode, val: &Value) -> Response {
    let bytes =
        serde_json::to_vec(val).unwrap_or_else(|_| b"{\"error\":\"serialization\"}".to_vec());
    let mut resp = Response::new(Body::from(bytes));
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    resp
}

fn extract_candidate_tokens(req: &Request) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(bearer) = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        out.push(bearer.to_owned());
    }
    for hdr in req.headers().get_all(COOKIE) {
        let Ok(raw) = hdr.to_str() else {
            continue;
        };
        for pair in raw.split(';') {
            let trimmed = pair.trim();
            if let Some((name, val)) = trimmed.split_once('=') {
                if name.trim() == INSPECTOR_COOKIE_NAME {
                    let clean = val.trim();
                    if !clean.is_empty() {
                        out.push(clean.to_owned());
                    }
                }
            }
        }
    }
    out
}

async fn verify_active_viewer_on_actor(
    actor_tx: &std_mpsc::Sender<ActorCommand>,
    viewer: &ActorId,
) -> Option<bool> {
    let (reply_tx, reply_rx) = oneshot::channel();
    actor_tx
        .send(ActorCommand::InspectorVerifyMember {
            viewer: viewer.clone(),
            reply: reply_tx,
        })
        .ok()?;
    reply_rx.await.ok()
}

fn current_epoch_ms() -> EpochMs {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(0))
        .unwrap_or(0);
    EpochMs(ms)
}

fn decode_percent_segment(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut idx = 0_usize;
    while idx < bytes.len() {
        if bytes[idx] == b'%' {
            let hi = *bytes.get(idx + 1)?;
            let lo = *bytes.get(idx + 2)?;
            let hex_pair = [hi, lo];
            let hex_str = std::str::from_utf8(&hex_pair).ok()?;
            let decoded = u8::from_str_radix(hex_str, 16).ok()?;
            out.push(decoded);
            idx += 3;
        } else {
            out.push(bytes[idx]);
            idx += 1;
        }
    }
    String::from_utf8(out).ok()
}

async fn dispatch_inspector_inner(state: &WebInspectorHttpState, req: Request) -> Response {
    // 1. Strict `Origin` validation (`http://127.0.0.1:<actual_port>`).
    if let Some(origin_hdr) = req.headers().get(ORIGIN) {
        let Ok(origin_str) = origin_hdr.to_str() else {
            return json_response(StatusCode::FORBIDDEN, &json!({"error": "forbidden origin"}));
        };
        let expected_origin = format!("http://127.0.0.1:{}", state.actual_port);
        if origin_str.trim().trim_end_matches('/') != expected_origin {
            return json_response(StatusCode::FORBIDDEN, &json!({"error": "forbidden origin"}));
        }
    }

    // 2. Strictly read-only HTTP methods (`GET` and `HEAD` only; `405` for everything else).
    if !matches!(*req.method(), Method::GET | Method::HEAD) {
        let mut resp = json_response(
            StatusCode::METHOD_NOT_ALLOWED,
            &json!({"error": "method not allowed; web inspector is strictly read-only"}),
        );
        resp.headers_mut()
            .insert(ALLOW, HeaderValue::from_static("GET, HEAD"));
        return resp;
    }

    let uri = req.uri().clone();
    let Ok(parsed_url) =
        reqwest::Url::parse(&format!("http://127.0.0.1:{}{uri}", state.actual_port))
    else {
        return json_response(StatusCode::BAD_REQUEST, &json!({"error": "invalid URI"}));
    };
    let query_params: BTreeMap<String, String> = parsed_url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();

    let path = uri.path();

    // 3. Bootstrap `GET /?token=<tok>` -> `303 See Other` to `/` with `HttpOnly; SameSite=Strict` cookie.
    if path == "/" {
        if let Some(query_tok) = query_params.get("token") {
            let Some(viewer) = constant_time_lookup_viewer(&state.tokens, query_tok) else {
                return plain_response(
                    StatusCode::UNAUTHORIZED,
                    "text/plain; charset=utf-8",
                    "Unauthorized: invalid inspector token. Run `kp daemon status` or `kp inspect` to obtain your local access link.\n",
                );
            };
            match verify_active_viewer_on_actor(&state.actor_tx, &viewer).await {
                Some(true) => {}
                Some(false) => {
                    return plain_response(
                        StatusCode::FORBIDDEN,
                        "text/plain; charset=utf-8",
                        "Forbidden: member access has been revoked.\n",
                    );
                }
                None => {
                    return plain_response(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "text/plain; charset=utf-8",
                        "Inspector unavailable.\n",
                    );
                }
            }
            let cookie_val = format!(
                "{INSPECTOR_COOKIE_NAME}={}; HttpOnly; SameSite=Strict; Path=/",
                query_tok.trim()
            );
            let Ok(cookie_hdr) = HeaderValue::from_str(&cookie_val) else {
                return plain_response(
                    StatusCode::UNAUTHORIZED,
                    "text/plain; charset=utf-8",
                    "Unauthorized.\n",
                );
            };
            let redirect_loc = query_params
                .get("at")
                .map(String::as_str)
                .map(str::trim)
                .and_then(|s| s.parse::<i64>().ok())
                .and_then(|ms| HeaderValue::from_str(&format!("/?at={ms}")).ok())
                .unwrap_or_else(|| HeaderValue::from_static("/"));
            let mut resp = Response::new(Body::empty());
            *resp.status_mut() = StatusCode::SEE_OTHER;
            resp.headers_mut().insert(LOCATION, redirect_loc);
            resp.headers_mut().insert(SET_COOKIE, cookie_hdr);
            return resp;
        }
    }

    // 4. Authenticate session via `Authorization: Bearer <tok>` or `kp_inspector_session` Cookie.
    //    Any `?viewer=...` query parameter is ignored: `viewer` is 100% bound to the server token!
    let Some(viewer) = extract_candidate_tokens(&req)
        .iter()
        .find_map(|tok| constant_time_lookup_viewer(&state.tokens, tok))
    else {
        if path.starts_with("/api/") {
            return json_response(
                StatusCode::UNAUTHORIZED,
                &json!({"error": "unauthorized: run `kp daemon status` or `kp inspect` to obtain your local access link"}),
            );
        }
        return plain_response(
            StatusCode::UNAUTHORIZED,
            "text/plain; charset=utf-8",
            "Unauthorized: run `kp daemon status` or `kp inspect` to obtain your local 127.0.0.1 access link.\n",
        );
    };

    // 5. Route read-only endpoints.
    match path {
        "/" => match verify_active_viewer_on_actor(&state.actor_tx, &viewer).await {
            Some(true) => plain_response(
                StatusCode::OK,
                "text/html; charset=utf-8",
                INSPECTOR_INDEX_HTML,
            ),
            Some(false) => plain_response(
                StatusCode::FORBIDDEN,
                "text/plain; charset=utf-8",
                "Forbidden: member access has been revoked.\n",
            ),
            None => plain_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "text/plain; charset=utf-8",
                "Inspector unavailable.\n",
            ),
        },
        "/styles.css" => match verify_active_viewer_on_actor(&state.actor_tx, &viewer).await {
            Some(true) => plain_response(
                StatusCode::OK,
                "text/css; charset=utf-8",
                INSPECTOR_STYLES_CSS,
            ),
            Some(false) => plain_response(
                StatusCode::FORBIDDEN,
                "text/plain; charset=utf-8",
                "Forbidden.\n",
            ),
            None => plain_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "text/plain; charset=utf-8",
                "Unavailable.\n",
            ),
        },
        "/app.js" => match verify_active_viewer_on_actor(&state.actor_tx, &viewer).await {
            Some(true) => plain_response(
                StatusCode::OK,
                "application/javascript; charset=utf-8",
                INSPECTOR_APP_JS,
            ),
            Some(false) => plain_response(
                StatusCode::FORBIDDEN,
                "text/plain; charset=utf-8",
                "Forbidden.\n",
            ),
            None => plain_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "text/plain; charset=utf-8",
                "Unavailable.\n",
            ),
        },
        "/api/v1/overview" => {
            let eval_at = match query_params.get("at") {
                Some(raw) if !raw.trim().is_empty() => match raw.trim().parse::<i64>() {
                    Ok(ms) => EpochMs(ms),
                    Err(_) => {
                        return json_response(
                            StatusCode::BAD_REQUEST,
                            &json!({"error": "invalid `at` timestamp"}),
                        );
                    }
                },
                _ => current_epoch_ms(),
            };
            let limit = match query_params.get("limit") {
                Some(raw) if !raw.trim().is_empty() => match raw.trim().parse::<usize>() {
                    Ok(n) => n.clamp(1, MAX_OVERVIEW_LIMIT),
                    Err(_) => {
                        return json_response(
                            StatusCode::BAD_REQUEST,
                            &json!({"error": "invalid `limit`"}),
                        );
                    }
                },
                _ => DEFAULT_OVERVIEW_LIMIT,
            };
            let query = query_params
                .get("q")
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty());
            let target_filter = query_params
                .get("target")
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty());

            let (reply_tx, reply_rx) = oneshot::channel();
            if state
                .actor_tx
                .send(ActorCommand::InspectorOverview {
                    viewer,
                    params: InspectorOverviewParams {
                        query,
                        limit,
                        target_filter,
                        at: eval_at,
                    },
                    reply: reply_tx,
                })
                .is_err()
            {
                return json_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    &json!({"error": "daemon store actor stopped"}),
                );
            }
            match reply_rx.await {
                Ok(Ok(payload)) => json_response(StatusCode::OK, &payload),
                Ok(Err(InspectorQueryError::Forbidden)) => {
                    json_response(StatusCode::FORBIDDEN, &json!({"error": "forbidden"}))
                }
                Ok(Err(InspectorQueryError::NotFound)) => {
                    json_response(StatusCode::NOT_FOUND, &json!({"error": "not found"}))
                }
                Ok(Err(InspectorQueryError::Failed)) | Err(_) => json_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &json!({"error": "operation failed"}),
                ),
            }
        }
        other => {
            let Some(encoded_fact_id) = other.strip_prefix("/api/v1/why/") else {
                return json_response(StatusCode::NOT_FOUND, &json!({"error": "not found"}));
            };
            if encoded_fact_id.is_empty() {
                return json_response(StatusCode::NOT_FOUND, &json!({"error": "not found"}));
            }
            let Some(raw_fact_id) = decode_percent_segment(encoded_fact_id) else {
                return json_response(StatusCode::NOT_FOUND, &json!({"error": "not found"}));
            };
            let eval_at = match query_params.get("at") {
                Some(raw) if !raw.trim().is_empty() => match raw.trim().parse::<i64>() {
                    Ok(ms) => EpochMs(ms),
                    Err(_) => {
                        return json_response(
                            StatusCode::BAD_REQUEST,
                            &json!({"error": "invalid `at` timestamp"}),
                        );
                    }
                },
                _ => current_epoch_ms(),
            };

            let (reply_tx, reply_rx) = oneshot::channel();
            if state
                .actor_tx
                .send(ActorCommand::InspectorWhy {
                    viewer,
                    raw_fact_id,
                    at: eval_at,
                    reply: reply_tx,
                })
                .is_err()
            {
                return json_response(
                    StatusCode::SERVICE_UNAVAILABLE,
                    &json!({"error": "daemon store actor stopped"}),
                );
            }
            match reply_rx.await {
                Ok(Ok(payload)) => json_response(StatusCode::OK, &payload),
                Ok(Err(InspectorQueryError::Forbidden)) => {
                    json_response(StatusCode::FORBIDDEN, &json!({"error": "forbidden"}))
                }
                Ok(Err(InspectorQueryError::NotFound)) => {
                    json_response(StatusCode::NOT_FOUND, &json!({"error": "not found"}))
                }
                Ok(Err(InspectorQueryError::Failed)) | Err(_) => json_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &json!({"error": "operation failed"}),
                ),
            }
        }
    }
}

/// Resolves embedder runtime status consistently across UDS `Status` and Web Inspector `/api/v1/overview`.
pub(super) fn resolve_daemon_embedder_status(
    store: &Store,
    handler: &dyn DaemonHandler,
    meta_embedding_model: &str,
) -> DaemonEmbedderRuntimeStatus {
    let mut embedder = handler.embedder_status();
    if embedder.model.is_empty() {
        let runtime_model = store.embedder().model_id();
        embedder.model = if runtime_model.is_empty() || runtime_model == "custom" {
            meta_embedding_model.to_owned()
        } else {
            runtime_model.to_owned()
        };
    }
    if embedder.state.is_empty() {
        embedder.state = if embedder.model == "none" {
            "disabled".to_owned()
        } else if store.embedder().is_enabled() {
            "ready".to_owned()
        } else {
            "fts_fallback".to_owned()
        };
    }
    embedder
}

fn extract_fact_id_from_search_result(r: &kp_retrieve::SearchResult) -> Option<FactId> {
    if r.kind == "chunk" {
        return None;
    }
    let lib_prefix = format!("{}:", r.library_id);
    let after_lib = r.id.strip_prefix(&lib_prefix).unwrap_or(&r.id);
    let raw_fid = after_lib.strip_prefix("fact-").unwrap_or(after_lib);
    raw_fid.parse::<FactId>().ok()
}

pub(super) fn execute_inspector_overview(
    library_dir: &Path,
    store: &mut Store,
    handler: &dyn DaemonHandler,
    viewer: &ActorId,
    params: &InspectorOverviewParams,
) -> std::result::Result<Value, InspectorQueryError> {
    let eval_at = params.at;
    let limit = params.limit;
    let query = params.query.as_deref();
    let target_filter = params.target_filter.as_deref();
    handler.refresh_store_embedder(library_dir, store);
    match store.member_role(viewer) {
        Ok(Some(_)) => {}
        Ok(None) => return Err(InspectorQueryError::Forbidden),
        Err(_) => return Err(InspectorQueryError::Failed),
    }

    let status = store.status().map_err(|_| InspectorQueryError::Failed)?;
    let embedder = resolve_daemon_embedder_status(store, handler, &status.embedding_model);

    let (facts, search_results) = if let Some(q) = query.map(str::trim).filter(|s| !s.is_empty()) {
        let probe_limit = limit.saturating_mul(2).max(32);
        let search_out = kp_retrieve::search_shallow(store, viewer, eval_at, q, probe_limit)
            .map_err(|_| InspectorQueryError::Failed)?;
        let mut ordered_ids = Vec::with_capacity(search_out.results.len());
        for hit in &search_out.results {
            if let Some(fid) = extract_fact_id_from_search_result(hit) {
                ordered_ids.push(fid);
            }
        }
        let cards = store
            .inspector_visible_facts(viewer, eval_at, Some(&ordered_ids), limit)
            .map_err(map_store_err)?;
        let mut top_hits = search_out.results;
        top_hits.truncate(limit);
        (cards, top_hits)
    } else {
        let cards = store
            .inspector_visible_facts(viewer, eval_at, None, limit)
            .map_err(map_store_err)?;
        (cards, Vec::new())
    };

    let timeline = store
        .inspector_timeline(viewer, eval_at, limit, target_filter)
        .map_err(map_store_err)?;
    let reviews = store
        .list_review_items(viewer, Some(ReviewStatus::Open))
        .map_err(map_store_err)?;

    Ok(json!({
        "library_id": status.library_id,
        "name": status.name,
        "mode": status.mode,
        "viewer": viewer,
        "evaluated_at": eval_at,
        "embedder": embedder,
        "writeback_hint": {
            "read_only": true,
            "export_cmd": "kp export ./notes",
            "apply_cmd": "kp apply ./notes",
        },
        "facts": facts,
        "search_results": search_results,
        "timeline": timeline,
        "reviews": reviews,
    }))
}

fn collect_proof_chain_ids(
    tree: &ProofTree,
    seen: &mut BTreeSet<FactId>,
    ordered: &mut Vec<FactId>,
) {
    if seen.insert(tree.fact_id.clone()) {
        ordered.push(tree.fact_id.clone());
    }
    for branch in &tree.justifications {
        for premise in &branch.premises {
            collect_proof_chain_ids(premise, seen, ordered);
        }
    }
}

pub(super) fn execute_inspector_why(
    library_dir: &Path,
    store: &mut Store,
    handler: &dyn DaemonHandler,
    viewer: &ActorId,
    raw_fact_id: &str,
    eval_at: EpochMs,
) -> std::result::Result<Value, InspectorQueryError> {
    handler.refresh_store_embedder(library_dir, store);
    match store.member_role(viewer) {
        Ok(Some(_)) => {}
        Ok(None) => return Err(InspectorQueryError::Forbidden),
        Err(_) => return Err(InspectorQueryError::Failed),
    }

    let resolved_id = store
        .resolve_visible_or_raw_fact_id(viewer, eval_at, raw_fact_id)
        .map_err(|err| match err {
            kp_store::StoreError::Model(_) => InspectorQueryError::NotFound,
            other => map_store_err(other),
        })?;
    let Some(proof) =
        kp_derive::why::why(store, &resolved_id, viewer, eval_at).map_err(map_store_err)?
    else {
        return Err(InspectorQueryError::NotFound);
    };

    let mut seen = BTreeSet::new();
    let mut chain_ids = Vec::new();
    collect_proof_chain_ids(&proof, &mut seen, &mut chain_ids);

    let cards = store
        .inspector_visible_facts(viewer, eval_at, Some(&chain_ids), chain_ids.len().max(1))
        .map_err(map_store_err)?;
    let fact_card = cards.iter().find(|c| c.fact_id == proof.fact_id).cloned();
    let premise_facts: Vec<_> = cards
        .into_iter()
        .filter(|c| c.fact_id != proof.fact_id)
        .collect();
    let highlights = store
        .inspector_fact_highlights(viewer, eval_at, &chain_ids)
        .map_err(map_store_err)?;

    Ok(json!({
        "fact": fact_card,
        "proof": proof,
        "premise_facts": premise_facts,
        "highlights": highlights,
    }))
}

fn map_store_err(err: kp_store::StoreError) -> InspectorQueryError {
    match err {
        kp_store::StoreError::PermissionDenied => InspectorQueryError::Forbidden,
        kp_store::StoreError::NotFound | kp_store::StoreError::Tombstoned(_) => {
            InspectorQueryError::NotFound
        }
        _ => InspectorQueryError::Failed,
    }
}

const INSPECTOR_INDEX_HTML: &str = r#"<!DOCTYPE html>
<html lang="zh-CN">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Kipo Web Inspector · 本机只读记忆视图</title>
  <link rel="stylesheet" href="/styles.css">
  <script src="/app.js" defer></script>
</head>
<body>
  <header class="topbar">
    <div class="brand-block">
      <div class="brand-row">
        <h1 id="lib-title">Kipo Inspector</h1>
        <span id="lib-mode" class="pill pill-neutral">personal</span>
        <span id="viewer-badge" class="pill pill-accent">viewer</span>
        <span id="embedder-badge" class="pill pill-neutral">embedder</span>
      </div>
      <p class="readonly-banner">
        本机只读查看界面（127.0.0.1）· 如需批量编辑或撤回记忆，请使用无损写回动线：
        <code id="cmd-export">kp export ./notes</code> → 编辑 Markdown → <code id="cmd-apply">kp apply ./notes</code>
      </p>
    </div>
    <div class="search-block">
      <input id="search-input" type="search" placeholder="搜索记忆、规矩或关键词…" autocomplete="off">
      <button id="search-clear" type="button" class="btn-secondary">清空</button>
      <button id="refresh-btn" type="button" class="btn-primary">刷新</button>
    </div>
  </header>

  <div id="error-banner" class="error-banner hidden" role="alert"></div>

  <main class="grid-four">
    <section class="column" aria-labelledby="col1-heading">
      <div class="column-header">
        <h2 id="col1-heading">① Kipo 记住了什么</h2>
        <span id="facts-count" class="count-badge">0</span>
      </div>
      <div id="facts-list" class="column-body"></div>
    </section>

    <section class="column" aria-labelledby="col2-heading">
      <div class="column-header">
        <h2 id="col2-heading">② 为什么记住（原话高亮与依据链）</h2>
      </div>
      <div id="why-panel" class="column-body"></div>
    </section>

    <section class="column" aria-labelledby="col3-heading">
      <div class="column-header">
        <h2 id="col3-heading">③ 后来变了什么（演变时间线）</h2>
        <span id="timeline-count" class="count-badge">0</span>
      </div>
      <div id="timeline-list" class="column-body"></div>
    </section>

    <section class="column" aria-labelledby="col4-heading">
      <div class="column-header">
        <h2 id="col4-heading">④ 待确认列表（kp review）</h2>
        <span id="reviews-count" class="count-badge">0</span>
      </div>
      <div id="reviews-list" class="column-body"></div>
    </section>
  </main>
</body>
</html>
"#;

const INSPECTOR_STYLES_CSS: &str = r#":root {
  --bg: #f6f8fb;
  --surface: #ffffff;
  --surface-alt: #f1f5f9;
  --border: #e2e8f0;
  --border-active: #2563eb;
  --text: #0f172a;
  --muted: #475569;
  --accent: #2563eb;
  --accent-soft: #dbeafe;
  --mark-bg: #fef08a;
  --mark-text: #713f12;
  --cat-supersede: #0284c7;
  --cat-withdraw: #dc2626;
  --cat-expire: #d97706;
  --cat-stale: #9333ea;
  --cat-insert: #16a34a;
}

* {
  box-sizing: border-box;
}

body {
  margin: 0;
  font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, "PingFang SC", "Noto Sans CJK SC", sans-serif;
  background: var(--bg);
  color: var(--text);
  line-height: 1.5;
}

.topbar {
  display: flex;
  flex-wrap: wrap;
  justify-content: space-between;
  align-items: center;
  gap: 12px 20px;
  padding: 14px 20px;
  background: var(--surface);
  border-bottom: 1px solid var(--border);
}

.brand-row {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 8px;
}

.brand-row h1 {
  margin: 0;
  font-size: 18px;
  font-weight: 700;
}

.readonly-banner {
  margin: 4px 0 0;
  font-size: 12px;
  color: var(--muted);
}

code {
  font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
  font-size: 12px;
  background: var(--surface-alt);
  padding: 2px 6px;
  border-radius: 4px;
  border: 1px solid var(--border);
}

.search-block {
  display: flex;
  align-items: center;
  gap: 8px;
}

#search-input {
  width: 260px;
  padding: 7px 11px;
  border: 1px solid var(--border);
  border-radius: 6px;
  font-size: 13px;
}

button {
  cursor: pointer;
  border-radius: 6px;
  padding: 7px 12px;
  font-size: 13px;
  border: 1px solid var(--border);
}

.btn-primary {
  background: var(--accent);
  color: #ffffff;
  border-color: var(--accent);
}

.btn-secondary {
  background: var(--surface);
  color: var(--muted);
}

.pill {
  display: inline-block;
  padding: 2px 8px;
  border-radius: 999px;
  font-size: 11px;
  font-weight: 600;
}

.pill-neutral {
  background: var(--surface-alt);
  color: var(--muted);
}

.pill-accent {
  background: var(--accent-soft);
  color: var(--accent);
}

.error-banner {
  margin: 12px 20px 0;
  padding: 10px 14px;
  background: #fee2e2;
  color: #991b1b;
  border: 1px solid #fecaca;
  border-radius: 6px;
  font-size: 13px;
}

.hidden {
  display: none;
}

.grid-four {
  display: grid;
  grid-template-columns: repeat(4, minmax(260px, 1fr));
  gap: 14px;
  padding: 14px 20px 24px;
  min-height: calc(100vh - 88px);
}

@media (max-width: 1280px) {
  .grid-four {
    grid-template-columns: repeat(2, minmax(260px, 1fr));
  }
}

@media (max-width: 720px) {
  .grid-four {
    grid-template-columns: 1fr;
  }
}

.column {
  display: flex;
  flex-direction: column;
  background: var(--surface);
  border: 1px solid var(--border);
  border-radius: 10px;
  overflow: hidden;
  max-height: calc(100vh - 110px);
}

.column-header {
  display: flex;
  justify-content: space-between;
  align-items: center;
  padding: 11px 14px;
  border-bottom: 1px solid var(--border);
  background: var(--surface-alt);
}

.column-header h2 {
  margin: 0;
  font-size: 14px;
  font-weight: 600;
}

.count-badge {
  font-size: 12px;
  font-weight: 600;
  color: var(--muted);
  background: var(--surface);
  padding: 1px 8px;
  border-radius: 999px;
  border: 1px solid var(--border);
}

.column-body {
  padding: 12px;
  overflow-y: auto;
  display: flex;
  flex-direction: column;
  gap: 10px;
}

.card {
  padding: 10px 12px;
  border: 1px solid var(--border);
  border-radius: 8px;
  background: var(--surface);
}

.fact-card {
  cursor: pointer;
  transition: border-color 0.12s ease, box-shadow 0.12s ease;
}

.fact-card:hover {
  border-color: #93c5fd;
}

.fact-card-selected {
  border-color: var(--border-active);
  box-shadow: 0 0 0 2px var(--accent-soft);
}

.card-meta {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
  align-items: center;
  font-size: 11px;
  color: var(--muted);
  margin-bottom: 5px;
}

.card-title {
  font-size: 13px;
  font-weight: 600;
  margin-bottom: 4px;
}

.card-claim {
  font-size: 13px;
  white-space: pre-wrap;
  word-break: break-word;
}

.quote-box {
  padding: 10px 12px;
  border-left: 3px solid var(--accent);
  background: var(--surface-alt);
  border-radius: 4px;
  font-size: 13px;
  white-space: pre-wrap;
  word-break: break-word;
}

mark {
  background: var(--mark-bg);
  color: var(--mark-text);
  padding: 1px 3px;
  border-radius: 3px;
  font-weight: 600;
}

.proof-branch {
  margin-top: 8px;
  padding-left: 10px;
  border-left: 2px solid var(--border);
}

.cat-badge {
  padding: 1px 7px;
  border-radius: 4px;
  font-size: 11px;
  font-weight: 600;
  color: #ffffff;
}

.cat-supersede { background: var(--cat-supersede); }
.cat-withdraw { background: var(--cat-withdraw); }
.cat-expire { background: var(--cat-expire); }
.cat-stale_premise { background: var(--cat-stale); }
.cat-insert { background: var(--cat-insert); }
.cat-container, .cat-review { background: #64748b; }

.empty-hint {
  color: var(--muted);
  font-size: 13px;
  padding: 16px 6px;
  text-align: center;
}

.cli-hint {
  margin-top: 8px;
  display: block;
  overflow-x: auto;
}
"#;

const INSPECTOR_APP_JS: &str = r#"(() => {
  "use strict";

  let selectedFactId = null;
  let searchTimer = null;
  const pageAtParam = new URLSearchParams(window.location.search).get("at");
  const SAFE_TIMELINE_CATEGORIES = new Set([
    "supersede",
    "withdraw",
    "expire",
    "stale_premise",
    "insert",
    "container",
    "review",
  ]);

  function el(tag, className, text) {
    const node = document.createElement(tag);
    if (className) {
      node.className = className;
    }
    if (text !== undefined && text !== null) {
      node.textContent = String(text);
    }
    return node;
  }

  function formatTime(ms) {
    if (typeof ms !== "number" || !Number.isFinite(ms)) {
      return "";
    }
    try {
      return new Date(ms).toISOString().replace("T", " ").slice(0, 19) + "Z";
    } catch (_e) {
      return String(ms);
    }
  }

  function showError(msg) {
    const banner = document.getElementById("error-banner");
    if (!msg) {
      banner.textContent = "";
      banner.classList.add("hidden");
      return;
    }
    banner.textContent = msg;
    banner.classList.remove("hidden");
  }

  async function fetchJson(path) {
    const resp = await fetch(path, { credentials: "same-origin" });
    if (!resp.ok) {
      throw new Error("HTTP " + resp.status);
    }
    return resp.json();
  }

  function renderHighlightBox(hl) {
    const wrap = el("div", "card");
    const meta = el("div", "card-meta");
    meta.appendChild(el("span", "pill pill-neutral", hl.source_label || "source"));
    if (hl.source_uri) {
      meta.appendChild(el("code", "", hl.source_uri));
    }
    if (hl.char_span && typeof hl.char_span.char_start === "number") {
      meta.appendChild(
        el("span", "", "chars " + hl.char_span.char_start + ".." + hl.char_span.char_end)
      );
    }
    wrap.appendChild(meta);

    if (hl.excerpt_prefix || hl.excerpt_highlighted || hl.excerpt_suffix) {
      const qbox = el("div", "quote-box");
      if (hl.excerpt_prefix) {
        qbox.appendChild(el("span", "", hl.excerpt_prefix));
      }
      if (hl.excerpt_highlighted) {
        qbox.appendChild(el("mark", "", hl.excerpt_highlighted));
      }
      if (hl.excerpt_suffix) {
        qbox.appendChild(el("span", "", hl.excerpt_suffix));
      }
      wrap.appendChild(qbox);
    } else {
      wrap.appendChild(el("div", "empty-hint", "该条目无可读原文切片（仅保留受众内结论）"));
    }
    return wrap;
  }

  function renderProofTree(tree, cardsById, highlights) {
    const container = el("div", "proof-branch");
    const header = el("div", "card-meta");
    header.appendChild(el("code", "", tree.fact_id));
    if (Array.isArray(tree.unverified_gaps) && tree.unverified_gaps.length > 0) {
      header.appendChild(el("span", "pill pill-accent", "待验证前提: " + tree.unverified_gaps.join(", ")));
    }
    container.appendChild(header);

    const card = cardsById.get(tree.fact_id);
    if (card) {
      container.appendChild(el("div", "card-claim", card.claim));
    }
    if (highlights && highlights[tree.fact_id]) {
      container.appendChild(renderHighlightBox(highlights[tree.fact_id]));
    }

    if (Array.isArray(tree.justifications)) {
      for (const branch of tree.justifications) {
        const bnode = el("div", "proof-branch");
        bnode.appendChild(
          el("div", "card-meta", "推导规则: " + branch.rule_id + " (" + branch.mode + ")")
        );
        if (Array.isArray(branch.premises)) {
          for (const prem of branch.premises) {
            bnode.appendChild(renderProofTree(prem, cardsById, highlights));
          }
        }
        container.appendChild(bnode);
      }
    }
    return container;
  }

  async function selectFact(factId) {
    selectedFactId = factId;
    for (const node of document.querySelectorAll(".fact-card")) {
      if (node.dataset.factId === factId) {
        node.classList.add("fact-card-selected");
      } else {
        node.classList.remove("fact-card-selected");
      }
    }
    const panel = document.getElementById("why-panel");
    panel.replaceChildren(el("div", "empty-hint", "正在加载原话依据链…"));
    try {
      const whyUrl =
        "/api/v1/why/" +
        encodeURIComponent(factId) +
        (pageAtParam ? "?at=" + encodeURIComponent(pageAtParam) : "");
      const data = await fetchJson(whyUrl);
      if (selectedFactId !== factId) {
        return;
      }
      const nodes = [];
      if (data.fact) {
        const head = el("div", "card");
        head.appendChild(el("div", "card-title", data.fact.subject + " · " + data.fact.kind));
        head.appendChild(el("div", "card-claim", data.fact.claim));
        nodes.push(head);
      }
      const cardsById = new Map();
      if (data.fact) {
        cardsById.set(data.fact.fact_id, data.fact);
      }
      if (Array.isArray(data.premise_facts)) {
        for (const pf of data.premise_facts) {
          cardsById.set(pf.fact_id, pf);
        }
      }
      if (data.proof) {
        nodes.push(renderProofTree(data.proof, cardsById, data.highlights || {}));
      }
      panel.replaceChildren(...nodes);
    } catch (err) {
      if (selectedFactId !== factId) {
        return;
      }
      panel.replaceChildren(el("div", "empty-hint", "无法加载依据链: " + err.message));
    }
  }

  function renderFacts(facts) {
    const list = document.getElementById("facts-list");
    document.getElementById("facts-count").textContent = String(facts.length);
    if (facts.length === 0) {
      list.replaceChildren(el("div", "empty-hint", "当前查看者视角下暂无匹配记忆"));
      document.getElementById("why-panel").replaceChildren(
        el("div", "empty-hint", "点击左侧任意记忆查看原话高亮与推导依据链")
      );
      selectedFactId = null;
      return;
    }

    const items = facts.map((f) => {
      const card = el("div", "card fact-card");
      card.dataset.factId = f.fact_id;
      const meta = el("div", "card-meta");
      meta.appendChild(el("code", "", f.kp_ref));
      meta.appendChild(el("span", "pill pill-neutral", f.kind));
      if (f.is_derived) {
        meta.appendChild(el("span", "pill pill-accent", "派生结论"));
      }
      meta.appendChild(el("span", "", "@" + f.author));
      if (f.container_label || f.container_id) {
        meta.appendChild(el("span", "", "· " + (f.container_label || f.container_id)));
      }
      card.appendChild(meta);
      card.appendChild(el("div", "card-title", f.subject));
      card.appendChild(el("div", "card-claim", f.claim));
      card.addEventListener("click", () => {
        selectFact(f.fact_id);
      });
      return card;
    });
    list.replaceChildren(...items);

    const stillPresent = facts.some((f) => f.fact_id === selectedFactId);
    const targetId = stillPresent ? selectedFactId : facts[0].fact_id;
    selectFact(targetId);
  }

  function renderTimeline(entries) {
    const list = document.getElementById("timeline-list");
    document.getElementById("timeline-count").textContent = String(entries.length);
    if (entries.length === 0) {
      list.replaceChildren(el("div", "empty-hint", "暂无取代、撤回或过期演变记录"));
      return;
    }
    const nodes = entries.map((e) => {
      const card = el("div", "card");
      const meta = el("div", "card-meta");
      const rawCat = e.category || "insert";
      const safeCat = SAFE_TIMELINE_CATEGORIES.has(rawCat) ? rawCat : "insert";
      meta.appendChild(el("span", "cat-badge cat-" + safeCat, rawCat));
      meta.appendChild(el("span", "", formatTime(e.occurred_at)));
      if (e.actor_id) {
        meta.appendChild(el("span", "", "@" + e.actor_id));
      }
      card.appendChild(meta);
      card.appendChild(el("code", "", e.target_id));
      card.appendChild(el("div", "card-claim", e.summary));
      return card;
    });
    list.replaceChildren(...nodes);
  }

  function renderReviews(reviews) {
    const list = document.getElementById("reviews-list");
    document.getElementById("reviews-count").textContent = String(reviews.length);
    if (reviews.length === 0) {
      list.replaceChildren(el("div", "empty-hint", "无待确认项"));
      return;
    }
    const nodes = reviews.map((r) => {
      const card = el("div", "card");
      const meta = el("div", "card-meta");
      meta.appendChild(el("code", "", r.id));
      meta.appendChild(el("span", "pill pill-accent", r.kind));
      card.appendChild(meta);
      card.appendChild(el("div", "card-claim", r.question));
      if (Array.isArray(r.options) && r.options.length > 0) {
        card.appendChild(el("div", "card-meta", "可选项: " + r.options.join(" / ")));
      }
      card.appendChild(
        el("code", "cli-hint", "kp review resolve " + r.id + " confirm")
      );
      return card;
    });
    list.replaceChildren(...nodes);
  }

  async function loadOverview() {
    showError("");
    const q = document.getElementById("search-input").value.trim();
    const qs = new URLSearchParams();
    if (q) {
      qs.set("q", q);
    }
    if (pageAtParam) {
      qs.set("at", pageAtParam);
    }
    const qsStr = qs.toString();
    const url = qsStr ? "/api/v1/overview?" + qsStr : "/api/v1/overview";
    try {
      const data = await fetchJson(url);
      document.getElementById("lib-title").textContent =
        (data.name || "Kipo") + " (" + data.library_id + ")";
      document.getElementById("lib-mode").textContent = data.mode || "personal";
      document.getElementById("viewer-badge").textContent = "viewer: " + data.viewer;
      if (data.embedder) {
        let embText = "embedder: " + data.embedder.model + " (" + data.embedder.state;
        if (typeof data.embedder.download_pct === "number") {
          embText += " " + data.embedder.download_pct + "%";
        }
        embText += ")";
        document.getElementById("embedder-badge").textContent = embText;
      }
      if (data.writeback_hint) {
        document.getElementById("cmd-export").textContent = data.writeback_hint.export_cmd;
        document.getElementById("cmd-apply").textContent = data.writeback_hint.apply_cmd;
      }
      renderFacts(Array.isArray(data.facts) ? data.facts : []);
      renderTimeline(Array.isArray(data.timeline) ? data.timeline : []);
      renderReviews(Array.isArray(data.reviews) ? data.reviews : []);
    } catch (err) {
      showError("读取本机查看器数据失败: " + err.message);
    }
  }

  document.addEventListener("DOMContentLoaded", () => {
    const input = document.getElementById("search-input");
    input.addEventListener("input", () => {
      if (searchTimer) {
        clearTimeout(searchTimer);
      }
      searchTimer = setTimeout(loadOverview, 180);
    });
    document.getElementById("search-clear").addEventListener("click", () => {
      input.value = "";
      loadOverview();
    });
    document.getElementById("refresh-btn").addEventListener("click", () => {
      loadOverview();
    });
    loadOverview();
  });
})();
"#;
