//! The HTTP API agents talk to. Wire-compatible with the original service for agents;
//! see the README for the deliberate differences (who may respond, and who may see what).

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::body::Bytes;
use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use futures_util::{Stream, StreamExt, stream};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio_stream::wrappers::BroadcastStream;
use tokio_util::sync::CancellationToken;

use crate::domain::{
    ActorType, CoordinationRequest, CreateRequest, State as CrState, SubmitResponse, new_id, timestamp,
};
use crate::engine::{Answer, Engine, EngineError};
use crate::slack;
use crate::store::{ApiKey, AuditFilter, MetricsError, RequestFilter, StoreError};
use crate::vault::Vault;

/// Lets a key submit human responses through `POST /v1/requests/{id}/respond`.
pub const SCOPE_RESPOND: &str = "respond";
/// Lets a key see every agent's requests and audit events.
pub const SCOPE_ADMIN: &str = "admin";

const MAX_PAGE: i64 = 500;

#[derive(Clone)]
pub struct AppState {
    pub engine: Engine,
    pub shutdown: CancellationToken,
    /// The vault being written to, for the status report.
    pub vault: Option<Arc<Vault>>,
    started_at: DateTime<Utc>,
    sse_clients: Arc<AtomicUsize>,
}

impl AppState {
    pub fn new(engine: Engine, shutdown: CancellationToken, vault: Option<Arc<Vault>>) -> Self {
        Self { engine, shutdown, vault, started_at: Utc::now(), sse_clients: Arc::new(AtomicUsize::new(0)) }
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/requests", post(create_request).get(list_requests))
        .route("/v1/requests/{id}", get(get_request).delete(cancel_request))
        .route("/v1/requests/{id}/respond", post(respond))
        .route("/v1/audit", get(audit))
        .route("/v1/events", get(events))
        .route("/v1/status", get(status))
        .route("/v1/status/{connector}/test", post(test_connector))
        .route("/v1/metrics", get(metrics))
        .route("/slack/interactions", post(slack_interactions))
        .with_state(state)
}

// ── Errors ──────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    body: Value,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self { status, body: json!({ "error": message.into() }) }
    }

    fn validation(details: impl serde::Serialize) -> Self {
        Self { status: StatusCode::BAD_REQUEST, body: json!({ "error": "Validation error", "details": details }) }
    }

    fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "Not found")
    }

    fn internal(err: impl std::fmt::Display) -> Self {
        tracing::error!(error = %err, "request failed");
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error")
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(self.body)).into_response()
    }
}

impl From<EngineError> for ApiError {
    fn from(err: EngineError) -> Self {
        match err {
            EngineError::NotFound => Self::not_found(),
            EngineError::Refused(problems) => Self::validation(problems),
            EngineError::WrongState { action, state } => {
                Self::new(StatusCode::CONFLICT, format!("Cannot {action} CR in state {state}"))
            }
            EngineError::Store(StoreError::Conflict { .. }) => {
                Self::new(StatusCode::CONFLICT, "The request changed state concurrently")
            }
            EngineError::Store(err) => Self::internal(err),
        }
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(err: sqlx::Error) -> Self {
        Self::internal(err)
    }
}

// ── Authentication ──────────────────────────────────────────────────────────

/// The API key that authenticated the request.
pub struct Caller(pub ApiKey);

impl Caller {
    fn is_admin(&self) -> bool {
        self.0.has_scope(SCOPE_ADMIN)
    }

    fn require_admin(&self) -> Result<(), ApiError> {
        if self.is_admin() {
            Ok(())
        } else {
            Err(ApiError::new(StatusCode::FORBIDDEN, "This API key lacks the 'admin' scope"))
        }
    }

    /// Agents see their own requests; nobody learns whether someone else's ID exists.
    fn can_see(&self, cr: &CoordinationRequest) -> bool {
        self.is_admin() || cr.agent_id == self.0.agent_id
    }

