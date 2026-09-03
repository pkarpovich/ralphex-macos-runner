//! An in-process stand-in for the farm's runner API.
//!
//! A test scripts each route with a queue of [`Reply`] values, or with a sticky
//! reply the route falls back to once the queue is empty, and reads back every
//! request the client made. The log route enforces the farm's own sequence
//! rule, and the run route its version check, so a client that numbers a chunk
//! below the last accepted value or speaks another protocol version meets the
//! same answer here as at the farm.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use ralphex_macos_runner::protocol::types::{
    Branch, CreatePr, HeartbeatAction, HeartbeatResponse, Job, OpenRunRequest, RunId, VERSION,
};
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
    requests: Vec<Recorded>,
    last_seq: HashMap<String, u64>,
}

struct Shared {
    routes: Mutex<Routes>,
    release: Notify,
}

/// A farm the client can talk to over loopback.
pub struct FakeFarm {
    url: String,
    shared: Arc<Shared>,
}

impl FakeFarm {
    /// Starts a farm on an ephemeral loopback port.
    pub async fn start() -> Self {
        let shared = Arc::new(Shared {
            routes: Mutex::new(Routes::default()),
            release: Notify::new(),
        });
        let router = Router::new()
            .route("/api/runner/claim", post(claim))
            .route("/api/runner/runs", post(runs))
            .route("/api/runner/jobs/{id}/log", post(log))
            .route("/api/runner/jobs/{id}/heartbeat", post(heartbeat))
            .route("/api/runner/jobs/{id}/complete", post(complete))
            .with_state(Arc::clone(&shared));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        FakeFarm {
            url: format!("http://{address}"),
            shared,
        }
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
        Reply::Job(job) => (StatusCode::OK, axum::Json(job)).into_response(),
        Reply::NoJob => StatusCode::NO_CONTENT.into_response(),
        Reply::Accepted => StatusCode::OK.into_response(),
        Reply::Beat(action) => {
            (StatusCode::OK, axum::Json(HeartbeatResponse { action })).into_response()
        }
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
    record(&shared, &uri, &headers, &body);
    let reply = take(&shared, RouteName::Claim, Reply::NoJob);
    let Reply::Hold = reply else {
        return respond(reply);
    };
    shared.release.notified().await;
    respond(take(&shared, RouteName::Claim, Reply::NoJob))
}

async fn runs(
    State(shared): State<Arc<Shared>>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    record(&shared, &uri, &headers, &body);
    if let Some(mismatch) = version_mismatch(&body) {
        return respond(mismatch);
    }
    let reply = take(
        &shared,
        RouteName::Runs,
        Reply::Status(400, "no run scripted".to_string()),
    );
    let Reply::Hold = reply else {
        return respond(reply);
    };
    shared.release.notified().await;
    respond(take(
        &shared,
        RouteName::Runs,
        Reply::Status(400, "no run scripted".to_string()),
    ))
}

fn version_mismatch(body: &Bytes) -> Option<Reply> {
    let Ok(request) = serde_json::from_slice::<OpenRunRequest>(body) else {
        return None;
    };
    let OpenRunRequest {
        runner,
        version,
        runtime: _,
        repo: _,
        ctx: _,
        plan: _,
        branch: _,
        create_pr: _,
    } = request;
    if version == VERSION {
        return None;
    }
    Some(Reply::Status(
        409,
        format!(
            r#"{{"error":"protocol version mismatch: runner \"{runner}\" speaks \"{version}\", farm speaks \"{VERSION}\""}}"#
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
    record(&shared, &uri, &headers, &body);
    let LogQuery { seq } = query;
    let mut routes = shared.routes.lock().unwrap();
    let last = match routes.last_seq.get(&id) {
        Some(last) => *last,
        None => 0,
    };
    if seq < 1 || seq <= last {
        drop(routes);
        return respond(Reply::Status(400, format!("bad seq {seq}")));
    }
    let reply = routes.log.take(Reply::Accepted);
    if accepts(&reply) {
        routes.last_seq.insert(id, seq);
    }
    drop(routes);
    respond(reply)
}

async fn heartbeat(
    State(shared): State<Arc<Shared>>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    record(&shared, &uri, &headers, &body);
    let reply = take(
        &shared,
        RouteName::Heartbeat,
        Reply::Beat(HeartbeatAction::None),
    );
    respond(reply)
}

async fn complete(
    State(shared): State<Arc<Shared>>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    record(&shared, &uri, &headers, &body);
    let reply = take(&shared, RouteName::Complete, Reply::Accepted);
    respond(reply)
}

#[derive(Debug, Clone, Copy)]
enum RouteName {
    Claim,
    Runs,
    Heartbeat,
    Complete,
}

fn take(shared: &Arc<Shared>, name: RouteName, fallback: Reply) -> Reply {
    let mut routes = shared.routes.lock().unwrap();
    match name {
        RouteName::Claim => routes.claim.take(fallback),
        RouteName::Runs => routes.runs.take(fallback),
        RouteName::Heartbeat => routes.heartbeat.take(fallback),
        RouteName::Complete => routes.complete.take(fallback),
    }
}

#[derive(serde::Deserialize)]
struct LogQuery {
    seq: u64,
}
