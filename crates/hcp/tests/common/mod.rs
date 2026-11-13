//! Shared harness for the end-to-end tests: a real server, a real (throwaway) Postgres,
//! a mock Slack Web API + Socket Mode server, and a temporary Obsidian vault.

#![allow(dead_code)]

use std::collections::HashMap;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::routing::post;
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};
use postgresql_embedded::{PostgreSQL, SettingsBuilder, VersionReq};
use reqwest::StatusCode;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use hcp::app::App;
use hcp::config::{Config, SlackConfig, VaultConfig};
use hcp::domain::{CoordinationRequest, State as CrState};
use hcp::vault::Vault;

pub const BOT_TOKEN: &str = "xoxb-test-bot-token";
pub const APP_TOKEN: &str = "xapp-test-app-token";
pub const SIGNING_SECRET: &str = "test-signing-secret";
pub const CHANNEL: &str = "C_APPROVALS";

/// Pinned so the binaries are downloaded once and then reused from `target/`.
const POSTGRES_VERSION: &str = "=17.12.0";

// ── Postgres ────────────────────────────────────────────────────────────────

/// A private Postgres server for one test. Stopped and deleted when dropped.
pub struct TestDb {
    server: PostgreSQL,
    pub url: String,
}

impl TestDb {
    pub async fn start() -> TestDb {
        // Installing the binaries is not safe to do from several tests at once.
        static INSTALL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

        let settings = SettingsBuilder::new()
            .version(VersionReq::parse(POSTGRES_VERSION).unwrap())
            .installation_dir(PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("postgresql"))
            .host("127.0.0.1")
            .temporary(true)
            .timeout(Some(Duration::from_secs(120)))
            .build();
        let mut server = PostgreSQL::new(settings);
        {
            let _installing = INSTALL.lock().await;
            server.setup().await.expect("failed to install the test Postgres");
        }
        server.start().await.expect("failed to start the test Postgres");
        server.create_database("hcp").await.expect("failed to create the test database");
        let url = server.settings().url("hcp");
        TestDb { server, url }
    }
}

// ── Mock Slack ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Call {
    pub method: String,
    pub authorization: String,
    pub body: Value,
}

#[derive(Default)]
struct MockState {
    calls: Mutex<Vec<Call>>,
    fail_posts: AtomicBool,
    posted: AtomicU64,
    socket_url: Mutex<Option<String>>,
}

/// Stands in for `https://slack.com/api` and for interaction `response_url`s.
#[derive(Clone)]
pub struct MockSlack {
    pub api_base: String,
    pub response_url: String,
    state: Arc<MockState>,
}

impl MockSlack {
    pub async fn start() -> MockSlack {
        let state = Arc::new(MockState::default());
        let router = Router::new()
            .route("/api/{method}", post(mock_api))
            .route("/response", post(mock_response_url))
            .with_state(state.clone());
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        MockSlack { api_base: format!("http://{addr}/api"), response_url: format!("http://{addr}/response"), state }
    }

    /// Makes `chat.postMessage` fail the way Slack does (`ok: false`), or work again.
    pub fn fail_posts(&self, fail: bool) {
        self.state.fail_posts.store(fail, Ordering::SeqCst);
    }

    pub fn calls(&self, method: &str) -> Vec<Call> {
        self.state.calls.lock().unwrap().iter().filter(|call| call.method == method).cloned().collect()
    }

    /// Waits until at least `count` calls to `method` have been made, and returns them.
    pub async fn wait_calls(&self, method: &str, count: usize) -> Vec<Call> {
        let found = eventually(|| async {
            let calls = self.calls(method);
            (calls.len() >= count).then_some(calls)
        })
        .await;
        found.unwrap_or_else(|| panic!("expected {count} {method} call(s), saw {}", self.calls(method).len()))
    }
}

