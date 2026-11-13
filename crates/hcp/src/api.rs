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
use chrono::Utc;
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
use crate::store::{ApiKey, AuditFilter, RequestFilter, StoreError};

/// Lets a key submit human responses through `POST /v1/requests/{id}/respond`.
pub const SCOPE_RESPOND: &str = "respond";
/// Lets a key see every agent's requests and audit events.
pub const SCOPE_ADMIN: &str = "admin";

const MAX_PAGE: i64 = 500;

#[derive(Clone)]
pub struct AppState {
    pub engine: Engine,
    pub shutdown: CancellationToken,
    sse_clients: Arc<AtomicUsize>,
}

impl AppState {
    pub fn new(engine: Engine, shutdown: CancellationToken) -> Self {
        Self { engine, shutdown, sse_clients: Arc::new(AtomicUsize::new(0)) }
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
            EngineError::IdempotencyKeyTaken => Self::new(StatusCode::CONFLICT, "Idempotency key is already in use"),
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
