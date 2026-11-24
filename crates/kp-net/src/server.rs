#[cfg(all(unix, any(test, feature = "test-inproc-uds")))]
use std::collections::BTreeMap;
#[cfg(unix)]
use std::path::Path;
use std::path::PathBuf;
#[cfg(all(unix, any(test, feature = "test-inproc-uds")))]
use std::sync::OnceLock;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use kp_model::EpochMs;
use kp_store::Store;
use serde_json::{json, Value};

use crate::policy::Principal;
use crate::service::{dispatch, dispatch_readonly, Operation};
use crate::{NetError, Result};

struct ServerState {
    library_path: PathBuf,
    initial_embedder: Arc<dyn kp_store::Embedder + Send + Sync>,
    writer: Mutex<Store>,
}

impl ServerState {
    fn open_reader(&self) -> Result<Store> {
        let mut ro = Store::open_readonly(&self.library_path)?;
        let (persisted_model, _) = ro.embedding_config()?;
        if self.initial_embedder.model_id() == "custom"
            || self.initial_embedder.model_id() == persisted_model
        {
            ro.set_embedder(Arc::clone(&self.initial_embedder));
        }
        Ok(ro)
    }
}

type SharedStore = Arc<ServerState>;

#[cfg(all(unix, any(test, feature = "test-inproc-uds")))]
type UnixHttpRegistry =
    Mutex<BTreeMap<PathBuf, tokio::sync::mpsc::UnboundedSender<tokio::net::UnixStream>>>;

#[cfg(all(unix, any(test, feature = "test-inproc-uds")))]
fn unix_http_registry() -> &'static UnixHttpRegistry {
    static REGISTRY: OnceLock<UnixHttpRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(BTreeMap::new()))
}

#[cfg(unix)]
pub(crate) async fn connect_unix_http(path: &Path) -> Result<tokio::net::UnixStream> {
    let uid = crate::daemon::current_process_uid()?;
    crate::daemon::verify_socket_parent_dir_with_uid(path, uid)?;
    let stream = match tokio::net::UnixStream::connect(path).await {
        Ok(stream) => stream,
        #[cfg(any(test, feature = "test-inproc-uds"))]
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            let sender = unix_http_registry()
                .lock()
                .ok()
                .and_then(|guard| guard.get(path).cloned())
                .ok_or(NetError::Unavailable)?;
            let (client_stream, server_stream) =
                tokio::net::UnixStream::pair().map_err(|_| NetError::Unavailable)?;
            sender
                .send(server_stream)
                .map_err(|_| NetError::Unavailable)?;
            client_stream
        }
        Err(_) => return Err(NetError::Unavailable),
    };
    crate::daemon::verify_stream_peer_uid(&stream, uid)?;
    Ok(stream)
}

#[cfg(unix)]
struct VerifiedUnixListener {
    listener: tokio::net::UnixListener,
    expected_uid: u32,
    #[cfg(any(test, feature = "test-inproc-uds"))]
    pair_rx: tokio::sync::mpsc::UnboundedReceiver<tokio::net::UnixStream>,
}

#[cfg(unix)]
impl axum::serve::Listener for VerifiedUnixListener {
    type Io = tokio::net::UnixStream;
    type Addr = ();

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            #[cfg(any(test, feature = "test-inproc-uds"))]
            let candidate = tokio::select! {
                accepted = self.listener.accept() => {
                    match accepted {
                        Ok((stream, _addr)) => Some(stream),
                        Err(_) => {
                            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                            None
                        }
                    }
                }
                paired = self.pair_rx.recv() => {
                    match paired {
                        Some(stream) => Some(stream),
                        None => std::future::pending::<Option<tokio::net::UnixStream>>().await,
                    }
                }
            };

            #[cfg(not(any(test, feature = "test-inproc-uds")))]
            let candidate = match self.listener.accept().await {
                Ok((stream, _addr)) => Some(stream),
                Err(_) => {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    None
                }
            };