async fn mock_api(
    State(state): State<Arc<MockState>>,
    Path(method): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::Json<Value> {
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let authorization = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
    state.calls.lock().unwrap().push(Call { method: method.clone(), authorization, body: body.clone() });

    axum::Json(match method.as_str() {
        "auth.test" => json!({ "ok": true, "user": "hcp-bot" }),
        "chat.postMessage" if state.fail_posts.load(Ordering::SeqCst) => {
            json!({ "ok": false, "error": "channel_not_found" })
        }
        "chat.postMessage" => {
            let n = state.posted.fetch_add(1, Ordering::SeqCst) + 1;
            json!({ "ok": true, "channel": body["channel"], "ts": format!("1700000000.{n:06}") })
        }
        "chat.update" | "views.open" => json!({ "ok": true }),
        "apps.connections.open" => match state.socket_url.lock().unwrap().clone() {
            Some(url) => json!({ "ok": true, "url": url }),
            None => json!({ "ok": false, "error": "not_allowed_token_type" }),
        },
        _ => json!({ "ok": false, "error": "unknown_method" }),
    })
}

async fn mock_response_url(State(state): State<Arc<MockState>>, body: Bytes) -> StatusCode {
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    state.calls.lock().unwrap().push(Call { method: "response_url".to_string(), authorization: String::new(), body });
    StatusCode::OK
}

/// The Slack end of a Socket Mode connection.
pub struct MockSocket {
    to_client: mpsc::UnboundedSender<Value>,
    received: Arc<Mutex<Vec<Value>>>,
    connections: Arc<AtomicUsize>,
}

impl MockSocket {
    pub async fn start(slack: &MockSlack) -> MockSocket {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        *slack.state.socket_url.lock().unwrap() = Some(format!("ws://{}", listener.local_addr().unwrap()));

        let (to_client, mut outgoing) = mpsc::unbounded_channel::<Value>();
        let received = Arc::new(Mutex::new(Vec::new()));
        let connections = Arc::new(AtomicUsize::new(0));
        let (frames, count) = (received.clone(), connections.clone());

        tokio::spawn(async move {
            loop {
                let Ok((tcp, _)) = listener.accept().await else { return };
                let Ok(mut socket) = tokio_tungstenite::accept_async(tcp).await else { continue };
                count.fetch_add(1, Ordering::SeqCst);
                let _ = socket.send(Message::text(json!({ "type": "hello" }).to_string())).await;
                loop {
                    tokio::select! {
                        Some(envelope) = outgoing.recv() => {
                            if socket.send(Message::text(envelope.to_string())).await.is_err() {
                                break;
                            }
                        }
                        frame = socket.next() => match frame {
                            Some(Ok(Message::Text(text))) => {
                                frames.lock().unwrap().push(serde_json::from_str(text.as_str()).unwrap());
                            }
                            Some(Ok(_)) => {}
                            _ => break,
                        },
                    }
                }
            }
        });
        MockSocket { to_client, received, connections }
    }

    pub fn send(&self, envelope: Value) {
        self.to_client.send(envelope).unwrap();
    }

    pub fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    /// Waits for the client's acknowledgement of an envelope.
    pub async fn wait_ack(&self, envelope_id: &str) -> Value {
        eventually(|| async {
            self.received.lock().unwrap().iter().find(|frame| frame["envelope_id"] == envelope_id).cloned()
        })
        .await
        .unwrap_or_else(|| panic!("no acknowledgement for envelope {envelope_id}"))
    }
}

// ── Harness ─────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SlackMode {
    /// Slack not configured at all.
    Off,
    /// Interactions arrive as signed HTTP requests.
    Http,
    /// Interactions arrive over Socket Mode.
    Socket,
}

pub struct Options {
    pub slack: SlackMode,
    pub responders: Vec<(&'static str, &'static str)>,
    pub notify_channel: Option<&'static str>,
    /// SQL run against the empty database before the server starts.
    pub seed_sql: Option<String>,
}

impl Default for Options {
    fn default() -> Self {
        Options { slack: SlackMode::Http, responders: Vec::new(), notify_channel: None, seed_sql: None }
    }
}

pub struct Harness {
    pub app: App,
    pub slack: MockSlack,
    pub socket: Option<MockSocket>,
    pub vault: Arc<Vault>,
    pub http: reqwest::Client,
    pub db_url: String,
    vault_dir: TempDir,
    // Dropped last: the server above is still using it.
    db: TestDb,
}

