//! Talks to the HCP server with the dashboard's admin key. The key never leaves this
//! process: the web view only ever receives the JSON answers.

use std::sync::RwLock;
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::Value;

pub const DEFAULT_BASE_URL: &str = "http://127.0.0.1:3100";

pub struct Server {
    http: reqwest::Client,
    /// No overall timeout: the event stream stays open indefinitely.
    stream: reqwest::Client,
    pub base_url: String,
    key: RwLock<Option<String>>,
}

/// Why a call failed, phrased for the dashboard.
pub fn describe(err: &reqwest::Error, base_url: &str) -> String {
    if err.is_connect() || err.is_timeout() {
        format!("The HCP server is not answering at {base_url}. Is the service running?")
    } else {
        format!("Could not reach the HCP server: {err}")
    }
}

impl Server {
    pub fn new(base_url: String, key: Option<String>) -> Self {
        let agent = concat!("hcp-desktop/", env!("CARGO_PKG_VERSION"));
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .user_agent(agent)
            .build()
            .expect("the HTTP client has a valid configuration");
        let stream = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .user_agent(agent)
            .build()
            .expect("the HTTP client has a valid configuration");
        Self { http, stream, base_url: base_url.trim_end_matches('/').to_string(), key: RwLock::new(key) }
    }

    pub fn key(&self) -> Option<String> {
        self.key.read().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }

    pub fn set_key(&self, key: Option<String>) {
        *self.key.write().unwrap_or_else(|poisoned| poisoned.into_inner()) = key;
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    /// The health check, which needs no key.
    pub async fn health(&self) -> Result<Value, String> {
        let response = self.http.get(self.url("/health")).send().await.map_err(|e| describe(&e, &self.base_url))?;
        response.json().await.map_err(|e| format!("The server's health check was not JSON: {e}"))
    }

    async fn call(&self, request: reqwest::RequestBuilder, key: &str) -> Result<Value, String> {
        let response = request.bearer_auth(key).send().await.map_err(|e| describe(&e, &self.base_url))?;
        let status = response.status();
        let body: Value = response.json().await.unwrap_or(Value::Null);
        match status.as_u16() {
            200..=299 => Ok(body),
            401 => Err("The dashboard's API key was rejected; it may have been revoked. Set it up again.".to_string()),
            403 => Err("The dashboard's API key lacks the admin scope. Set it up again.".to_string()),
            404 => Err("Not found".to_string()),
            _ => Err(body.get("error").and_then(Value::as_str).unwrap_or("The server returned an error").to_string()),
        }
    }

    fn required_key(&self) -> Result<String, String> {
        self.key().ok_or_else(|| "The dashboard is not set up with an API key yet.".to_string())
    }

    pub async fn get(&self, path: &str) -> Result<Value, String> {
        let key = self.required_key()?;
        self.call(self.http.get(self.url(path)), &key).await
    }

    pub async fn post(&self, path: &str) -> Result<Value, String> {
        let key = self.required_key()?;
        self.call(self.http.post(self.url(path)), &key).await
    }

    /// Checks a key before it is saved: it must work and carry the admin scope.
    pub async fn check_key(&self, key: &str) -> Result<(), String> {
        self.call(self.http.get(self.url("/v1/status")), key).await.map(drop)
    }

    /// Follows `/v1/events` and calls `on_change` for each state change, until the stream
    /// ends or fails.
    pub async fn follow_events(&self, mut on_change: impl FnMut(Value)) -> Result<(), String> {
        let key = self.required_key()?;
        let response = self
            .stream
            .get(self.url("/v1/events"))
            .bearer_auth(key)
            .send()
            .await
            .map_err(|e| describe(&e, &self.base_url))?;
        if !response.status().is_success() {
            return Err(format!("The event stream was refused (HTTP {})", response.status().as_u16()));
        }
        let mut bytes = response.bytes_stream();
        let mut buffer = String::new();
        while let Some(chunk) = bytes.next().await {
            let chunk = chunk.map_err(|e| describe(&e, &self.base_url))?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));
            // Server-sent events are separated by a blank line.
            while let Some(end) = buffer.find("\n\n") {
                let block: String = buffer.drain(..end + 2).collect();
                let (mut event, mut data) = ("message", String::new());
                for line in block.lines() {
                    if let Some(name) = line.strip_prefix("event:") {
                        event = if name.trim() == "state_change" { "state_change" } else { "other" };
                    } else if let Some(part) = line.strip_prefix("data:") {
                        data.push_str(part.trim_start());
                    }
                }
                if event == "state_change"
                    && let Ok(change) = serde_json::from_str(&data)
                {
                    on_change(change);
                }
            }
        }
        Ok(())
    }
}

/// Percent-encodes a URL component.
pub fn encode(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn components_are_percent_encoded() {
        assert_eq!(encode("/Users/me/My Vault"), "%2FUsers%2Fme%2FMy%20Vault");
        assert_eq!(encode("01ABC-x_y.z~"), "01ABC-x_y.z~");
        assert_eq!(encode("é&="), "%C3%A9%26%3D");
    }
}
