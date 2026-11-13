//! Domain types: the coordination request, its lifecycle states and the audit event.
//!
//! The JSON shapes here are wire-compatible with the original TypeScript service, so
//! existing agents, the Node SDK and rows already in Postgres keep working.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Declares an enum whose wire and database form is a fixed string.
macro_rules! string_enum {
    ($(#[$meta:meta])* $name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub enum $name {
            $(#[serde(rename = $text)] $variant),+
        }

        impl $name {
            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $text),+ }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl std::str::FromStr for $name {
            type Err = String;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s {
                    $($text => Ok(Self::$variant),)+
                    other => Err(format!("unknown {}: {other:?}", stringify!($name))),
                }
            }
        }
    };
}

string_enum!(
    /// What the agent needs from the human.
    Intent {
        Approval => "APPROVAL",
        Clarification => "CLARIFICATION",
        Escalation => "ESCALATION",
        Notification => "NOTIFICATION",
        Decision => "DECISION",
        Review => "REVIEW",
        Input => "INPUT",
    }
);

string_enum!(Urgency {
    Critical => "CRITICAL",
    High => "HIGH",
    Medium => "MEDIUM",
    Low => "LOW",
});

string_enum!(
    /// Lifecycle state of a coordination request.
    State {
        Submitted => "SUBMITTED",
        Routing => "ROUTING",
        PendingResponse => "PENDING_RESPONSE",
        Responded => "RESPONDED",
        Delivered => "DELIVERED",
        Escalated => "ESCALATED",
        TimedOut => "TIMED_OUT",
        Cancelled => "CANCELLED",
    }
);

string_enum!(
    /// What happens when nobody responds before the deadline.
    Fallback {
        AutoApprove => "AUTO_APPROVE",
        AutoReject => "AUTO_REJECT",
        Escalate => "ESCALATE",
        Block => "BLOCK",
        Fail => "FAIL",
        Skip => "SKIP",
    }
);

string_enum!(ActorType {
    Agent => "AGENT",
    Human => "HUMAN",
    System => "SYSTEM",
});

string_enum!(
    /// Routing channel requested by the agent. `portal` is still accepted on the wire for
    /// compatibility, but there is no portal any more: requests that need a human go to Slack.
    Channel {
        Portal => "portal",
        Slack => "slack",
    }
);

string_enum!(ResponseKind {
    Choice => "choice",
    Text => "text",
    Structured => "structured",
});

impl Intent {
    /// Notifications are one-way; every other intent waits on a human.
    pub fn needs_response(self) -> bool {
        self != Intent::Notification
    }
}

impl State {
    /// The lifecycle graph. `ROUTING -> TIMED_OUT` is an addition over the original service:
    /// it is how a request that could never be delivered to a human ends, instead of being
    /// parked in `PENDING_RESPONSE` where an `AUTO_APPROVE` fallback could approve it unseen.
    pub fn can_transition_to(self, to: State) -> bool {
        use State::*;
        matches!(
            (self, to),
            (Submitted, Routing | Cancelled)
                | (Routing, PendingResponse | Escalated | TimedOut | Cancelled)
                | (PendingResponse, Responded | Escalated | TimedOut | Cancelled)
                | (Responded, Delivered)
                | (Escalated, Routing | TimedOut | Cancelled)
        )
    }

    pub fn is_cancellable(self) -> bool {
        use State::*;
        matches!(self, Submitted | Routing | PendingResponse | Escalated)
    }

    /// States an agent polling for a result should stop on.
    pub fn is_terminal(self) -> bool {
        use State::*;
        matches!(self, Delivered | TimedOut | Cancelled)
    }

    /// The audit event type recorded when a request enters this state.
    pub fn audit_event(self) -> String {
        format!("CR_{}", self.as_str())
    }
}