    /// The request, if it exists and this caller may see it.
    async fn visible(&self, engine: &Engine, request_id: &str) -> Result<CoordinationRequest, ApiError> {
        let cr = engine.get(request_id).await?;
        if self.can_see(&cr) { Ok(cr) } else { Err(ApiError::not_found()) }
    }
}

impl FromRequestParts<AppState> for Caller {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        let token = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .ok_or_else(|| ApiError::new(StatusCode::UNAUTHORIZED, "Missing or invalid Authorization header"))?;
        match state.engine.store().authenticate(token).await? {
            Some(key) => Ok(Caller(key)),
            None => Err(ApiError::new(StatusCode::UNAUTHORIZED, "Invalid API key")),
        }
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn parse_body<T: DeserializeOwned>(body: &Bytes) -> Result<T, ApiError> {
    serde_json::from_slice(body).map_err(|err| ApiError::validation(err.to_string()))
}

/// Reads a non-negative integer query parameter, clamped to `max`.
fn page_param(query: &HashMap<String, String>, name: &str, default: i64, max: i64) -> Result<i64, ApiError> {
    match query.get(name) {
        None => Ok(default),
        Some(raw) => match raw.parse::<i64>() {
            Ok(value) if value >= 0 => Ok(value.min(max)),
            _ => Err(ApiError::validation(format!("{name} must be a non-negative integer"))),
        },
    }
}

// ── Handlers ────────────────────────────────────────────────────────────────

async fn health(State(state): State<AppState>) -> Response {
    let database = state.engine.store().ping().await.is_ok();
    let status = if database { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE };
    let body = json!({
        "status": if database { "ok" } else { "degraded" },
        "timestamp": timestamp::format(&Utc::now()),
        "version": env!("CARGO_PKG_VERSION"),
        "sse_clients": state.sse_clients.load(Ordering::Relaxed),
    });
    (status, Json(body)).into_response()
}

async fn create_request(State(state): State<AppState>, caller: Caller, body: Bytes) -> Result<Response, ApiError> {
    let input: CreateRequest = parse_body(&body)?;
    input.validate(Utc::now()).map_err(ApiError::validation)?;
    let (cr, created) = state.engine.submit(&caller.0.agent_id, input).await?;
    let status = if created { StatusCode::CREATED } else { StatusCode::OK };
    Ok((status, Json(cr)).into_response())
}

async fn get_request(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<String>,
) -> Result<Json<CoordinationRequest>, ApiError> {
    let cr = caller.visible(&state.engine, &id).await?;
    let cr = state.engine.settle_if_expired(cr, Utc::now()).await?;
    // Reading an answered request is how its agent takes delivery of the answer.
    if cr.state == CrState::Responded && cr.agent_id == caller.0.agent_id {
        return Ok(Json(state.engine.mark_delivered(cr).await?));
    }
    Ok(Json(cr))
}

async fn list_requests(
    State(state): State<AppState>,
    caller: Caller,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Json<Value>, ApiError> {
    let filter = RequestFilter {
        agent_id: if caller.is_admin() { query.get("agent_id").cloned() } else { Some(caller.0.agent_id.clone()) },
        state: query.get("state").cloned(),
        intent: query.get("intent").cloned(),
        urgency: query.get("urgency").cloned(),
        responder_id: query.get("responder_id").cloned(),
        limit: page_param(&query, "limit", 50, MAX_PAGE)?,
        offset: page_param(&query, "offset", 0, i64::MAX)?,
    };
    let requests = state.engine.store().list_requests(&filter).await?;
    Ok(Json(json!({ "requests": requests })))
}

async fn cancel_request(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let cr = caller.visible(&state.engine, &id).await?;
    state.engine.cancel(&cr, &caller.0.agent_id, ActorType::Agent).await?;
    Ok(Json(json!({ "status": "cancelled", "request_id": cr.request_id })))
}

async fn respond(
    State(state): State<AppState>,
    caller: Caller,
    Path(id): Path<String>,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    // An agent's own key must never be able to answer the question it asked.
    if !caller.0.has_scope(SCOPE_RESPOND) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "This API key cannot submit responses: it lacks the 'respond' scope",
        ));
    }
    let input: SubmitResponse = parse_body(&body)?;
    input.validate().map_err(ApiError::validation)?;
    let responded = state
        .engine
        .respond(
            &id,
            Answer {
                response_data: Value::Object(input.response_data),
                actor: &input.responded_by,
                responded_by: &input.responded_by,
                slack_user: None,
                audit: json!({ "via_key": caller.0.key_id }),
            },
        )
        .await?;
    Ok(Json(json!({ "status": "responded", "request_id": responded.request_id })))
}

