use std::collections::{BTreeMap, BTreeSet};
#[cfg(unix)]
use std::path::PathBuf;
use std::time::Duration;

use kp_model::{
    AudienceSpec, ContentHash, EpochMs, FactAccess, FactStatus, LibraryId, LibraryMode,
    SecretRedactor,
};
use kp_store::Store;
use serde::Serialize;
use serde_json::{json, Value};

use crate::service::{redact_fact, CommunityChangeFeed};
use crate::{NetError, PublishRequest, Result};

enum ClientTransport {
    Http(reqwest::Client),
    #[cfg(unix)]
    Unix(PathBuf),
}

pub struct CommunityClient {
    url: reqwest::Url,
    token: String,
    transport: ClientTransport,
}

impl CommunityClient {
    pub fn new(url: &str, token: &str) -> Result<Self> {
        let url = parse_community_url(url)?;
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(NetError::Transport)?;
        Ok(Self {
            url,
            token: token.into(),
            transport: ClientTransport::Http(client),
        })
    }

    #[cfg(unix)]
    pub fn with_unix_socket(
        url: &str,
        token: &str,
        socket_path: impl Into<PathBuf>,
    ) -> Result<Self> {
        let url = parse_community_url(url)?;
        let socket_path = socket_path.into();
        crate::daemon::validate_socket_path(&socket_path)?;
        Ok(Self {
            url,
            token: token.into(),
            transport: ClientTransport::Unix(socket_path),
        })
    }

    pub fn url(&self) -> &str {
        self.url.as_str()
    }

    pub async fn get(&self, path: &str) -> Result<Value> {
        let (status, bytes) = self.get_raw(path).await?;
        decode_bytes(status, &bytes)
    }

    pub async fn post(&self, path: &str, body: &impl Serialize) -> Result<Value> {
        let payload = serde_json::to_vec(body)?;
        let (status, bytes) = self.post_raw_with_headers(path, &payload, &[]).await?;
        decode_bytes(status, &bytes)
    }

    pub async fn get_raw(&self, path: &str) -> Result<(u16, Vec<u8>)> {
        let joined = self
            .url
            .join(path)
            .map_err(|_| NetError::Invalid("invalid endpoint"))?;
        match &self.transport {
            ClientTransport::Http(client) => {
                let mut req = client.get(joined);
                if !self.token.is_empty() {
                    req = req.bearer_auth(&self.token);
                }
                let response = req.send().await.map_err(NetError::Transport)?;
                let status = response.status().as_u16();
                let bytes = response
                    .bytes()
                    .await
                    .map_err(NetError::Transport)?
                    .to_vec();
                Ok((status, bytes))
            }
            #[cfg(unix)]
            ClientTransport::Unix(socket_path) => {
                unix_http_request(socket_path, "GET", &joined, &self.token, None, &[]).await
            }
        }
    }

    pub async fn post_raw_with_headers(
        &self,
        path: &str,
        body: &[u8],
        extra_headers: &[(&str, &str)],
    ) -> Result<(u16, Vec<u8>)> {
        let joined = self
            .url
            .join(path)
            .map_err(|_| NetError::Invalid("invalid endpoint"))?;
        match &self.transport {
            ClientTransport::Http(client) => {
                let mut req = client
                    .post(joined)
                    .header("content-type", "application/json")
                    .body(body.to_vec());
                if !self.token.is_empty() {
                    req = req.bearer_auth(&self.token);
                }
                for &(k, v) in extra_headers {
                    req = req.header(k, v);
                }
                let response = req.send().await.map_err(NetError::Transport)?;
                let status = response.status().as_u16();
                let bytes = response
                    .bytes()
                    .await
                    .map_err(NetError::Transport)?
                    .to_vec();
                Ok((status, bytes))
            }
            #[cfg(unix)]
            ClientTransport::Unix(socket_path) => {
                unix_http_request(
                    socket_path,
                    "POST",
                    &joined,
                    &self.token,
                    Some(body),
                    extra_headers,
                )
                .await
            }
        }
    }
}