/// Audit event types that are not state transitions.
pub mod event {
    pub const SLACK_NOTIFIED: &str = "SLACK_NOTIFIED";
    pub const SLACK_NOTIFY_FAILED: &str = "SLACK_NOTIFY_FAILED";
    pub const SLACK_INTERACTION: &str = "SLACK_INTERACTION";
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Attachment {
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextPackage {
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachments: Option<Vec<Attachment>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimeoutPolicy {
    pub timeout_seconds: u64,
    pub fallback: Fallback,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub escalation_responder_id: Option<String>,
}

impl TimeoutPolicy {
    /// The deadline for a window opening at `from`, or `None` if it is not representable.
    pub fn deadline_from(&self, from: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let seconds = i64::try_from(self.timeout_seconds).ok()?;
        from.checked_add_signed(Duration::try_seconds(seconds)?)
    }
}

fn default_channel() -> Channel {
    Channel::Slack
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoutingHints {
    pub responder_id: String,
    #[serde(default = "default_channel")]
    pub channel: Channel,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slack_channel_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponseOption {
    pub key: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponseSchema {
    #[serde(rename = "type")]
    pub kind: ResponseKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<ResponseOption>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub json_schema: Option<Map<String, Value>>,
}

/// Body of `POST /v1/requests`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateRequest {
    pub intent: Intent,
    pub urgency: Urgency,
    pub context_package: ContextPackage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_schema: Option<ResponseSchema>,
    pub timeout_policy: TimeoutPolicy,
    pub routing_hints: RoutingHints,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

impl CreateRequest {
    /// Checks the constraints the type system does not express. Returns every problem found.
    pub fn validate(&self, now: DateTime<Utc>) -> Result<(), Vec<String>> {
        let mut problems = Vec::new();
        if self.context_package.summary.trim().is_empty() {
            problems.push("context_package.summary must not be empty".to_string());
        }
        if self.routing_hints.responder_id.trim().is_empty() {
            problems.push("routing_hints.responder_id must not be empty".to_string());
        }
        if self.timeout_policy.timeout_seconds == 0 {
            problems.push("timeout_policy.timeout_seconds must be a positive integer".to_string());
        } else if self.timeout_policy.deadline_from(now).is_none() {
            problems.push("timeout_policy.timeout_seconds is too large".to_string());
        }
        if matches!(&self.idempotency_key, Some(key) if key.is_empty()) {
            problems.push("idempotency_key must not be empty when present".to_string());
        }
        if problems.is_empty() { Ok(()) } else { Err(problems) }
    }
}

/// Body of `POST /v1/requests/{id}/respond`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubmitResponse {
    pub response_data: Map<String, Value>,
    pub responded_by: String,
}

impl SubmitResponse {
    pub fn validate(&self) -> Result<(), Vec<String>> {
        if self.responded_by.trim().is_empty() {
            Err(vec!["responded_by must not be empty".to_string()])
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CoordinationRequest {
    pub request_id: String,
    pub agent_id: String,
    pub intent: Intent,
    pub urgency: Urgency,
    pub state: State,
    pub context_package: ContextPackage,
    pub response_schema: Option<ResponseSchema>,
    pub timeout_policy: TimeoutPolicy,
    pub routing_hints: RoutingHints,
    pub trace_id: Option<String>,
    pub idempotency_key: Option<String>,
    pub responder_id: String,
    pub response_data: Option<Value>,
    pub responded_by: Option<String>,
    #[serde(with = "timestamp::option")]
    pub responded_at: Option<DateTime<Utc>>,
    #[serde(with = "timestamp")]
    pub submitted_at: DateTime<Utc>,
    #[serde(with = "timestamp")]
    pub updated_at: DateTime<Utc>,
    #[serde(with = "timestamp")]
    pub timeout_at: DateTime<Utc>,
    #[serde(with = "timestamp::option")]
    pub delivered_at: Option<DateTime<Utc>>,
}

impl CoordinationRequest {
    /// The `decision` field of the response, when the response carries one.
    pub fn decision(&self) -> Option<&str> {
        self.response_data.as_ref()?.get("decision")?.as_str()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditEvent {
    pub event_id: String,
    pub request_id: String,
    pub event_type: String,
    pub actor: String,
    pub actor_type: ActorType,
    pub payload: Value,
    #[serde(with = "timestamp")]
    pub created_at: DateTime<Utc>,
}

/// A state change, as broadcast to SSE subscribers.
#[derive(Debug, Clone, Serialize)]
pub struct StateChange {
    pub request_id: String,
    pub state: State,
    pub agent_id: String,
}

pub fn new_id() -> String {
    ulid::Ulid::generate().to_string()
}

/// Millisecond-precision UTC timestamps (`2026-01-15T12:00:00.000Z`), the format the
/// original service emitted.
pub mod timestamp {
    use chrono::{DateTime, SecondsFormat, Utc};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn format(value: &DateTime<Utc>) -> String {
        value.to_rfc3339_opts(SecondsFormat::Millis, true)
    }

    pub fn serialize<S: Serializer>(value: &DateTime<Utc>, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format(value))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<DateTime<Utc>, D::Error> {
        DateTime::<Utc>::deserialize(d)
    }

    pub mod option {
        use super::*;

        pub fn serialize<S: Serializer>(value: &Option<DateTime<Utc>>, s: S) -> Result<S::Ok, S::Error> {
            match value {
                Some(value) => s.serialize_str(&format(value)),
                None => s.serialize_none(),
            }
        }

        pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<DateTime<Utc>>, D::Error> {
            Option::<DateTime<Utc>>::deserialize(d)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ALL_STATES: [State; 8] = [
        State::Submitted,
        State::Routing,
        State::PendingResponse,
        State::Responded,
        State::Delivered,
        State::Escalated,
        State::TimedOut,
        State::Cancelled,
    ];

    fn sample() -> Value {
        json!({
            "intent": "APPROVAL",
            "urgency": "HIGH",
            "context_package": { "summary": "Deploy v2.1.0 to production" },
            "timeout_policy": { "timeout_seconds": 600, "fallback": "AUTO_REJECT" },
            "routing_hints": { "responder_id": "ops-lead" }
        })
    }

    #[test]
    fn happy_path_transitions_are_allowed() {
        assert!(State::Submitted.can_transition_to(State::Routing));
        assert!(State::Routing.can_transition_to(State::PendingResponse));
        assert!(State::PendingResponse.can_transition_to(State::Responded));
        assert!(State::Responded.can_transition_to(State::Delivered));
        assert!(State::PendingResponse.can_transition_to(State::Escalated));
        assert!(State::Escalated.can_transition_to(State::Routing));
    }

    #[test]
    fn invalid_transitions_are_rejected() {
        assert!(!State::Submitted.can_transition_to(State::Delivered));
        assert!(!State::Submitted.can_transition_to(State::Responded));
        assert!(!State::Escalated.can_transition_to(State::Responded));
        assert!(!State::Responded.can_transition_to(State::Cancelled));
    }

    #[test]
    fn terminal_states_have_no_exits_and_are_not_cancellable() {
        for from in ALL_STATES.into_iter().filter(|s| s.is_terminal()) {
            assert!(!from.is_cancellable(), "{from} must not be cancellable");
            for to in ALL_STATES {
                assert!(!from.can_transition_to(to), "{from} -> {to} must be rejected");
            }
        }
    }

    #[test]
    fn cancellable_states_are_exactly_those_that_can_reach_cancelled() {
        for state in ALL_STATES {
            assert_eq!(state.is_cancellable(), state.can_transition_to(State::Cancelled));
        }
    }

    #[test]
    fn an_undelivered_request_can_time_out_but_never_be_answered() {
        assert!(State::Routing.can_transition_to(State::TimedOut));
        assert!(!State::Routing.can_transition_to(State::Responded));
    }

    #[test]
    fn state_strings_round_trip() {
        for state in ALL_STATES {
            assert_eq!(state.as_str().parse::<State>().unwrap(), state);
            assert_eq!(serde_json::to_value(state).unwrap(), json!(state.as_str()));
        }
        assert!("NOPE".parse::<State>().is_err());
        assert_eq!(State::PendingResponse.audit_event(), "CR_PENDING_RESPONSE");
    }

    #[test]
    fn create_request_defaults_the_channel_to_slack() {
        let request: CreateRequest = serde_json::from_value(sample()).unwrap();
        assert_eq!(request.routing_hints.channel, Channel::Slack);
        assert!(request.validate(Utc::now()).is_ok());
    }

    #[test]
    fn create_request_still_accepts_the_portal_channel() {
        let mut body = sample();
        body["routing_hints"]["channel"] = json!("portal");
        let request: CreateRequest = serde_json::from_value(body).unwrap();
        assert_eq!(request.routing_hints.channel, Channel::Portal);
    }

    #[test]
    fn create_request_rejects_unknown_enums_and_missing_fields() {
        let mut body = sample();
        body["intent"] = json!("INVALID");
        assert!(serde_json::from_value::<CreateRequest>(body).is_err());
        assert!(serde_json::from_value::<CreateRequest>(json!({ "intent": "APPROVAL" })).is_err());
    }

    #[test]
    fn validation_reports_every_problem() {
        let mut body = sample();
        body["context_package"]["summary"] = json!("  ");
        body["routing_hints"]["responder_id"] = json!("");
        body["timeout_policy"]["timeout_seconds"] = json!(0);
        let request: CreateRequest = serde_json::from_value(body).unwrap();
        assert_eq!(request.validate(Utc::now()).unwrap_err().len(), 3);
    }

    #[test]
    fn validation_rejects_an_unrepresentable_deadline() {
        let mut body = sample();
        body["timeout_policy"]["timeout_seconds"] = json!(u64::MAX);
        let request: CreateRequest = serde_json::from_value(body).unwrap();
        assert!(request.validate(Utc::now()).is_err());
    }

    #[test]
    fn timestamps_serialize_with_millisecond_precision() {
        let at = DateTime::parse_from_rfc3339("2026-01-15T12:00:00.123456Z").unwrap().to_utc();
        assert_eq!(timestamp::format(&at), "2026-01-15T12:00:00.123Z");
    }

    #[test]
    fn optional_context_fields_are_omitted_when_absent() {
        let request: CreateRequest = serde_json::from_value(sample()).unwrap();
        let out = serde_json::to_value(&request.context_package).unwrap();
        assert_eq!(out, json!({ "summary": "Deploy v2.1.0 to production" }));
    }
}