impl Harness {
    pub async fn start(options: Options) -> Harness {
        let db = TestDb::start().await;
        if let Some(sql) = options.seed_sql {
            let pool = sqlx::PgPool::connect(&db.url).await.unwrap();
            sqlx::raw_sql(sqlx::AssertSqlSafe(sql)).execute(&pool).await.unwrap();
            pool.close().await;
        }
        let slack = MockSlack::start().await;
        let socket = match options.slack {
            SlackMode::Socket => Some(MockSocket::start(&slack).await),
            _ => None,
        };

        let vault_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(vault_dir.path().join(".obsidian")).unwrap();
        let vault_config = VaultConfig { path: vault_dir.path().to_path_buf(), folder: "HCP".to_string() };
        let vault = Arc::new(Vault::open(&vault_config).unwrap());

        let slack_config = (options.slack != SlackMode::Off).then(|| SlackConfig {
            bot_token: BOT_TOKEN.to_string(),
            app_token: (options.slack == SlackMode::Socket).then(|| APP_TOKEN.to_string()),
            signing_secret: (options.slack == SlackMode::Http).then(|| SIGNING_SECRET.to_string()),
            channel: CHANNEL.to_string(),
            notify_channel: options.notify_channel.map(str::to_string),
            responders: options
                .responders
                .iter()
                .map(|(r, u)| (r.to_string(), u.to_string()))
                .collect::<HashMap<_, _>>(),
            api_base: slack.api_base.clone(),
        });
        let config = Config {
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 0,
            database_url: db.url.clone(),
            db_max_connections: 5,
            vault: Some(vault_config),
            slack: slack_config,
            // Tests drive the scheduler by hand, with explicit clocks.
            poll_interval: Duration::from_secs(3600),
            routing_retry_after: Duration::from_secs(30),
        };
        let app = App::start(config, Some(vault.clone())).await.expect("failed to start HCP");

        Harness { app, slack, socket, vault, http: reqwest::Client::new(), db_url: db.url.clone(), vault_dir, db }
    }

    /// Creates an API key and returns its secret.
    pub async fn key(&self, agent_id: &str, scopes: &[&str]) -> String {
        let scopes: Vec<String> = scopes.iter().map(|s| s.to_string()).collect();
        self.app.engine.store().create_api_key(agent_id, "test key", &scopes).await.unwrap().1
    }