fn parse_community_url(url: &str) -> Result<reqwest::Url> {
    let url = reqwest::Url::parse(url).map_err(|_| NetError::Invalid("invalid community URL"))?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(NetError::Invalid("invalid community URL"));
    }
    Ok(url)
}

fn decode_bytes(status: u16, bytes: &[u8]) -> Result<Value> {
    if !(200..300).contains(&status) {
        return Err(NetError::Http(status));
    }
    if bytes.is_empty() {
        return Ok(Value::Null);
    }
    Ok(serde_json::from_slice(bytes)?)
}

#[cfg(unix)]
async fn unix_http_request(
    socket_path: &std::path::Path,
    method: &str,
    url: &reqwest::Url,
    token: &str,
    body: Option<&[u8]>,
    extra_headers: &[(&str, &str)],
) -> Result<(u16, Vec<u8>)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let target = match url.query() {
        Some(q) => format!("{}?{q}", url.path()),
        None => url.path().to_owned(),
    };
    let host = match url.port() {
        Some(port) => format!("{}:{port}", url.host_str().unwrap_or("localhost")),
        None => url.host_str().unwrap_or("localhost").to_owned(),
    };
    let mut head = format!("{method} {target} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
    if !token.is_empty() {
        head.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    if let Some(payload) = body {
        head.push_str("Content-Type: application/json\r\n");
        head.push_str(&format!("Content-Length: {}\r\n", payload.len()));
    }
    for &(k, v) in extra_headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");

    let mut stream = tokio::time::timeout(
        Duration::from_secs(10),
        crate::server::connect_unix_http(socket_path),
    )
    .await
    .map_err(|_| NetError::Unavailable)??;

    stream
        .write_all(head.as_bytes())
        .await
        .map_err(|_| NetError::Unavailable)?;
    if let Some(payload) = body {
        stream
            .write_all(payload)
            .await
            .map_err(|_| NetError::Unavailable)?;
    }
    stream.flush().await.map_err(|_| NetError::Unavailable)?;

    let mut raw = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut raw))
        .await
        .map_err(|_| NetError::Unavailable)?
        .map_err(|_| NetError::Unavailable)?;

    parse_http_response(&raw)
}

#[cfg(unix)]
fn parse_http_response(raw: &[u8]) -> Result<(u16, Vec<u8>)> {
    let sep = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or(NetError::Unavailable)?;
    let header_str = std::str::from_utf8(&raw[..sep]).map_err(|_| NetError::Unavailable)?;
    let mut lines = header_str.split("\r\n");
    let status_line = lines.next().ok_or(NetError::Unavailable)?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or(NetError::Unavailable)?;

    let mut chunked = false;
    let mut content_length: Option<usize> = None;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("transfer-encoding")
                && value.to_ascii_lowercase().contains("chunked")
            {
                chunked = true;
            } else if name.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse::<usize>().ok();
            }
        }
    }

    let body_bytes = &raw[sep + 4..];
    if chunked {
        Ok((status, decode_chunked_body(body_bytes)?))
    } else if let Some(len) = content_length {
        Ok((status, body_bytes[..len.min(body_bytes.len())].to_vec()))
    } else {
        Ok((status, body_bytes.to_vec()))
    }
}

#[cfg(unix)]
fn decode_chunked_body(mut rest: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    while let Some(crlf) = rest.windows(2).position(|w| w == b"\r\n") {
        let size_token = std::str::from_utf8(&rest[..crlf])
            .map_err(|_| NetError::Unavailable)?
            .split(';')
            .next()
            .unwrap_or("")
            .trim();
        let size = usize::from_str_radix(size_token, 16).map_err(|_| NetError::Unavailable)?;
        rest = &rest[crlf + 2..];
        if size == 0 {
            break;
        }
        if rest.len() < size + 2 {
            return Err(NetError::Unavailable);
        }
        out.extend_from_slice(&rest[..size]);
        rest = &rest[size + 2..];
    }
    Ok(out)
}