async fn audit(
    State(state): State<AppState>,
    caller: Caller,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Json<Value>, ApiError> {
    let filter = AuditFilter {
        request_id: query.get("request_id").cloned(),
        event_type: query.get("event_type").cloned(),
        agent_id: if caller.is_admin() { None } else { Some(caller.0.agent_id.clone()) },
        limit: page_param(&query, "limit", 100, MAX_PAGE)?,
        offset: page_param(&query, "offset", 0, i64::MAX)?,
    };
    let events = state.engine.store().audit_trail(&filter).await?;
    Ok(Json(json!({ "events": events })))
}

/// Counts an SSE connection for as long as its stream is alive.
struct SseClient(Arc<AtomicUsize>);

impl SseClient {
    fn connect(counter: &Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        Self(counter.clone())
    }
}

impl Drop for SseClient {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

async fn events(State(state): State<AppState>, caller: Caller) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let client = SseClient::connect(&state.sse_clients);
    let (admin, agent_id) = (caller.is_admin(), caller.0.agent_id.clone());

    let connected = Event::default().event("connected").data(json!({ "client_id": new_id() }).to_string());
    let changes = BroadcastStream::new(state.engine.subscribe()).filter_map(move |change| {
        // Keeps the connection counted until the stream is dropped.
        let _ = &client;
        // A lagging subscriber misses events rather than stalling everyone else.
        let event = change.ok().filter(|change| admin || change.agent_id == agent_id).map(|change| {
            Ok(Event::default().event("state_change").data(serde_json::to_string(&change).unwrap_or_default()))
        });
        std::future::ready(event)
    });

    let stream = stream::once(std::future::ready(Ok(connected)))
        .chain(changes)
        // Without this, open streams would hold graceful shutdown open forever.
        .take_until(state.shutdown.clone().cancelled_owned());
    Sse::new(stream).keep_alive(KeepAlive::default())
}

// ── Dashboard ───────────────────────────────────────────────────────────────

const MAX_METRICS_DAYS: i64 = 90;

fn millis(started: std::time::Instant) -> f64 {
    (started.elapsed().as_secs_f64() * 1000.0 * 10.0).round() / 10.0
}

async fn database_status(state: &AppState) -> Value {
    let started = std::time::Instant::now();
    match state.engine.store().database_stats().await {
        Ok(stats) => {
            let mut body = json!({ "ok": true, "latency_ms": millis(started) });
            if let (Some(body), Value::Object(stats)) = (body.as_object_mut(), json!(stats)) {
                body.extend(stats);
            }
            body
        }
        Err(err) => json!({ "ok": false, "latency_ms": millis(started), "error": err.to_string() }),
    }
}

fn slack_status(state: &AppState) -> Value {
    let Some(slack) = state.engine.slack() else {
        return json!({ "configured": false });
    };
    let config = slack.config();
    let socket = config.app_token.is_some();
    json!({
        "configured": true,
        "mode": if socket { "socket" } else { "http" },
        "channel": config.channel,
        "notify_channel": config.notify_channel,
        "allowed_channels": config.allowed_channels,
        "responders": config.responders,
        "identity": slack.identity(),
        "api": slack.api_health().snapshot(),
        "socket": socket.then(|| slack.socket_health().snapshot()),
    })
}

fn vault_status(state: &AppState) -> Value {
    let Some(vault) = &state.vault else {
        return json!({ "configured": false });
    };
    let (last_write_at, written) = vault.writes();
    json!({
        "configured": true,
        "vault_path": vault.vault_path(),
        "folder_path": vault.root(),
        "looks_like_obsidian": vault.vault_path().join(".obsidian").is_dir(),
        "health": vault.health(),
        "last_write_at": last_write_at.as_ref().map(timestamp::format),
        "written_since_start": written,
    })
}

/// Connector health for the desktop dashboard. Names channels and responders, never
/// secrets.
async fn status(State(state): State<AppState>, caller: Caller) -> Result<Json<Value>, ApiError> {
    caller.require_admin()?;
    let now = Utc::now();
    Ok(Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "started_at": timestamp::format(&state.started_at),
        "uptime_seconds": (now - state.started_at).num_seconds(),
        "sse_clients": state.sse_clients.load(Ordering::Relaxed),
        "database": database_status(&state).await,
        "slack": slack_status(&state),
        "vault": vault_status(&state),
    })))
}

