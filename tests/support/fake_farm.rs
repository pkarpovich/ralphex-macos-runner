//! An in-process stand-in for the farm's runner API and its token endpoint.
//!
//! A test scripts each route with a queue of [`Reply`] values, or with a sticky
//! reply the route falls back to once the queue is empty, and reads back every
//! request the client made. The token endpoint at [`TOKEN_PATH`] checks the
//! form and the client assertion the way the authorization server does - the
//! signature against the key registered for its `kid`, `iss` and `sub` equal to
//! the client id, `aud` equal to the farm's URL as the issuer, a `jti` it has
//! not seen and an `exp` sixty seconds after `iat` - and mints a token it then
//! accepts on every runner route; any other bearer is answered `401`. A run
//! belongs to the client whose claim or opening handed it out, and a log,
//! heartbeat, progress or completion from another client is answered `410`.
//! The log route answers the way the real one does -
//! `204` for a chunk it took and for a chunk whose sequence number it has
//! already seen, `400` only for a number below one - and the run route runs the
//! same version check, so a client that repeats a sequence number or speaks
//! another protocol version meets the same answer here as at the farm. The
//! progress route answers `204` unless a test scripts it otherwise.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use aws_lc_rs::signature::{RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};
use axum::body::Bytes;
use axum::extract::{Form, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ralphex_macos_runner::auth::{ASSERTION_TYPE, GRANT_TYPE, SCOPE};
use ralphex_macos_runner::protocol::types::{
    Branch, CreatePr, HeartbeatAction, HeartbeatResponse, Job, RunId, VERSION,
};
use serde_json::Value;
use tokio::sync::Notify;

/// What the fake farm answers to one request.
#[derive(Debug, Clone)]
pub enum Reply {
    /// `200` with a job.
    Job(Box<Job>),
    /// `204`, the answer to a claim whose window expired.
    NoJob,
    /// `200` with an empty body.
    Accepted,
    /// `200` with a heartbeat answer.
    Beat(HeartbeatAction),
    /// An arbitrary status and body.
    Status(u16, String),
    /// Hold the request open until the test releases it.
    Hold,
}

/// The path of the fake token endpoint under the farm's URL.
pub const TOKEN_PATH: &str = "/api/oidc/token";

/// The lifetime of a token the fake endpoint mints unless a test scripts another.
pub const TOKEN_LIFETIME: u64 = 3600;

/// What the fake token endpoint answers to one valid token request.
#[derive(Debug, Clone)]
pub enum Grant {
    /// `200` with a fresh token that expires after this many seconds.
    Token(u64),
    /// An arbitrary status and body, with no token minted.
    Status(u16, String),
}

/// One token request the fake endpoint received.
#[derive(Debug, Clone)]
pub struct TokenRequest {
    /// The form fields of the request.
    pub form: HashMap<String, String>,
    /// The decoded header of the client assertion, `null` when it did not decode.
    pub header: Value,
    /// The decoded claims of the client assertion, `null` when they did not decode.
    pub claims: Value,
    /// Why the request was refused as `invalid_client`, empty when it was not.
    pub refusal: String,
}

/// One request the fake farm received.
#[derive(Debug, Clone)]
pub struct Recorded {
    /// The request's path, without the query.
    pub path: String,
    /// The request's query, empty when there was none.
    pub query: String,
    /// The `Authorization` header, empty when there was none.
    pub authorization: String,
    /// The `Content-Type` header, empty when there was none.
    pub content_type: String,
    /// The request's body.
    pub body: Vec<u8>,
}

impl Recorded {
    /// Returns the `seq` query parameter of a log request.
    #[must_use]
    pub fn seq(&self) -> Option<u64> {
        for pair in self.query.split('&') {
            let Some((key, value)) = pair.split_once('=') else {
                continue;
            };
            if key == "seq" {
                return value.parse::<u64>().ok();
            }
        }
        None
    }

    /// Returns the request's body as text.
    #[must_use]
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

#[derive(Default)]
struct Route {
    queue: VecDeque<Reply>,
    sticky: Option<Reply>,
}

impl Route {
    fn take(&mut self, fallback: Reply) -> Reply {
        if let Some(reply) = self.queue.pop_front() {
            return reply;
        }
        let Some(reply) = &self.sticky else {
            return fallback;
        };
        reply.clone()
    }
}

#[derive(Default)]
struct Routes {
    claim: Route,
    runs: Route,
    log: Route,
    heartbeat: Route,
    complete: Route,
    progress: Route,
    requests: Vec<Recorded>,
    last_seq: HashMap<String, u64>,
    grants: VecDeque<Grant>,
    token_requests: Vec<TokenRequest>,
    keys: HashMap<String, Vec<u8>>,
    seen_jti: HashSet<String>,
    issued: HashMap<String, String>,
    minted: u64,
    owners: HashMap<String, String>,
}

struct Shared {
    routes: Mutex<Routes>,
    release: Notify,
    issuer: Mutex<String>,
}

/// A farm the client can talk to over loopback.
pub struct FakeFarm {
    url: String,
    shared: Arc<Shared>,
}

impl FakeFarm {
    /// Starts a farm on an ephemeral loopback port that knows the suite's key.
    pub async fn start() -> Self {
        let shared = Arc::new(Shared {
            routes: Mutex::new(Routes::default()),
            release: Notify::new(),
            issuer: Mutex::new(String::new()),
        });
        let router = Router::new()
            .route(TOKEN_PATH, post(token))
            .route("/external/runner/claim", post(claim))
            .route("/external/runner/runs", post(runs))
            .route("/external/runner/jobs/{id}/log", post(log))
            .route("/external/runner/jobs/{id}/heartbeat", post(heartbeat))
            .route("/external/runner/jobs/{id}/complete", post(complete))
            .route("/external/runner/jobs/{id}/progress", post(progress))
            .with_state(Arc::clone(&shared));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let url = format!("http://{address}");
        *shared.issuer.lock().unwrap() = url.clone();
        let farm = FakeFarm { url, shared };
        let key = super::test_key();
        farm.register_key(super::KEY_ID, &key.public);
        farm
    }

    /// Accepts assertions whose `kid` is `kid` when `public` verifies them.
    pub fn register_key(&self, kid: &str, public: &[u8]) {
        let mut routes = self.shared.routes.lock().unwrap();
        routes.keys.insert(kid.to_string(), public.to_vec());
    }

    /// Queues `grant` for the next valid token request.
    pub fn push_grant(&self, grant: Grant) {
        let mut routes = self.shared.routes.lock().unwrap();
        routes.grants.push_back(grant);
    }

    /// Forgets every token minted so far, so each one is answered `401`.
    pub fn revoke_tokens(&self) {
        let mut routes = self.shared.routes.lock().unwrap();
        routes.issued.clear();
    }

    /// Returns every token request the endpoint received, in order.
    #[must_use]
    pub fn token_requests(&self) -> Vec<TokenRequest> {
        let routes = self.shared.routes.lock().unwrap();
        routes.token_requests.clone()
    }

    /// Returns the base URL of this farm.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Queues `reply` for the next claim.
    pub fn push_claim(&self, reply: Reply) {
        let mut routes = self.shared.routes.lock().unwrap();
        routes.claim.queue.push_back(reply);
    }

    /// Queues `reply` for the next run opening.
    pub fn push_runs(&self, reply: Reply) {
        let mut routes = self.shared.routes.lock().unwrap();
        routes.runs.queue.push_back(reply);
    }

    /// Queues `reply` for the next log chunk.
    pub fn push_log(&self, reply: Reply) {
        let mut routes = self.shared.routes.lock().unwrap();
        routes.log.queue.push_back(reply);
    }

    /// Queues `reply` for the next heartbeat.
    pub fn push_heartbeat(&self, reply: Reply) {
        let mut routes = self.shared.routes.lock().unwrap();
        routes.heartbeat.queue.push_back(reply);
    }

    /// Queues `reply` for the next completion.
    pub fn push_complete(&self, reply: Reply) {
        let mut routes = self.shared.routes.lock().unwrap();
        routes.complete.queue.push_back(reply);
    }

    /// Queues `reply` for the next progress snapshot.
    pub fn push_progress(&self, reply: Reply) {
        let mut routes = self.shared.routes.lock().unwrap();
        routes.progress.queue.push_back(reply);
    }

    /// Answers every unqueued claim with `reply`.
    pub fn always_claim(&self, reply: Reply) {
        let mut routes = self.shared.routes.lock().unwrap();
        routes.claim.sticky = Some(reply);
    }

    /// Answers every unqueued log chunk with `reply`.
    pub fn always_log(&self, reply: Reply) {
        let mut routes = self.shared.routes.lock().unwrap();
        routes.log.sticky = Some(reply);
    }

    /// Answers every unqueued heartbeat with `reply`.
    pub fn always_heartbeat(&self, reply: Reply) {
        let mut routes = self.shared.routes.lock().unwrap();
        routes.heartbeat.sticky = Some(reply);
    }

    /// Answers every unqueued completion with `reply`.
    pub fn always_complete(&self, reply: Reply) {
        let mut routes = self.shared.routes.lock().unwrap();
        routes.complete.sticky = Some(reply);
    }

    /// Answers every unqueued progress snapshot with `reply`.
    pub fn always_progress(&self, reply: Reply) {
        let mut routes = self.shared.routes.lock().unwrap();
        routes.progress.sticky = Some(reply);
    }

    /// Releases a held claim, which then answers `reply`.
    pub fn release_claim(&self, reply: Reply) {
        let mut routes = self.shared.routes.lock().unwrap();
        routes.claim.queue.push_front(reply);
        drop(routes);
        self.shared.release.notify_one();
    }

    /// Releases a held run opening, which then answers `reply`.
    pub fn release_runs(&self, reply: Reply) {
        let mut routes = self.shared.routes.lock().unwrap();
        routes.runs.queue.push_front(reply);
        drop(routes);
        self.shared.release.notify_one();
    }

    /// Releases a held completion, which then answers `reply`.
    pub fn release_complete(&self, reply: Reply) {
        let mut routes = self.shared.routes.lock().unwrap();
        routes.complete.queue.push_front(reply);
        drop(routes);
        self.shared.release.notify_one();
    }

    /// Releases a held progress post, which then answers `reply`.
    pub fn release_progress(&self, reply: Reply) {
        let mut routes = self.shared.routes.lock().unwrap();
        routes.progress.queue.push_front(reply);
        drop(routes);
        self.shared.release.notify_one();
    }

    /// Returns every request this farm received, in order.
    #[must_use]
    pub fn requests(&self) -> Vec<Recorded> {
        let routes = self.shared.routes.lock().unwrap();
        routes.requests.clone()
    }

    /// Returns every received request whose path ends with `suffix`.
    #[must_use]
    pub fn requests_ending(&self, suffix: &str) -> Vec<Recorded> {
        let routes = self.shared.routes.lock().unwrap();
        let mut selected = Vec::new();
        for request in &routes.requests {
            if request.path.ends_with(suffix) {
                selected.push(request.clone());
            }
        }
        selected
    }
}

/// Returns the reply that hands out [`sample_job`] under `run_id`.
#[must_use]
pub fn job_reply(run_id: &str) -> Reply {
    Reply::Job(Box::new(sample_job(run_id)))
}

/// Returns a job the fake farm can hand out, with `run_id` as its identifier.
#[must_use]
pub fn sample_job(run_id: &str) -> Job {
    Job {
        run_id: RunId(run_id.to_string()),
        issue_id: "issue-uuid".to_string(),
        identifier: "FARM-12".to_string(),
        issue_url: "https://linear.app/example/issue/FARM-12".to_string(),
        title: "split farm and runner".to_string(),
        repo_slug: "owner/repo".to_string(),
        plan_path: "/abs/checkout/docs/plans/x.md".to_string(),
        branch: Branch("x".to_string()),
        mode: String::new(),
        lease_ttl_seconds: 180,
        runtime: "native".to_string(),
        ctx: "/abs/checkout".to_string(),
        create_pr: CreatePr::Yes,
    }
}

fn admitted(shared: &Arc<Shared>, uri: &Uri, headers: &HeaderMap, body: &Bytes) -> Option<String> {
    record(shared, uri, headers, body);
    let authorization = header_value(headers, "authorization");
    let routes = shared.routes.lock().unwrap();
    let token = authorization.strip_prefix("Bearer ")?;
    routes.issued.get(token).cloned()
}

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, r#"{"error":"invalid_token"}"#).into_response()
}

fn held_by_another(shared: &Arc<Shared>, id: &str, caller: &str) -> Option<Response> {
    let routes = shared.routes.lock().unwrap();
    let owner = routes.owners.get(id)?;
    if owner == caller {
        return None;
    }
    Some((StatusCode::GONE, r#"{"error":"run is gone"}"#).into_response())
}

fn handed_out(shared: &Arc<Shared>, reply: &Reply, caller: &str) {
    let Reply::Job(job) = reply else {
        return;
    };
    let mut routes = shared.routes.lock().unwrap();
    routes
        .owners
        .insert(job.run_id.as_str().to_string(), caller.to_string());
}

fn record(shared: &Arc<Shared>, uri: &Uri, headers: &HeaderMap, body: &Bytes) {
    let authorization = header_value(headers, "authorization");
    let content_type = header_value(headers, "content-type");
    let query = match uri.query() {
        Some(query) => query.to_string(),
        None => String::new(),
    };
    let mut routes = shared.routes.lock().unwrap();
    routes.requests.push(Recorded {
        path: uri.path().to_string(),
        query,
        authorization,
        content_type,
        body: body.to_vec(),
    });
}

fn header_value(headers: &HeaderMap, name: &str) -> String {
    let Some(value) = headers.get(name) else {
        return String::new();
    };
    let Ok(value) = value.to_str() else {
        return String::new();
    };
    value.to_string()
}

fn respond(reply: Reply) -> Response {
    match reply {
        Reply::Job(job) => (StatusCode::OK, Json(job)).into_response(),
        Reply::NoJob => StatusCode::NO_CONTENT.into_response(),
        Reply::Accepted => StatusCode::OK.into_response(),
        Reply::Beat(action) => (StatusCode::OK, Json(HeartbeatResponse { action })).into_response(),
        Reply::Status(code, body) => {
            let status = StatusCode::from_u16(code).unwrap();
            (status, body).into_response()
        }
        Reply::Hold => StatusCode::NO_CONTENT.into_response(),
    }
}

fn accepts(reply: &Reply) -> bool {
    match reply {
        Reply::Job(_) => true,
        Reply::NoJob => true,
        Reply::Accepted => true,
        Reply::Beat(_) => true,
        Reply::Hold => true,
        Reply::Status(code, _) => (200..300).contains(code),
    }
}

async fn claim(
    State(shared): State<Arc<Shared>>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(caller) = admitted(&shared, &uri, &headers, &body) else {
        return unauthorized();
    };
    if let Some(mismatch) = version_mismatch(&body) {
        return respond(mismatch);
    }
    let reply = take(&shared, RouteName::Claim, Reply::NoJob);
    let reply = match reply {
        Reply::Hold => {
            shared.release.notified().await;
            take(&shared, RouteName::Claim, Reply::NoJob)
        }
        Reply::Job(_) | Reply::NoJob | Reply::Accepted | Reply::Beat(_) | Reply::Status(..) => {
            reply
        }
    };
    handed_out(&shared, &reply, &caller);
    respond(reply)
}

async fn runs(
    State(shared): State<Arc<Shared>>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(caller) = admitted(&shared, &uri, &headers, &body) else {
        return unauthorized();
    };
    if let Some(mismatch) = version_mismatch(&body) {
        return respond(mismatch);
    }
    let unscripted = Reply::Status(400, "no run scripted".to_string());
    let reply = take(&shared, RouteName::Runs, unscripted.clone());
    let reply = match reply {
        Reply::Hold => {
            shared.release.notified().await;
            take(&shared, RouteName::Runs, unscripted)
        }
        Reply::Job(_) | Reply::NoJob | Reply::Accepted | Reply::Beat(_) | Reply::Status(..) => {
            reply
        }
    };
    handed_out(&shared, &reply, &caller);
    respond(reply)
}

fn version_mismatch(body: &Bytes) -> Option<Reply> {
    let Ok(request) = serde_json::from_slice::<Value>(body) else {
        return None;
    };
    let version = match request.get("version") {
        Some(Value::String(version)) => version.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    };
    if version == VERSION {
        return None;
    }
    Some(Reply::Status(
        409,
        format!(
            r#"{{"error":"protocol version mismatch: runner speaks \"{version}\", farm speaks \"{VERSION}\""}}"#
        ),
    ))
}

async fn log(
    State(shared): State<Arc<Shared>>,
    Path(id): Path<String>,
    Query(query): Query<LogQuery>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(caller) = admitted(&shared, &uri, &headers, &body) else {
        return unauthorized();
    };
    if let Some(gone) = held_by_another(&shared, &id, &caller) {
        return gone;
    }
    let LogQuery { seq } = query;
    let mut routes = shared.routes.lock().unwrap();
    let last = match routes.last_seq.get(&id) {
        Some(last) => *last,
        None => 0,
    };
    if seq < 1 {
        drop(routes);
        return respond(Reply::Status(
            400,
            "invalid seq: must be a positive integer".to_string(),
        ));
    }
    if seq <= last {
        drop(routes);
        return accepted();
    }
    let reply = routes.log.take(Reply::Accepted);
    if accepts(&reply) {
        routes.last_seq.insert(id, seq);
    }
    drop(routes);
    let Reply::Accepted = reply else {
        return respond(reply);
    };
    accepted()
}

fn accepted() -> Response {
    StatusCode::NO_CONTENT.into_response()
}

async fn heartbeat(
    State(shared): State<Arc<Shared>>,
    Path(id): Path<String>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(caller) = admitted(&shared, &uri, &headers, &body) else {
        return unauthorized();
    };
    if let Some(gone) = held_by_another(&shared, &id, &caller) {
        return gone;
    }
    if let Some(mismatch) = version_mismatch(&body) {
        return respond(mismatch);
    }
    let reply = take(
        &shared,
        RouteName::Heartbeat,
        Reply::Beat(HeartbeatAction::None),
    );
    respond(reply)
}

async fn complete(
    State(shared): State<Arc<Shared>>,
    Path(id): Path<String>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(caller) = admitted(&shared, &uri, &headers, &body) else {
        return unauthorized();
    };
    if let Some(gone) = held_by_another(&shared, &id, &caller) {
        return gone;
    }
    let reply = take(&shared, RouteName::Complete, Reply::Accepted);
    let Reply::Hold = reply else {
        return respond(reply);
    };
    shared.release.notified().await;
    respond(take(&shared, RouteName::Complete, Reply::Accepted))
}

async fn progress(
    State(shared): State<Arc<Shared>>,
    Path(id): Path<String>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(caller) = admitted(&shared, &uri, &headers, &body) else {
        return unauthorized();
    };
    if let Some(gone) = held_by_another(&shared, &id, &caller) {
        return gone;
    }
    let reply = take(&shared, RouteName::Progress, Reply::Accepted);
    let reply = match reply {
        Reply::Hold => {
            shared.release.notified().await;
            take(&shared, RouteName::Progress, Reply::Accepted)
        }
        Reply::Job(_) | Reply::NoJob | Reply::Accepted | Reply::Beat(_) | Reply::Status(..) => {
            reply
        }
    };
    let Reply::Accepted = reply else {
        return respond(reply);
    };
    accepted()
}

#[derive(Debug, Clone, Copy)]
enum RouteName {
    Claim,
    Runs,
    Heartbeat,
    Complete,
    Progress,
}

fn take(shared: &Arc<Shared>, name: RouteName, fallback: Reply) -> Reply {
    let mut routes = shared.routes.lock().unwrap();
    match name {
        RouteName::Claim => routes.claim.take(fallback),
        RouteName::Runs => routes.runs.take(fallback),
        RouteName::Heartbeat => routes.heartbeat.take(fallback),
        RouteName::Complete => routes.complete.take(fallback),
        RouteName::Progress => routes.progress.take(fallback),
    }
}

#[derive(serde::Deserialize)]
struct LogQuery {
    seq: u64,
}

async fn token(
    State(shared): State<Arc<Shared>>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let issuer = shared.issuer.lock().unwrap().clone();
    let mut routes = shared.routes.lock().unwrap();
    let assertion = match form.get("client_assertion") {
        Some(assertion) => assertion.clone(),
        None => String::new(),
    };
    let (header, claims, signed, signature) = split_assertion(&assertion);
    let refusal = refusal(
        &routes, &form, &issuer, &header, &claims, &signed, &signature,
    );
    routes.token_requests.push(TokenRequest {
        form: form.clone(),
        header,
        claims: claims.clone(),
        refusal: refusal.clone(),
    });
    if !refusal.is_empty() {
        return (StatusCode::UNAUTHORIZED, r#"{"error":"invalid_client"}"#).into_response();
    }
    if let Some(Value::String(jti)) = claims.get("jti") {
        routes.seen_jti.insert(jti.clone());
    }
    let grant = match routes.grants.pop_front() {
        Some(grant) => grant,
        None => Grant::Token(TOKEN_LIFETIME),
    };
    match grant {
        Grant::Token(lifetime) => {
            routes.minted += 1;
            let value = format!("token-{}", routes.minted);
            let client = match form.get("client_id") {
                Some(client) => client.clone(),
                None => String::new(),
            };
            routes.issued.insert(value.clone(), client);
            let body = serde_json::json!({
                "access_token": value,
                "token_type": "bearer",
                "expires_in": lifetime,
                "scope": SCOPE,
            });
            (StatusCode::OK, Json(body)).into_response()
        }
        Grant::Status(code, body) => (StatusCode::from_u16(code).unwrap(), body).into_response(),
    }
}

fn split_assertion(assertion: &str) -> (Value, Value, String, Vec<u8>) {
    let parts: Vec<&str> = assertion.split('.').collect();
    if parts.len() != 3 {
        return (Value::Null, Value::Null, String::new(), Vec::new());
    }
    let header = decode_part(parts[0]);
    let claims = decode_part(parts[1]);
    let signature = URL_SAFE_NO_PAD.decode(parts[2]).unwrap_or_default();
    (
        header,
        claims,
        format!("{}.{}", parts[0], parts[1]),
        signature,
    )
}

fn decode_part(part: &str) -> Value {
    let Ok(bytes) = URL_SAFE_NO_PAD.decode(part) else {
        return Value::Null;
    };
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(value) => value,
        Err(_) => Value::Null,
    }
}

fn refusal(
    routes: &Routes,
    form: &HashMap<String, String>,
    issuer: &str,
    header: &Value,
    claims: &Value,
    signed: &str,
    signature: &[u8],
) -> String {
    let field = |name: &str| match form.get(name) {
        Some(value) => value.clone(),
        None => String::new(),
    };
    let claim = |name: &str| match claims.get(name) {
        Some(Value::String(value)) => value.clone(),
        Some(_) => String::new(),
        None => String::new(),
    };
    if field("grant_type") != GRANT_TYPE {
        return "grant_type".to_string();
    }
    if field("client_assertion_type") != ASSERTION_TYPE {
        return "client_assertion_type".to_string();
    }
    if field("scope") != SCOPE {
        return "scope".to_string();
    }
    if field("resource") != super::RESOURCE {
        return "resource".to_string();
    }
    let client = field("client_id");
    if client.is_empty() {
        return "client_id".to_string();
    }
    if header.get("alg") != Some(&Value::String("RS256".to_string())) {
        return "alg".to_string();
    }
    let Some(Value::String(kid)) = header.get("kid") else {
        return "kid".to_string();
    };
    let Some(public) = routes.keys.get(kid) else {
        return "unknown kid".to_string();
    };
    let verified = UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, public)
        .verify(signed.as_bytes(), signature);
    if verified.is_err() {
        return "signature".to_string();
    }
    if claim("iss") != client || claim("sub") != client {
        return "iss or sub".to_string();
    }
    if claim("aud") != issuer {
        return "aud".to_string();
    }
    let jti = claim("jti");
    if jti.is_empty() || routes.seen_jti.contains(&jti) {
        return "jti".to_string();
    }
    let (Some(iat), Some(exp)) = (
        claims.get("iat").and_then(Value::as_u64),
        claims.get("exp").and_then(Value::as_u64),
    ) else {
        return "iat or exp".to_string();
    };
    if exp != iat + 60 {
        return "lifetime".to_string();
    }
    String::new()
}