#[derive(Default)]
pub struct PublishOptions {
    pub audience: Option<AudienceSpec>,
    pub map_predicate: Option<String>,
}
#[derive(Serialize)]
pub struct PublishResult {
    pub queued: bool,
    pub idempotency_key: String,
}
#[derive(Default, Serialize)]
pub struct SyncResult {
    pub delivered: usize,
    pub pending: usize,
    pub mirrored: usize,
    pub unchanged: bool,
}

pub async fn publish(
    store: &mut Store,
    target: &str,
    client: &CommunityClient,
    options: &PublishOptions,
    at: EpochMs,
) -> Result<PublishResult> {
    publish_inner(store, target, client, options, false, at)
        .await?
        .ok_or(NetError::Invalid("publish candidate no longer eligible"))
}

async fn publish_inner(
    store: &mut Store,
    target: &str,
    client: &CommunityClient,
    options: &PublishOptions,
    require_multi_actor_or_prior_receipt: bool,
    at: EpochMs,
) -> Result<Option<PublishResult>> {
    let raw_fact = store.resolve_fact(target)?.ok_or(NetError::NotFound)?;
    let mut fact = raw_fact.clone();
    let redactor = SecretRedactor::new().map_err(|_| NetError::Unavailable)?;
    let mut origin_reference = fact.strong_ref();
    origin_reference.logical_key = redactor
        .redact(origin_reference.logical_key.as_str())
        .parse()?;
    let skill = fact.kind == kp_model::FactKind::SkillVersion;
    if skill {
        if !store
            .derivation_view()?
            .evaluate(&fact.author, at)
            .facts
            .contains(&fact.id)
        {
            return Err(NetError::NotFound);
        }
        let package: kp_model::SkillPackage = serde_json::from_value(fact.object.clone())?;
        package.validate()?;
    } else if !matches!(fact.access, FactAccess::Base { .. }) {
        return Err(NetError::Invalid(
            "publish requires a base claim or promoted skill",
        ));
    }
    if let Some(audience) = &options.audience {
        match &mut fact.access {
            FactAccess::Base {
                audience: current, ..
            } => *current = audience.clone(),
            FactAccess::Derived { .. } => {}
        }
    }
    if let Some(predicate) = &options.map_predicate {
        fact.predicate = predicate.clone();
    }
    let request_author = fact.author.clone();
    let skill_audience = skill.then(|| {
        options
            .audience
            .clone()
            .unwrap_or_else(|| AudienceSpec::Explicit(BTreeSet::from([request_author])))
    });
    let mut attached_scope = None;
    let mut scope_ceiling: Option<(kp_model::SourceScope, BTreeSet<kp_model::ActorId>)> = None;
    if let Some(scope_id) = &fact.source_scope_id {
        let requested_audience = if skill {
            skill_audience.as_ref().ok_or(NetError::Invalid(
                "skill publication requires an explicit audience",
            ))?
        } else {
            match &fact.access {
                FactAccess::Base { audience, .. } => audience,
                FactAccess::Derived { .. } => {
                    return Err(NetError::Invalid(
                        "publish requires a base claim or promoted skill",
                    ));
                }
            }
        };
        if let Some(mut local_scope) = store.source_scope(scope_id)? {
            let mode = store.status()?.mode;
            let graph = store.derivation_view()?;
            let ceiling_members: BTreeSet<_> = match (mode, &local_scope.current_audience) {
                (LibraryMode::Personal, AudienceSpec::Explicit(actors)) => {
                    graph.members.union(actors).cloned().collect()
                }
                _ => graph.members,
            };
            local_scope
                .validate_publish_audience(requested_audience, &ceiling_members)
                .map_err(|_| NetError::Forbidden)?;
            if let Some(ingester) = fact.ingested_by.as_ref().filter(|i| *i != &fact.author) {
                if !local_scope.allows(&fact.author, &ceiling_members)
                    || !local_scope.allows(ingester, &ceiling_members)
                {
                    return Err(NetError::Forbidden);
                }
            }
            local_scope.label = redactor.redact(&local_scope.label);
            scope_ceiling = Some((local_scope.clone(), ceiling_members));
            attached_scope = Some(local_scope);
        }
    }
    let concepts: Vec<_> = store
        .exchange_concepts_for_facts(std::slice::from_ref(&raw_fact))?
        .into_iter()
        .map(|mut c| {
            c.canonical_label = redactor.redact(&c.canonical_label);
            c.domain = redactor.redact(&c.domain);
            c
        })
        .collect();
    redact_fact(&mut fact, &redactor)?;
    let entities: BTreeSet<String> = store
        .entities()?
        .remove(&raw_fact.id)
        .unwrap_or_default()
        .iter()
        .map(|s| redactor.redact(s))
        .collect();
    let origin_library = fact.origin_library.clone();
    let logical_key = fact.logical_key.clone();
    let explicit_override = if skill {
        skill_audience.as_ref()
    } else {
        options.audience.as_ref()
    };
    let dest_url = client.url().to_owned();
    let origin_ref_for_check = origin_reference.clone();
    let Some((_, idempotency_key)) = (match store.enqueue_publish_atomic(
        &kp_store::PublishEnqueueSpec {
            destination: &dest_url,
            origin_library: &origin_library,
            logical_key: &logical_key,
            fact_id: &raw_fact.id,
            explicit_audience_override: explicit_override,
            require_multi_actor_or_prior_receipt,
        },
        |audience_revision, effective_audience, live_scope_opt| {
            if let Some((ref local_scope, ref ceiling_members)) = scope_ceiling {
                if live_scope_opt.is_none() {
                    local_scope
                        .validate_publish_audience(effective_audience, ceiling_members)
                        .map_err(|_| kp_store::StoreError::PermissionDenied)?;
                }
            }
            let mut scope_for_payload = live_scope_opt.cloned().or_else(|| attached_scope.clone());
            if let Some(ref mut sc) = scope_for_payload {
                sc.label = redactor.redact(&sc.label);
            }
            let mut tx_fact = fact.clone();
            if !skill {
                if let FactAccess::Base {
                    audience: ref mut aud_slot,
                    ..
                } = tx_fact.access
                {
                    *aud_slot = effective_audience.clone();
                }
            }
            let idem = ContentHash::digest(serde_json::to_vec(&(
                &dest_url,
                &tx_fact.author,
                tx_fact.strong_ref(),
                effective_audience,
                audience_revision,
                &scope_for_payload,
            ))?)
            .to_string();
            let request = PublishRequest {
                fact: tx_fact,
                entities: entities.clone(),
                idempotency_key: idem.clone(),
                origin_reference: origin_reference.clone(),
                audience_revision,
                skill_audience: skill_audience.clone(),
                concepts: concepts.clone(),
                source_scope: scope_for_payload,
            };
            let val = serde_json::to_value(&request)?;
            Ok((idem, val))
        },
    ) {
        Ok(opt) => opt,
        Err(kp_store::StoreError::PermissionDenied) => return Err(NetError::Forbidden),
        Err(err) => return Err(err.into()),
    }) else {
        return Ok(None);
    };
    flush(store, client, at, Some(&idempotency_key)).await?;
    let queued = store.pending_outbox(client.url())?.iter().any(|e| {
        e.key == idempotency_key
            || (e.kind == "publish"
                && e.payload
                    .get("origin_reference")
                    .and_then(|v| serde_json::from_value::<kp_model::StrongRef>(v.clone()).ok())
                    .as_ref()
                    == Some(&origin_ref_for_check))
    });
    Ok(Some(PublishResult {
        queued,
        idempotency_key,
    }))
}

