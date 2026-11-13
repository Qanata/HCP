//! HTTP client for the HCP API, used by the agent-side commands (`hcp mcp`, `hcp hook`).

use std::time::Duration;

use chrono::Utc;
use serde_json::Value;

use crate::domain::CoordinationRequest;

pub const DEFAULT_BASE_URL: &str = "http://127.0.0.1:3100";
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(2);
/// Clients may abort a tool call that shows no sign of life; Claude Code does so after
/// 30 minutes for stdio servers. A progress notification this often keeps a long wait alive.
pub const DEFAULT_PROGRESS_INTERVAL: Duration = Duration::from_secs(30);

/// How long past a request's deadline to keep polling before giving up on it. Servers
/// settle an expired request when it is read; the allowance covers ones that only do so
/// from their scheduler.
const OVERDUE_GRACE: Duration = Duration::from_secs(15);

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("HCP API error {status}: {body}")]
    Api { status: u16, body: String },
    #[error("cannot reach HCP at {base_url}: {source}")]
    Unreachable {
        base_url: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("HCP returned a response this client does not understand: {0}")]
    Malformed(String),
}

impl ClientError {
    /// Whether trying again later could succeed: the server was unreachable or failing,
    /// as opposed to having refused the request.
    pub fn is_transient(&self) -> bool {
        match self {
            ClientError::Unreachable { .. } => true,
            ClientError::Api { status, .. } => *status >= 500,
            ClientError::Malformed(_) => false,
        }
    }
}

/// How waiting for a request ended.
#[derive(Debug)]
pub enum Waited {
    /// The request reached a terminal state.
    Settled(Box<CoordinationRequest>),
    /// The deadline passed and the server still had not settled it.
    Overdue(Box<CoordinationRequest>),
}