            if let Some(stream) = candidate {
                if crate::daemon::verify_stream_peer_uid(&stream, self.expected_uid).is_ok() {
                    return (stream, ());
                }
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        Ok(())
    }
}

#[cfg(unix)]
pub struct UnixHttpHandle {
    socket_path: PathBuf,
    task: tokio::task::JoinHandle<()>,
}

#[cfg(unix)]
impl UnixHttpHandle {
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub fn stop(&self) {
        self.task.abort();
        #[cfg(any(test, feature = "test-inproc-uds"))]
        if let Ok(mut guard) = unix_http_registry().lock() {
            guard.remove(&self.socket_path);
        }
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

#[cfg(unix)]
impl Drop for UnixHttpHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Serves `router` over a local Unix Domain Socket (`socket_path < 90` bytes, `0o600` socket inside a `0700` dir)
/// with server-side `peer_cred()` UID verification on every accepted stream.
#[cfg(unix)]
pub fn serve_unix(socket_path: impl Into<PathBuf>, router: Router) -> Result<UnixHttpHandle> {
    use std::os::unix::fs::PermissionsExt;
    let socket_path = socket_path.into();
    crate::daemon::validate_socket_path(&socket_path)?;
    let uid = crate::daemon::current_process_uid()?;
    crate::daemon::ensure_socket_parent_dir_with_uid(&socket_path, uid)?;
    let _ = std::fs::remove_file(&socket_path);
    let listener = tokio::net::UnixListener::bind(&socket_path)?;
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))?;

    #[cfg(any(test, feature = "test-inproc-uds"))]
    let task = {
        let (pair_tx, pair_rx) = tokio::sync::mpsc::unbounded_channel();
        if let Ok(mut guard) = unix_http_registry().lock() {
            guard.insert(socket_path.clone(), pair_tx);
        }
        let verified = VerifiedUnixListener {
            listener,
            expected_uid: uid,
            pair_rx,
        };
        tokio::spawn(async move {
            let _ = axum::serve(verified, router).await;
        })
    };

    #[cfg(not(any(test, feature = "test-inproc-uds")))]
    let task = {
        let verified = VerifiedUnixListener {
            listener,
            expected_uid: uid,
        };
        tokio::spawn(async move {
            let _ = axum::serve(verified, router).await;
        })
    };

    Ok(UnixHttpHandle { socket_path, task })
}

pub fn router(store: Store) -> Router {
    let state = Arc::new(ServerState {
        library_path: store.path().to_path_buf(),
        initial_embedder: store.embedder_arc(),
        writer: Mutex::new(store),
    });
    Router::new()
        .route("/v1/status", get(handle))
        .route("/v1/publish", post(handle))
        .route("/v1/scopes", post(handle))
        .route("/v1/withdraw", post(handle))
        .route("/v1/purge", post(handle))
        .route("/v1/search", get(handle))
        .route("/v1/why/{fact_id}", get(handle))
        .route("/v1/synthesize", post(handle))
        .route("/v1/changes", get(handle))
        .route("/v1/verify", post(handle))
        .route("/mcp", post(handle))
        .route("/a2a", post(handle))
        .layer(middleware::from_fn_with_state(state.clone(), authenticate))
        .route("/.well-known/agent.json", get(agent_card))
        .with_state(state)
}

async fn authenticate(
    State(store): State<SharedStore>,
    mut request: Request,
    next: Next,
) -> Result<Response> {
    if let Some(origin) = request.headers().get("origin") {
        let origin = origin
            .to_str()
            .map_err(|_| NetError::Forbidden)?
            .trim_end_matches('/');
        let host = request
            .headers()
            .get("host")
            .and_then(|h| h.to_str().ok())
            .ok_or(NetError::Forbidden)?;
        if origin != format!("http://{host}") && origin != format!("https://{host}") {
            return Err(NetError::Forbidden);
        }
    }
    let token = request
        .headers()
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .ok_or(NetError::Unauthorized)?
        .to_owned();
    let principal = tokio::task::spawn_blocking(move || {
        let ro = store.open_reader()?;
        let (actor, role) = ro.authenticate(&token)?.ok_or(NetError::Unauthorized)?;
        Ok::<_, NetError>(Principal { actor, role })
    })
    .await
    .map_err(|_| NetError::Unavailable)??;
    request.extensions_mut().insert(principal);
    Ok(next.run(request).await)
}