async fn flush(
    store: &mut Store,
    client: &CommunityClient,
    at: EpochMs,
    immediate_publish_key: Option<&str>,
) -> Result<SyncResult> {
    let mut result = SyncResult::default();
    let mut failed_ids = BTreeSet::new();
    for pass in 0..2 {
        let mut delivered_publish_in_pass = false;
        for entry in store.pending_outbox(client.url())? {
            if pass > 0 && failed_ids.contains(&entry.id) {
                continue;
            }
            let send_payload = if entry.kind == "publish" {
                let Some(prepared) = store.prepare_outbox_publish(entry.id)? else {
                    delivered_publish_in_pass = true;
                    continue;
                };
                prepared
            } else {
                entry.payload.clone()
            };
            let current_key = send_payload
                .get("idempotency_key")
                .and_then(Value::as_str)
                .unwrap_or(entry.key.as_str())
                .to_owned();
            let is_immediate_target = entry.kind == "publish"
                && (immediate_publish_key == Some(entry.key.as_str())
                    || immediate_publish_key == Some(current_key.as_str()));
            let path = match entry.kind.as_str() {
                "publish" => "/v1/publish",
                "withdraw" => "/v1/withdraw",
                "purge" => "/v1/purge",
                _ => return Err(NetError::Invalid("unknown outbox event")),
            };
            match client.post(path, &send_payload).await {
                Ok(response) => {
                    if entry.kind == "publish" {
                        let request: PublishRequest = serde_json::from_value(send_payload)?;
                        let remote_id = response
                            .get("fact_id")
                            .and_then(Value::as_str)
                            .ok_or(NetError::Invalid("missing published ID"))?;
                        let effective_audience = if let Some(aud) = &request.skill_audience {
                            aud.clone()
                        } else {
                            match &request.fact.access {
                                FactAccess::Base { audience, .. } => audience.clone(),
                                FactAccess::Derived { .. } => AudienceSpec::AllMembers,
                            }
                        };
                        store.finalize_inflight_publish(&kp_store::FinalizePublishSpec {
                            outbox_id: entry.id,
                            destination: client.url(),
                            reference: &request.origin_reference,
                            remote_id,
                            audience: &effective_audience,
                            revision: request.audience_revision.max(1),
                            at,
                        })?;
                        delivered_publish_in_pass = true;
                    } else {
                        store.outbox_result(entry.id, Some(at))?;
                    }
                    result.delivered += 1;
                }
                Err(NetError::Http(404)) if entry.kind == "withdraw" => {
                    store.outbox_result(entry.id, Some(at))?;
                    result.delivered += 1;
                }
                Err(NetError::Http(code @ (400 | 403 | 404 | 409))) if is_immediate_target => {
                    store.exchange_drop_outbox(entry.id)?;
                    return Err(NetError::Http(code));
                }
                Err(NetError::Http(404)) if entry.kind == "publish" => {
                    store.exchange_drop_outbox(entry.id)?;
                }
                Err(err) => {
                    if entry.kind == "publish" {
                        let definitely_never_sent =
                            matches!(&err, NetError::Transport(req_err) if req_err.is_connect());
                        store.outbox_publish_attempt_failed(entry.id, definitely_never_sent)?;
                    } else {
                        store.outbox_result(entry.id, None)?;
                    }
                    failed_ids.insert(entry.id);
                    result.pending += 1;
                }
            }
        }
        if !delivered_publish_in_pass {
            break;
        }
    }
    Ok(result)
}

