//! Slack is where humans see and answer coordination requests.
//!
//! This module builds the messages and talks to the Slack Web API. Button clicks and modal
//! submissions come back either over Socket Mode ([`socket`]) or over signed HTTP requests
//! (`POST /slack/interactions`); both end up in [`interactions::handle`].

pub mod interactions;
pub mod socket;

use std::time::Duration;

use hmac::{Hmac, KeyInit, Mac};
use serde_json::{Value, json};
use sha2::Sha256;

use crate::config::SlackConfig;
use crate::domain::{CoordinationRequest, Fallback, Intent, State, Urgency};

pub const ACTION_APPROVE: &str = "hcp_approve";
pub const ACTION_REJECT: &str = "hcp_reject";
pub const ACTION_RESPOND: &str = "hcp_respond";
pub const MODAL_CALLBACK: &str = "hcp_respond_modal";
pub const MODAL_BLOCK: &str = "hcp_response";
pub const MODAL_INPUT: &str = "hcp_response_text";

/// Slack rejects section text over 3000 characters; leave room for labels and escaping.
const MAX_SECTION_CHARS: usize = 2500;
/// Requests older than this are refused, as Slack recommends, to defeat replays.
const SIGNATURE_MAX_AGE_SECONDS: i64 = 300;

#[derive(Debug, thiserror::Error)]
pub enum SlackError {
    #[error("Slack API {method} failed: {error}")]
    Api { method: String, error: String },
    #[error("Slack API {method} returned HTTP {status}")]
    Http { method: String, status: u16 },
    #[error("Slack API {method} returned an unexpected body")]
    Malformed { method: String },
    #[error("Slack request failed: {0}")]
    Transport(#[from] reqwest::Error),
}

#[derive(Clone)]
pub struct SlackClient {
    http: reqwest::Client,
    config: SlackConfig,
}

impl SlackClient {
    pub fn new(config: SlackConfig) -> Result<Self, SlackError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .user_agent(concat!("hcp/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self { http, config })
    }

    pub fn config(&self) -> &SlackConfig {
        &self.config
    }

    async fn call(&self, method: &str, token: &str, body: Option<&Value>) -> Result<Value, SlackError> {
        let request = self.http.post(format!("{}/{method}", self.config.api_base)).bearer_auth(token);
        let request = match body {
            Some(body) => request.json(body),
            None => request.header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded"),
        };
        let response = request.send().await?;
        if !response.status().is_success() {
            return Err(SlackError::Http { method: method.to_string(), status: response.status().as_u16() });
        }
        let reply: Value = response.json().await?;
        if reply.get("ok").and_then(Value::as_bool) == Some(true) {
            Ok(reply)
        } else {
            let error = reply.get("error").and_then(Value::as_str).unwrap_or("unknown_error").to_string();
            Err(SlackError::Api { method: method.to_string(), error })
        }
    }

    /// Checks the bot token and returns the bot's user name.
    pub async fn auth_test(&self) -> Result<String, SlackError> {
        let reply = self.call("auth.test", &self.config.bot_token, None).await?;
        Ok(reply.get("user").and_then(Value::as_str).unwrap_or("unknown").to_string())
    }

    /// Posts the request to a channel and returns the message timestamp.
    pub async fn post_request(&self, cr: &CoordinationRequest, channel: &str) -> Result<String, SlackError> {
        let body = json!({
            "channel": channel,
            "text": fallback_text(cr),
            "blocks": request_blocks(cr, &self.config),
            "unfurl_links": false,
            "unfurl_media": false,
        });
        let reply = self.call("chat.postMessage", &self.config.bot_token, Some(&body)).await?;
        reply
            .get("ts")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| SlackError::Malformed { method: "chat.postMessage".to_string() })
    }