async fn agent_card() -> Json<Value> {
    Json(
        json!({"name":"Kipo Avatar","version":"0.1.0","url":"/a2a","capabilities":{"streaming":false},"authentication":{"schemes":["Bearer"]},"skills":[{"id":"search"},{"id":"why"},{"id":"propose_fact"}]}),
    )
}

async fn handle(
    State(store): State<SharedStore>,
    Extension(principal): Extension<Principal>,
    method: Method,
    uri: Uri,
    body: Bytes,
) -> Result<Response> {
    let runtime = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        let fast_rest_read =
            (method == Method::GET && uri.path() != "/v1/changes") || uri.path() == "/v1/verify";
        if fast_rest_read {
            let ro = store.open_reader()?;
            if !ro.derivation_view()?.members.contains(&principal.actor) {
                return Err(NetError::Unauthorized);
            }
            return runtime.block_on(async {
                let now = now()?;
                let parsed_body: Value = if body.is_empty() {
                    Value::Null
                } else {
                    serde_json::from_slice(&body)?
                };
                let operation = rest_operation(&method, &uri, parsed_body, now)?;
                dispatch_readonly(&ro, &principal, operation, now)
                    .await
                    .map(|value| Json(value).into_response())
            });
        }

        if uri.path() == "/mcp" || uri.path() == "/a2a" {
            let ro = store.open_reader()?;
            if !ro.derivation_view()?.members.contains(&principal.actor) {
                return Err(NetError::Unauthorized);
            }
            let parsed_body: Value = if body.is_empty() {
                Value::Null
            } else {
                serde_json::from_slice(&body)?
            };
            if uri.path() == "/mcp" && crate::protocol::is_readonly_mcp_body(&parsed_body) {
                return runtime.block_on(async {
                    let now = now()?;
                    crate::protocol::mcp_readonly(&ro, &principal, parsed_body, now).await
                });
            }
            if uri.path() == "/a2a" && crate::protocol::is_readonly_a2a_body(&parsed_body) {
                return runtime.block_on(async {
                    let now = now()?;
                    crate::protocol::a2a_readonly(&ro, &principal, parsed_body, now)
                        .await
                        .map(|value| Json(value).into_response())
                });
            }
            drop(ro);
        }

        let mut writer = store.writer.lock().map_err(|_| NetError::Unavailable)?;
        // A revocation queued after middleware must still win before the operation starts.
        let graph = writer.derivation_view()?;
        if !graph.members.contains(&principal.actor) {
            return Err(NetError::Unauthorized);
        }
        runtime.block_on(async {
            let now = now()?;
            let body: Value = if body.is_empty() {
                Value::Null
            } else {
                serde_json::from_slice(&body)?
            };
            if uri.path() == "/mcp" {
                return crate::protocol::mcp(&mut writer, &principal, body, now).await;
            }
            if uri.path() == "/a2a" {
                return crate::protocol::a2a(&mut writer, &principal, body, now)
                    .await
                    .map(|value| Json(value).into_response());
            }
            let operation = rest_operation(&method, &uri, body, now)?;
            dispatch(&mut writer, &principal, operation, now)
                .await
                .map(|value| Json(value).into_response())
        })
    })
    .await
    .map_err(|_| NetError::Unavailable)?
}