pub async fn sync(store: &mut Store, client: &CommunityClient, at: EpochMs) -> Result<SyncResult> {
    sync_inner(store, client, at, None).await
}

async fn verify_remote_actor(
    client: &CommunityClient,
    expected: &kp_model::ActorId,
    at: EpochMs,
) -> Result<()> {
    let status = client.get(&format!("/v1/status?at={at}")).await?;
    if status.get("actor").and_then(Value::as_str) != Some(expected.as_str()) {
        return Err(NetError::Forbidden);
    }
    Ok(())
}

async fn sync_inner(
    store: &mut Store,
    client: &CommunityClient,
    at: EpochMs,
    expected_actor: Option<&kp_model::ActorId>,
) -> Result<SyncResult> {
    if let Some(expected) = expected_actor {
        verify_remote_actor(client, expected, at).await?;
    }
    let mut result = flush(store, client, at, None).await?;
    let credential = ContentHash::digest(&client.token);
    let cursor = store.sync_cursor(client.url(), &credential)?;
    let since = cursor.as_ref().map(ContentHash::as_str).unwrap_or("0");
    let feed: CommunityChangeFeed = serde_json::from_value(
        client
            .get(&format!("/v1/changes?since={since}&at={at}"))
            .await?,
    )?;
    if let Some(expected) = expected_actor {
        if feed.authenticated_actor.as_ref() != Some(expected) {
            return Err(NetError::Forbidden);
        }
    }
    if !feed.members.is_empty() {
        let _ = store.register_peer_members(&feed.members, at)?;
    }
    result.unchanged = feed.snapshot.unchanged;
    if feed.snapshot.unchanged {
        if cursor.as_ref() != Some(&feed.snapshot.cursor) {
            return Err(NetError::Invalid("unrecognized unchanged cursor"));
        }
    } else {
        result.mirrored = feed.snapshot.facts.len();
        store.import_snapshot_with_governance(
            client.url(),
            &feed.snapshot,
            &feed.concepts,
            &feed.source_scopes,
            Some(&credential),
            at,
        )?;
    }
    Ok(result)
}