    /// Replaces a posted request with its outcome, removing the buttons.
    pub async fn show_outcome(
        &self,
        cr: &CoordinationRequest,
        channel: &str,
        ts: &str,
        outcome: &str,
    ) -> Result<(), SlackError> {
        let body = json!({
            "channel": channel,
            "ts": ts,
            "text": fallback_text(cr),
            "blocks": resolved_blocks(cr, &self.config, outcome),
        });
        self.call("chat.update", &self.config.bot_token, Some(&body)).await.map(drop)
    }

    /// Opens the free-text response modal for a request.
    pub async fn open_response_modal(&self, trigger_id: &str, cr: &CoordinationRequest) -> Result<(), SlackError> {
        let body = json!({ "trigger_id": trigger_id, "view": response_modal(cr) });
        self.call("views.open", &self.config.bot_token, Some(&body)).await.map(drop)
    }

    /// Tells only the person who clicked why their click did nothing.
    pub async fn reply_ephemeral(&self, response_url: &str, text: &str) -> Result<(), SlackError> {
        let body = json!({ "response_type": "ephemeral", "replace_original": false, "text": text });
        let response = self.http.post(response_url).json(&body).send().await?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(SlackError::Http { method: "response_url".to_string(), status: response.status().as_u16() })
        }
    }

    /// Asks Slack for a Socket Mode WebSocket URL.
    pub async fn open_socket_url(&self) -> Result<String, SlackError> {
        let method = "apps.connections.open";
        let token = self.config.app_token.as_deref().unwrap_or_default();
        let reply = self.call(method, token, None).await?;
        reply
            .get("url")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| SlackError::Malformed { method: method.to_string() })
    }
}

// ── Message building ────────────────────────────────────────────────────────

/// Escapes text for Slack `mrkdwn`. Agent-supplied text passes through here so that it
/// cannot mention `@channel`, ping people or disguise links.
pub fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        let mut cut: String = text.chars().take(max_chars).collect();
        cut.push('…');
        cut
    }
}

/// An inline code span holding untrusted text.
fn code(text: &str) -> String {
    format!("`{}`", escape(&text.replace('`', "'")))
}

fn title_case(shouting: &str) -> String {
    let lower = shouting.replace('_', " ").to_lowercase();
    let mut chars = lower.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => lower,
    }
}

fn urgency_emoji(urgency: Urgency) -> &'static str {
    match urgency {
        Urgency::Critical => ":rotating_light:",
        Urgency::High => ":warning:",
        Urgency::Medium => ":large_blue_circle:",
        Urgency::Low => ":white_circle:",
    }
}

fn responder_label(cr: &CoordinationRequest, config: &SlackConfig) -> String {
    match config.responders.get(&cr.responder_id) {
        Some(user) => format!("<@{user}>"),
        None => code(&cr.responder_id),
    }
}

fn on_timeout(cr: &CoordinationRequest) -> String {
    match cr.timeout_policy.fallback {
        Fallback::AutoApprove => "it is *approved automatically*".to_string(),
        Fallback::AutoReject => "it is rejected automatically".to_string(),
        Fallback::Escalate => match &cr.timeout_policy.escalation_responder_id {
            Some(target) if *target != cr.responder_id => format!("it escalates to {}", code(target)),
            _ => "it times out".to_string(),
        },
        Fallback::Block | Fallback::Fail | Fallback::Skip => "it times out".to_string(),
    }
}

/// Notification text, also what screen readers and push notifications show.
pub fn fallback_text(cr: &CoordinationRequest) -> String {
    escape(&truncate(&format!("[HCP] {} request from {}: {}", cr.intent, cr.agent_id, cr.context_package.summary), 300))
}

fn section(text: String) -> Value {
    json!({ "type": "section", "text": { "type": "mrkdwn", "text": text } })
}

fn button(label: &str, style: Option<&str>, action_id: &str, request_id: &str) -> Value {
    let mut button = json!({
        "type": "button",
        "text": { "type": "plain_text", "text": label },
        "action_id": action_id,
        "value": request_id,
    });
    if let Some(style) = style {
        button["style"] = json!(style);
    }
    button
}