fn rest_operation(method: &Method, uri: &Uri, body: Value, now: EpochMs) -> Result<Operation> {
    let params: std::collections::BTreeMap<_, _> =
        reqwest::Url::parse(&format!("http://localhost{uri}"))
            .map_err(|_| NetError::Invalid("invalid URI"))?
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
    let at = params
        .get("at")
        .map(|v| {
            v.parse::<i64>()
                .map(EpochMs)
                .map_err(|_| NetError::Invalid("invalid time"))
        })
        .transpose()?
        .unwrap_or(now);
    match (method.as_str(), uri.path()) {
        ("GET", "/v1/status") => Ok(Operation::Status(at)),
        ("GET", "/v1/changes") => Ok(Operation::Changes {
            at,
            since: params.get("since").cloned(),
        }),
        ("GET", "/v1/search") => Ok(Operation::Search(crate::service::SearchRequest {
            q: params.get("q").cloned().unwrap_or_default(),
            at: Some(at),
            deep: params
                .get("deep")
                .map(|v| v.parse().map_err(|_| NetError::Invalid("invalid deep")))
                .transpose()?
                .unwrap_or(false),
            limit: params
                .get("limit")
                .map(|v| v.parse().map_err(|_| NetError::Invalid("invalid limit")))
                .transpose()?,
        })),
        ("POST", "/v1/publish") => Ok(Operation::Publish(Box::new(serde_json::from_value(body)?))),
        ("POST", "/v1/scopes") => Ok(Operation::UpsertScope(serde_json::from_value(body)?)),
        ("POST", "/v1/withdraw") => Ok(Operation::Withdraw(serde_json::from_value(body)?)),
        ("POST", "/v1/purge") => Ok(Operation::Purge(serde_json::from_value(body)?)),
        ("POST", "/v1/synthesize") => Ok(Operation::Synthesize(serde_json::from_value(body)?)),
        ("POST", "/v1/verify") => Ok(Operation::Verify(serde_json::from_value(body)?)),
        ("GET", path) => path
            .strip_prefix("/v1/why/")
            .map(|id| Operation::Why(id.into(), at))
            .ok_or(NetError::NotFound),
        _ => Err(NetError::NotFound),
    }
}

fn now() -> Result<EpochMs> {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| NetError::Unavailable)?
        .as_millis();
    Ok(EpochMs(
        i64::try_from(ms).map_err(|_| NetError::Unavailable)?,
    ))
}

pub(crate) fn public_error(error: &NetError) -> &'static str {
    match error {
        NetError::Unauthorized => "unauthorized",
        NetError::Forbidden
        | NetError::Policy(_)
        | NetError::Store(kp_store::StoreError::PermissionDenied) => "forbidden",
        NetError::NotFound
        | NetError::Store(kp_store::StoreError::NotFound | kp_store::StoreError::Tombstoned(_)) => {
            "not found"
        }
        NetError::Invalid(_) | NetError::Json(_) | NetError::Model(_) | NetError::Daemon(_) => {
            "invalid request"
        }
        NetError::Store(kp_store::StoreError::Conflict) => "conflict",
        NetError::Store(_)
        | NetError::Retrieve(_)
        | NetError::Io(_)
        | NetError::Transport(_)
        | NetError::Http(_)
        | NetError::Unavailable => "operation failed",
    }
}

impl IntoResponse for NetError {
    fn into_response(self) -> Response {
        let status = match &self {
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Forbidden
            | Self::Policy(_)
            | Self::Store(kp_store::StoreError::PermissionDenied) => StatusCode::FORBIDDEN,
            Self::NotFound
            | Self::Store(kp_store::StoreError::NotFound | kp_store::StoreError::Tombstoned(_)) => {
                StatusCode::NOT_FOUND
            }
            Self::Invalid(_) | Self::Json(_) | Self::Model(_) | Self::Daemon(_) => {
                StatusCode::BAD_REQUEST
            }
            Self::Store(kp_store::StoreError::Conflict) => StatusCode::CONFLICT,
            Self::Store(_)
            | Self::Retrieve(_)
            | Self::Io(_)
            | Self::Transport(_)
            | Self::Http(_)
            | Self::Unavailable => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, Json(json!({"error":public_error(&self)}))).into_response()
    }
}

#[cfg(test)]
#[path = "server_tests.rs"]
mod tests;