    pub async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        key: Option<&str>,
        body: Option<&Value>,
    ) -> (StatusCode, Value) {
        let mut request = self.http.request(method, format!("{}{path}", self.app.base_url()));
        if let Some(key) = key {
            request = request.bearer_auth(key);
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().await.unwrap();
        let status = response.status();
        let text = response.text().await.unwrap();
        (status, serde_json::from_str(&text).unwrap_or(Value::String(text)))
    }

    pub async fn post(&self, path: &str, key: &str, body: &Value) -> (StatusCode, Value) {
        self.request(reqwest::Method::POST, path, Some(key), Some(body)).await
    }

    pub async fn get(&self, path: &str, key: &str) -> (StatusCode, Value) {
        self.request(reqwest::Method::GET, path, Some(key), None).await
    }

    pub async fn delete(&self, path: &str, key: &str) -> (StatusCode, Value) {
        self.request(reqwest::Method::DELETE, path, Some(key), None).await
    }

    /// Submits a request and returns its ID.
    pub async fn submit(&self, key: &str, body: &Value) -> String {
        let (status, created) = self.post("/v1/requests", key, body).await;
        assert_eq!(status, StatusCode::CREATED, "submit failed: {created}");
        assert_eq!(created["state"], "SUBMITTED");
        created["request_id"].as_str().unwrap().to_string()
    }

    /// Reads a request straight from the engine, without the side effects of the agent API.
    pub async fn peek(&self, request_id: &str) -> CoordinationRequest {
        self.app.engine.get(request_id).await.unwrap()
    }

    pub async fn wait_state(&self, request_id: &str, wanted: CrState) -> CoordinationRequest {
        let reached = eventually(|| async {
            let cr = self.peek(request_id).await;
            (cr.state == wanted).then_some(cr)
        })
        .await;
        match reached {
            Some(cr) => cr,
            None => panic!("request never reached {wanted}; it is {}", self.peek(request_id).await.state),
        }
    }

    /// Sends an interaction to `/slack/interactions` signed the way Slack signs it.
    pub async fn slack_interaction(&self, payload: &Value) -> (StatusCode, Value) {
        let body = serde_urlencoded::to_string([("payload", payload.to_string())]).unwrap();
        let timestamp = Utc::now().timestamp().to_string();
        let signature = hcp::slack::sign(SIGNING_SECRET, &timestamp, body.as_bytes());
        self.raw_slack_interaction(body, &timestamp, &signature).await
    }

    pub async fn raw_slack_interaction(&self, body: String, timestamp: &str, signature: &str) -> (StatusCode, Value) {
        let response = self
            .http
            .post(format!("{}/slack/interactions", self.app.base_url()))
            .header("content-type", "application/x-www-form-urlencoded")
            .header("x-slack-request-timestamp", timestamp)
            .header("x-slack-signature", signature)
            .body(body)
            .send()
            .await
            .unwrap();
        let status = response.status();
        let text = response.text().await.unwrap();
        (status, serde_json::from_str(&text).unwrap_or(Value::String(text)))
    }

    /// A button click, as Slack reports it.
    pub fn click(&self, action_id: &str, request_id: &str, user_id: &str, user_name: &str) -> Value {
        json!({
            "type": "block_actions",
            "user": { "id": user_id, "username": user_name },
            "trigger_id": "trigger-123",
            "response_url": self.slack.response_url,
            "actions": [{ "action_id": action_id, "value": request_id }],
        })
    }

    /// A submission of the free-text response modal.
    pub fn modal_submission(&self, request_id: &str, user_id: &str, user_name: &str, text: &str) -> Value {
        json!({
            "type": "view_submission",
            "user": { "id": user_id, "username": user_name },
            "view": {
                "callback_id": hcp::slack::MODAL_CALLBACK,
                "private_metadata": request_id,
                "state": { "values": { hcp::slack::MODAL_BLOCK: { hcp::slack::MODAL_INPUT: { "value": text } } } },
            },
        })
    }

    pub async fn audit_types(&self, key: &str, request_id: &str) -> Vec<String> {
        let (status, body) = self.get(&format!("/v1/audit?request_id={request_id}"), key).await;
        assert_eq!(status, StatusCode::OK);
        body["events"].as_array().unwrap().iter().map(|e| e["event_type"].as_str().unwrap().to_string()).collect()
    }

    /// Waits for every queued audit event to be written to the vault.
    pub async fn wait_vault(&self) {
        let store = self.app.engine.store();
        eventually(|| async { (store.outbox_len().await.unwrap() == 0).then_some(()) })
            .await
            .expect("the vault outbox never drained");
    }

    pub async fn request_note(&self, request_id: &str) -> String {
        self.wait_vault().await;
        std::fs::read_to_string(self.vault.request_note_path(&self.peek(request_id).await)).unwrap()
    }

    /// The contents of every daily audit log, concatenated.
    pub async fn audit_log(&self) -> String {
        self.wait_vault().await;
        let mut paths: Vec<_> =
            std::fs::read_dir(self.vault.root().join("Log")).unwrap().map(|entry| entry.unwrap().path()).collect();
        paths.sort();
        paths.iter().map(|path| std::fs::read_to_string(path).unwrap()).collect()
    }
}

/// A valid request body; override fields with [`with`].
pub fn approval() -> Value {
    json!({
        "intent": "APPROVAL",
        "urgency": "HIGH",
        "context_package": {
            "summary": "Deploy v2.1.0 to production",
            "detail": "Includes the new auth flow and 3 bug fixes.",
            "metadata": { "version": "2.1.0" },
        },
        "timeout_policy": { "timeout_seconds": 600, "fallback": "AUTO_REJECT" },
        "routing_hints": { "responder_id": "ops-lead" },
    })
}

/// Returns `base` with `pointer` (a JSON pointer) set to `value`.
pub fn with(mut base: Value, pointer: &str, value: Value) -> Value {
    let (parent, key) = pointer.rsplit_once('/').unwrap();
    let parent = if parent.is_empty() { &mut base } else { base.pointer_mut(parent).unwrap() };
    parent[key] = value;
    base
}

/// Polls `check` until it yields a value, for up to ten seconds.
pub async fn eventually<T, F, Fut>(check: F) -> Option<T>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Option<T>>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(value) = check().await {
            return Some(value);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Text of every block in a Slack message body, for content assertions.
pub fn block_text(body: &Value) -> String {
    serde_json::to_string(&body["blocks"]).unwrap()
}

pub fn action_ids(body: &Value) -> Vec<String> {
    body["blocks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| block["type"] == "actions")
        .flat_map(|block| block["elements"].as_array().cloned().unwrap_or_default())
        .map(|element| element["action_id"].as_str().unwrap().to_string())
        .collect()
}