/// The part of a request message that stays the same before and after it is answered.
fn description_blocks(cr: &CoordinationRequest, config: &SlackConfig) -> Vec<Value> {
    let mut facts = format!(
        "{} *Urgency:* {}\n*Agent:* {}\n*For:* {}",
        urgency_emoji(cr.urgency),
        cr.urgency,
        code(&cr.agent_id),
        responder_label(cr, config),
    );
    if cr.intent.needs_response() {
        facts.push_str(&format!(
            "\n*Respond by:* <!date^{}^{{date_short_pretty}} {{time}}|{}> — after that {}",
            cr.timeout_at.timestamp(),
            crate::domain::timestamp::format(&cr.timeout_at),
            on_timeout(cr),
        ));
    }

    let mut blocks = vec![
        json!({
            "type": "header",
            "text": { "type": "plain_text", "text": format!("{} request", title_case(cr.intent.as_str())) },
        }),
        section(facts),
        section(format!("*Summary*\n{}", escape(&truncate(&cr.context_package.summary, MAX_SECTION_CHARS)))),
    ];
    if let Some(detail) = cr.context_package.detail.as_deref().filter(|d| !d.trim().is_empty()) {
        blocks.push(section(format!("*Detail*\n{}", escape(&truncate(detail, MAX_SECTION_CHARS)))));
    }
    blocks.push(json!({
        "type": "context",
        "elements": [{ "type": "mrkdwn", "text": format!("ID {}", code(&cr.request_id)) }],
    }));
    blocks
}

/// A request awaiting a human: approvals get Approve/Reject, other intents get a button
/// that opens a free-text modal, notifications get no controls.
pub fn request_blocks(cr: &CoordinationRequest, config: &SlackConfig) -> Vec<Value> {
    let mut blocks = description_blocks(cr, config);
    let elements = match cr.intent {
        Intent::Notification => return blocks,
        Intent::Approval => vec![
            button("Approve", Some("primary"), ACTION_APPROVE, &cr.request_id),
            button("Reject", Some("danger"), ACTION_REJECT, &cr.request_id),
        ],
        _ => vec![button("Respond", Some("primary"), ACTION_RESPOND, &cr.request_id)],
    };
    blocks.push(json!({
        "type": "actions",
        "block_id": format!("hcp_actions_{}", cr.request_id),
        "elements": elements,
    }));
    blocks
}

/// A request that no longer accepts answers: the description plus what happened to it.
pub fn resolved_blocks(cr: &CoordinationRequest, config: &SlackConfig, outcome: &str) -> Vec<Value> {
    let mut blocks = description_blocks(cr, config);
    blocks.push(section(outcome.to_string()));
    blocks
}

/// One line describing how a request ended, or `None` while it is still open.
/// `slack_user` is the Slack user ID of whoever answered, when the answer came from Slack.
pub fn outcome_line(cr: &CoordinationRequest, slack_user: Option<&str>) -> Option<String> {
    let who = || match slack_user {
        Some(user) => format!("<@{user}>"),
        None => code(cr.responded_by.as_deref().unwrap_or("unknown")),
    };
    let automatic = cr.response_data.as_ref().and_then(|d| d.get("auto")).and_then(Value::as_bool) == Some(true);

    Some(match cr.state {
        State::Responded | State::Delivered => match (cr.decision(), automatic) {
            (Some("approved"), true) => {
                ":alarm_clock: *Approved automatically* — nobody responded before the deadline".to_string()
            }
            (Some("rejected"), true) => {
                ":alarm_clock: *Rejected automatically* — nobody responded before the deadline".to_string()
            }
            (Some("approved"), false) => format!(":white_check_mark: *Approved* by {}", who()),
            (Some("rejected"), false) => format!(":x: *Rejected* by {}", who()),
            _ => {
                let mut line = format!(":speech_balloon: *Answered* by {}", who());
                if let Some(text) = cr.response_data.as_ref().and_then(|d| d.get("text")).and_then(Value::as_str) {
                    let quoted = escape(&truncate(text, 500)).replace('\n', "\n> ");
                    line.push_str(&format!("\n> {quoted}"));
                }
                line
            }
        },
        State::TimedOut => ":alarm_clock: *Timed out* — nobody responded before the deadline".to_string(),
        State::Cancelled => ":no_entry_sign: *Cancelled* by the agent".to_string(),
        State::Escalated => {
            format!(":arrow_up: *Escalated* to {} — nobody responded before the deadline", code(&cr.responder_id))
        }
        State::Submitted | State::Routing | State::PendingResponse => return None,
    })
}