#[derive(Clone)]
pub struct HcpClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl HcpClient {
    pub fn new(base_url: &str, api_key: &str, timeout: Duration) -> Result<Self, reqwest::Error> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .user_agent(concat!("hcp/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self { http, base_url: base_url.trim_end_matches('/').to_string(), api_key: api_key.to_string() })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> Result<String, ClientError> {
        let unreachable = |source| ClientError::Unreachable { base_url: self.base_url.clone(), source };
        let response = request.bearer_auth(&self.api_key).send().await.map_err(unreachable)?;
        let status = response.status();
        let body = response.text().await.map_err(unreachable)?;
        if status.is_success() { Ok(body) } else { Err(ClientError::Api { status: status.as_u16(), body }) }
    }

    fn request_from(body: &str) -> Result<CoordinationRequest, ClientError> {
        serde_json::from_str(body).map_err(|err| ClientError::Malformed(err.to_string()))
    }

    /// Submits a coordination request. The body is sent as given, so the server's
    /// validation errors come back to the caller unchanged.
    pub async fn submit(&self, body: &Value) -> Result<CoordinationRequest, ClientError> {
        let text = self.send(self.http.post(format!("{}/v1/requests", self.base_url)).json(body)).await?;
        Self::request_from(&text)
    }

    /// Reads a request. For the agent that owns it, reading an answered request takes
    /// delivery of the answer.
    pub async fn get(&self, request_id: &str) -> Result<CoordinationRequest, ClientError> {
        let text = self.send(self.http.get(self.request_url(request_id))).await?;
        Self::request_from(&text)
    }

    pub async fn cancel(&self, request_id: &str) -> Result<(), ClientError> {
        self.send(self.http.delete(self.request_url(request_id))).await.map(drop)
    }

    fn request_url(&self, request_id: &str) -> String {
        // Request IDs are ULIDs; anything else is rejected rather than spliced into a path.
        let safe: String = request_id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
        format!("{}/v1/requests/{safe}", self.base_url)
    }

    /// Polls until the request is answered, cancelled or timed out. Follows the server's
    /// deadline rather than a local timer, so an escalation that extends the deadline
    /// extends the wait. A server that is briefly unreachable is retried until the last
    /// known deadline; a refusal (401, 404) ends the wait at once.
    pub async fn wait(&self, submitted: CoordinationRequest, poll: Duration) -> Result<Waited, ClientError> {
        let request_id = submitted.request_id.clone();
        let mut latest = submitted;
        loop {
            if latest.state.is_terminal() {
                return Ok(Waited::Settled(Box::new(latest)));
            }
            let overdue = Utc::now() > latest.timeout_at + OVERDUE_GRACE;
            if overdue {
                return Ok(Waited::Overdue(Box::new(latest)));
            }
            tokio::time::sleep(poll).await;
            match self.get(&request_id).await {
                Ok(current) => latest = current,
                Err(err) if err.is_transient() && Utc::now() <= latest.timeout_at + OVERDUE_GRACE => {}
                Err(err) => return Err(err),
            }
        }
    }
}

/// Settings the agent-side commands (`hcp mcp`, `hcp hook`) read from the environment.
/// Deliberately not `Debug`: it holds the API key.
#[derive(Clone)]
pub struct AgentEnv {
    pub base_url: String,
    pub api_key: Option<String>,
    /// Who requests are addressed to when the caller does not say.
    pub responder: String,
    /// Slack channel to name on this agent's requests; unset uses the server's default.
    pub slack_channel: Option<String>,
    pub poll: Duration,
    /// How often a blocked MCP tool call reports that it is still waiting.
    pub progress: Duration,
}

impl AgentEnv {
    pub fn from_env() -> Self {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Self {
        let get = |key: &str| lookup(key).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let millis = |key: &str, default: Duration| {
            get(key)
                .and_then(|raw| raw.parse::<u64>().ok())
                .filter(|ms| *ms > 0)
                .map(Duration::from_millis)
                .unwrap_or(default)
        };
        let poll = millis("HCP_POLL_INTERVAL_MS", DEFAULT_POLL_INTERVAL);
        let progress = millis("HCP_PROGRESS_INTERVAL_MS", DEFAULT_PROGRESS_INTERVAL);
        Self {
            base_url: get("HCP_BASE_URL").unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            api_key: get("HCP_API_KEY"),
            responder: get("HCP_RESPONDER").unwrap_or_else(|| "default".to_string()),
            slack_channel: get("HCP_SLACK_CHANNEL"),
            poll,
            progress,
        }
    }

    pub fn require_api_key(&self) -> anyhow::Result<&str> {
        self.api_key
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("HCP_API_KEY is not set (create one with `hcp keys create <agent>`)"))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn env(pairs: &[(&str, &str)]) -> AgentEnv {
        let map: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        AgentEnv::from_lookup(|key| map.get(key).cloned())
    }

    #[test]
    fn defaults_point_at_the_local_server() {
        let env = env(&[]);
        assert_eq!(env.base_url, DEFAULT_BASE_URL);
        assert_eq!(env.responder, "default");
        assert_eq!((env.poll, env.progress), (DEFAULT_POLL_INTERVAL, DEFAULT_PROGRESS_INTERVAL));
        assert!(env.api_key.is_none() && env.slack_channel.is_none());
        assert!(env.require_api_key().is_err());
    }

    #[test]
    fn values_are_read_and_blank_ones_ignored() {
        let env = env(&[
            ("HCP_BASE_URL", "http://studio.local:3100"),
            ("HCP_API_KEY", "hcp_abc"),
            ("HCP_RESPONDER", "isaac"),
            ("HCP_SLACK_CHANNEL", "  "),
            ("HCP_POLL_INTERVAL_MS", "250"),
        ]);
        assert_eq!(env.base_url, "http://studio.local:3100");
        assert_eq!(env.require_api_key().unwrap(), "hcp_abc");
        assert_eq!(env.responder, "isaac");
        assert!(env.slack_channel.is_none());
        assert_eq!(env.poll, Duration::from_millis(250));
    }

    #[test]
    fn transient_errors_are_distinguished_from_refusals() {
        assert!(ClientError::Api { status: 503, body: String::new() }.is_transient());
        assert!(!ClientError::Api { status: 404, body: String::new() }.is_transient());
        assert!(!ClientError::Api { status: 401, body: String::new() }.is_transient());
    }

    #[test]
    fn request_ids_cannot_change_the_path() {
        let client = HcpClient::new("http://h/", "k", Duration::from_secs(1)).unwrap();
        assert_eq!(client.request_url("01ABC"), "http://h/v1/requests/01ABC");
        assert_eq!(client.request_url("../audit?x=1"), "http://h/v1/requests/auditx1");
    }
}