/// Actively checks one connector: `database` (a round trip), `slack` (`auth.test`; nothing
/// is posted) or `vault` (writes and removes a file in HCP's folder).
async fn test_connector(
    State(state): State<AppState>,
    caller: Caller,
    Path(connector): Path<String>,
) -> Result<Json<Value>, ApiError> {
    caller.require_admin()?;
    let started = std::time::Instant::now();
    let outcome: Result<String, String> = match connector.as_str() {
        "database" => {
            state.engine.store().ping().await.map(|()| "Database answered".to_string()).map_err(|err| err.to_string())
        }
        "slack" => match state.engine.slack() {
            None => Err("Slack is not configured (SLACK_BOT_TOKEN is not set)".to_string()),
            Some(slack) => slack
                .auth_test()
                .await
                .map(|identity| format!("Connected as {} to {}", identity.bot, identity.team))
                .map_err(|err| err.to_string()),
        },
        "vault" => match state.vault.clone() {
            None => Err("No vault is configured (HCP_OBSIDIAN_VAULT is not set)".to_string()),
            Some(vault) => tokio::task::spawn_blocking(move || vault.probe())
                .await
                .map_err(|err| err.to_string())
                .and_then(|probe| probe.map_err(|err| err.to_string()))
                .map(|()| "HCP's vault folder is writable".to_string()),
        },
        _ => return Err(ApiError::not_found()),
    };
    let (ok, message) = match outcome {
        Ok(message) => (true, message),
        Err(message) => (false, message),
    };
    Ok(Json(json!({ "connector": connector, "ok": ok, "latency_ms": millis(started), "message": message })))
}

/// Figures for the dashboard: `days` whole days up to today (default 14, at most 90) in
/// time zone `tz` (an IANA name, default `UTC`).
async fn metrics(
    State(state): State<AppState>,
    caller: Caller,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Json<crate::store::Metrics>, ApiError> {
    caller.require_admin()?;
    let days = match query.get("days") {
        None => 14,
        Some(raw) => match raw.parse::<i64>() {
            Ok(days) if (1..=MAX_METRICS_DAYS).contains(&days) => days,
            _ => return Err(ApiError::validation(format!("days must be between 1 and {MAX_METRICS_DAYS}"))),
        },
    };
    let tz = query.get("tz").map(String::as_str).filter(|tz| !tz.is_empty()).unwrap_or("UTC");
    match state.engine.store().metrics(days, tz).await {
        Ok(metrics) => Ok(Json(metrics)),
        Err(err @ MetricsError::UnknownTimeZone(_)) => Err(ApiError::validation(err.to_string())),
        Err(MetricsError::Database(err)) => Err(err.into()),
    }
}

#[derive(serde::Deserialize)]
struct SlackForm {
    payload: String,
}

/// Slack's HTTP interactivity endpoint. Only served when a signing secret is configured,
/// and every request must carry a valid signature.
async fn slack_interactions(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let Some(secret) = state.engine.slack().and_then(|slack| slack.config().signing_secret.as_deref()) else {
        return ApiError::not_found().into_response();
    };
    let header = |name: &str| headers.get(name).and_then(|value| value.to_str().ok()).unwrap_or_default();
    let verified = slack::verify_signature(
        secret,
        header("x-slack-request-timestamp"),
        &body,
        header("x-slack-signature"),
        Utc::now().timestamp(),
    );
    if let Err(err) = verified {
        tracing::warn!(error = %err, "rejected a Slack interaction");
        return ApiError::new(StatusCode::UNAUTHORIZED, "Invalid Slack signature").into_response();
    }

    let payload = serde_urlencoded::from_bytes::<SlackForm>(&body)
        .ok()
        .and_then(|form| serde_json::from_str::<Value>(&form.payload).ok());
    let Some(payload) = payload else {
        return ApiError::new(StatusCode::BAD_REQUEST, "Invalid payload").into_response();
    };
    match slack::interactions::handle(&state.engine, &payload).await {
        Some(reply) => Json(reply).into_response(),
        None => StatusCode::OK.into_response(),
    }
}