/// The modal used to answer anything that is not a yes/no approval.
pub fn response_modal(cr: &CoordinationRequest) -> Value {
    json!({
        "type": "modal",
        "callback_id": MODAL_CALLBACK,
        "private_metadata": cr.request_id,
        "title": { "type": "plain_text", "text": "Respond to agent" },
        "submit": { "type": "plain_text", "text": "Send" },
        "close": { "type": "plain_text", "text": "Cancel" },
        "blocks": [
            section(format!(
                "*{}* request from {}\n{}",
                title_case(cr.intent.as_str()),
                code(&cr.agent_id),
                escape(&truncate(&cr.context_package.summary, MAX_SECTION_CHARS)),
            )),
            {
                "type": "input",
                "block_id": MODAL_BLOCK,
                "label": { "type": "plain_text", "text": "Your response" },
                "element": {
                    "type": "plain_text_input",
                    "action_id": MODAL_INPUT,
                    "multiline": true,
                    "max_length": 3000,
                },
            },
        ],
    })
}

// ── Request signing ─────────────────────────────────────────────────────────

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum SignatureError {
    #[error("missing or malformed Slack signature headers")]
    Malformed,
    #[error("Slack request timestamp is too old")]
    Stale,
    #[error("Slack signature does not match")]
    Mismatch,
}

/// Verifies Slack's `X-Slack-Signature` over `v0:{timestamp}:{body}`.
pub fn verify_signature(
    signing_secret: &str,
    timestamp: &str,
    body: &[u8],
    signature: &str,
    now_unix: i64,
) -> Result<(), SignatureError> {
    let sent_at: i64 = timestamp.parse().map_err(|_| SignatureError::Malformed)?;
    if (now_unix - sent_at).abs() > SIGNATURE_MAX_AGE_SECONDS {
        return Err(SignatureError::Stale);
    }
    let expected =
        signature.strip_prefix("v0=").and_then(|digest| hex::decode(digest).ok()).ok_or(SignatureError::Malformed)?;

    let mut mac = Hmac::<Sha256>::new_from_slice(signing_secret.as_bytes()).map_err(|_| SignatureError::Malformed)?;
    mac.update(b"v0:");
    mac.update(timestamp.as_bytes());
    mac.update(b":");
    mac.update(body);
    mac.verify_slice(&expected).map_err(|_| SignatureError::Mismatch)
}