/// Automatically enqueues and publishes all active local multi-actor base facts not yet shared
/// with `client.url()` (never touching self-only facts), then pulls community updates via [`sync`].
pub async fn auto_publish_and_sync(
    store: &mut Store,
    client: &CommunityClient,
    at: EpochMs,
) -> Result<SyncResult> {
    let owner = store.owner_actor().clone();
    verify_remote_actor(client, &owner, at).await?;
    let graph = store.derivation_view()?;
    let alignment = kp_derive::ConceptAlignmentTable::from_graph(&graph);
    let _ = store.reconcile_relation_group_audiences(|l, r| alignment.entails(l, r), at)?;

    let mut delivered_from_auto = 0usize;
    for fact_id in store.unshared_multi_actor_facts(client.url())? {
        if let Ok(Some(pub_res)) = publish_inner(
            store,
            fact_id.as_str(),
            client,
            &PublishOptions::default(),
            true,
            at,
        )
        .await
        {
            if !pub_res.queued {
                delivered_from_auto += 1;
            }
        }
    }
    let mut res = sync_inner(store, client, at, Some(&owner)).await?;
    res.delivered += delivered_from_auto;
    Ok(res)
}

pub async fn reconcile_remote_origins(
    store: &mut Store,
    origins: &BTreeMap<LibraryId, CommunityClient>,
    at: EpochMs,
) -> Result<()> {
    let graph = store.derivation_view()?;
    let mut checked = BTreeSet::new();
    let references = store.origin_references()?;
    for fact in graph
        .facts
        .values()
        .filter(|f| matches!(f.status, FactStatus::Active | FactStatus::UnreachableHidden))
    {
        let reference = references
            .get(&fact.id)
            .cloned()
            .unwrap_or_else(|| fact.strong_ref());
        if let Some(client) = origins.get(&reference.origin_library) {
            if !checked.insert(fact.id.clone()) {
                continue;
            }
            // Only an explicit authenticated `200 {"confirmed": false}` from the origin marks
            // a mirror permanently invalidated; transient network/HTTP errors (including 401/403/404/5xx
            // and offline timeouts) map to `None` (`UnreachableHidden` only).
            let confirmed = match client.post("/v1/verify", &reference).await {
                Ok(value) => Some(
                    value
                        .get("confirmed")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                ),
                Err(NetError::Http(status @ (401 | 403))) => {
                    eprintln!(
                        "kp warning: origin `{}` rejected authentication (HTTP {status}); hiding mirror temporarily — please refresh your token",
                        reference.origin_library
                    );
                    None
                }
                Err(_) => None,
            };
            store.reconcile_origin_fact(&fact.id, confirmed, at)?;
        }
    }
    Ok(())
}

/// Queue explicit withdrawals without sending source transcripts or credentials to the outbox.
pub fn queue_withdraw(store: &mut Store, client: &CommunityClient, fact_id: &str) -> Result<()> {
    let key = ContentHash::digest(serde_json::to_vec(&(client.url(), "withdraw", fact_id))?);
    store.enqueue(
        client.url(),
        "withdraw",
        key.as_str(),
        &json!({"fact_id":fact_id}),
    )?;
    Ok(())
}