/// Computes the signature Slack would send. Used by tests and local tooling.
pub fn sign(signing_secret: &str, timestamp: &str, body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(signing_secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(b"v0:");
    mac.update(timestamp.as_bytes());
    mac.update(b":");
    mac.update(body);
    format!("v0={}", hex::encode(mac.finalize().into_bytes()))
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::HashMap;

    use chrono::{TimeZone, Utc};

    use super::*;
    use crate::domain::{Channel, ContextPackage, RoutingHints, TimeoutPolicy};

    pub(crate) fn config() -> SlackConfig {
        SlackConfig {
            bot_token: "xoxb-test".to_string(),
            app_token: None,
            signing_secret: Some("secret".to_string()),
            channel: "C_APPROVALS".to_string(),
            notify_channel: None,
            responders: HashMap::from([("isaac".to_string(), "U_ISAAC".to_string())]),
            api_base: "http://127.0.0.1:1".to_string(),
        }
    }

    pub(crate) fn request(intent: Intent) -> CoordinationRequest {
        let at = Utc.with_ymd_and_hms(2026, 1, 15, 12, 0, 0).unwrap();
        CoordinationRequest {
            request_id: "01TESTREQUEST".to_string(),
            agent_id: "claude-code".to_string(),
            intent,
            urgency: Urgency::High,
            state: State::PendingResponse,
            context_package: ContextPackage {
                summary: "Deploy v2.1.0 to production".to_string(),
                detail: None,
                metadata: None,
                attachments: None,
            },
            response_schema: None,
            timeout_policy: TimeoutPolicy {
                timeout_seconds: 600,
                fallback: Fallback::AutoReject,
                escalation_responder_id: None,
            },
            routing_hints: RoutingHints {
                responder_id: "ops-lead".to_string(),
                channel: Channel::Slack,
                slack_channel_id: None,
            },
            trace_id: None,
            idempotency_key: None,
            responder_id: "ops-lead".to_string(),
            response_data: None,
            responded_by: None,
            responded_at: None,
            submitted_at: at,
            updated_at: at,
            timeout_at: at + chrono::Duration::seconds(600),
            delivered_at: None,
        }
    }

    fn action_ids(blocks: &[Value]) -> Vec<String> {
        blocks
            .iter()
            .filter(|b| b["type"] == "actions")
            .flat_map(|b| b["elements"].as_array().cloned().unwrap_or_default())
            .map(|e| e["action_id"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn approvals_get_approve_and_reject_buttons_carrying_the_request_id() {
        let blocks = request_blocks(&request(Intent::Approval), &config());
        assert_eq!(action_ids(&blocks), [ACTION_APPROVE, ACTION_REJECT]);
        let actions = blocks.iter().find(|b| b["type"] == "actions").unwrap();
        assert_eq!(actions["elements"][0]["value"], "01TESTREQUEST");
    }

    #[test]
    fn other_intents_get_a_respond_button_and_notifications_get_none() {
        assert_eq!(action_ids(&request_blocks(&request(Intent::Clarification), &config())), [ACTION_RESPOND]);
        assert!(action_ids(&request_blocks(&request(Intent::Notification), &config())).is_empty());
    }

    #[test]
    fn agent_text_cannot_inject_mentions_or_links() {
        let mut cr = request(Intent::Approval);
        cr.context_package.summary = "<!channel> click <https://evil.example|here> & approve".to_string();
        cr.context_package.detail = Some("<@U123> said yes".to_string());
        let rendered = serde_json::to_string(&request_blocks(&cr, &config())).unwrap();
        assert!(!rendered.contains("<!channel>"));
        assert!(!rendered.contains("<https://evil.example"));
        assert!(!rendered.contains("<@U123>"));
        assert!(rendered.contains("&lt;!channel&gt;"));
        assert!(rendered.contains("&amp; approve"));
        assert!(!fallback_text(&cr).contains("<!channel>"));
    }

    #[test]
    fn mapped_responders_are_mentioned_and_unmapped_ones_are_shown_as_code() {
        let mut cr = request(Intent::Approval);
        let unmapped = serde_json::to_string(&request_blocks(&cr, &config())).unwrap();
        assert!(unmapped.contains("`ops-lead`"));

        cr.responder_id = "isaac".to_string();
        let mapped = serde_json::to_string(&request_blocks(&cr, &config())).unwrap();
        assert!(mapped.contains("<@U_ISAAC>"));
    }

    #[test]
    fn long_text_is_truncated_below_the_slack_limit() {
        let mut cr = request(Intent::Approval);
        cr.context_package.detail = Some("x".repeat(10_000));
        for block in request_blocks(&cr, &config()) {
            if let Some(text) = block["text"]["text"].as_str() {
                assert!(text.chars().count() < 3000);
            }
        }
    }

    #[test]
    fn resolved_messages_have_no_buttons() {
        let mut cr = request(Intent::Approval);
        cr.state = State::Responded;
        cr.response_data = Some(json!({ "decision": "approved" }));
        cr.responded_by = Some("slack:isaac".to_string());
        let line = outcome_line(&cr, Some("U_ISAAC")).unwrap();
        assert_eq!(line, ":white_check_mark: *Approved* by <@U_ISAAC>");
        assert!(action_ids(&resolved_blocks(&cr, &config(), &line)).is_empty());
    }

    #[test]
    fn outcome_lines_distinguish_automatic_decisions() {
        let mut cr = request(Intent::Approval);
        assert!(outcome_line(&cr, None).is_none());

        cr.state = State::Responded;
        cr.response_data = Some(json!({ "decision": "rejected", "auto": true }));
        assert!(outcome_line(&cr, None).unwrap().contains("Rejected automatically"));

        cr.response_data = Some(json!({ "text": "use <b>postgres</b>" }));
        cr.responded_by = Some("ops-lead".to_string());
        let answered = outcome_line(&cr, None).unwrap();
        assert!(answered.contains("*Answered* by `ops-lead`"));
        assert!(answered.contains("&lt;b&gt;postgres"));

        cr.state = State::TimedOut;
        assert!(outcome_line(&cr, None).unwrap().contains("Timed out"));
        cr.state = State::Cancelled;
        assert!(outcome_line(&cr, None).unwrap().contains("Cancelled"));
    }

    #[test]
    fn modal_carries_the_request_id_and_the_input_ids() {
        let modal = response_modal(&request(Intent::Decision));
        assert_eq!(modal["callback_id"], MODAL_CALLBACK);
        assert_eq!(modal["private_metadata"], "01TESTREQUEST");
        assert_eq!(modal["blocks"][1]["block_id"], MODAL_BLOCK);
        assert_eq!(modal["blocks"][1]["element"]["action_id"], MODAL_INPUT);
        assert!(modal["title"]["text"].as_str().unwrap().len() <= 24);
    }

    /// The worked example from Slack's "Verifying requests from Slack" documentation.
    #[test]
    fn signature_matches_slacks_documented_example() {
        let secret = "8f742231b10e8888abcd99yyyzzz85a5";
        let timestamp = "1531420618";
        let body = b"token=xyzz0WbapA4vBCDEFasx0q6G&team_id=T1DC2JH3J&team_domain=testteamnow&channel_id=G8PSS9T3V&channel_name=foobar&user_id=U2CERLKJA&user_name=roadrunner&command=%2Fwebhook-collect&text=&response_url=https%3A%2F%2Fhooks.slack.com%2Fcommands%2FT1DC2JH3J%2F397700885554%2F96rGlfmibIGlgcZRskXaIFfN&trigger_id=398738663015.47445629121.803a0bc887a14d10d2c447fce8b6703c";
        let signature = "v0=a2114d57b48eac39b9ad189dd8316235a7b4a8d21a10bd27519666489c69b503";

        assert_eq!(sign(secret, timestamp, body), signature);
        assert_eq!(verify_signature(secret, timestamp, body, signature, 1531420618), Ok(()));
    }

    #[test]
    fn signature_rejects_tampering_staleness_and_garbage() {
        let (secret, timestamp, body) = ("secret", "1700000000", b"payload=%7B%7D".as_slice());
        let signature = sign(secret, timestamp, body);
        let now = 1700000010;

        assert_eq!(verify_signature(secret, timestamp, body, &signature, now), Ok(()));
        assert_eq!(verify_signature(secret, timestamp, b"payload=x", &signature, now), Err(SignatureError::Mismatch));
        assert_eq!(verify_signature("other", timestamp, body, &signature, now), Err(SignatureError::Mismatch));
        assert_eq!(verify_signature(secret, timestamp, body, &signature, now + 301), Err(SignatureError::Stale));
        assert_eq!(verify_signature(secret, "soon", body, &signature, now), Err(SignatureError::Malformed));
        assert_eq!(verify_signature(secret, timestamp, body, "v0=nothex", now), Err(SignatureError::Malformed));
        assert_eq!(verify_signature(secret, timestamp, body, "", now), Err(SignatureError::Malformed));
    }
}
