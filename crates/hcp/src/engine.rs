//! The coordination engine: submitting, routing, answering, cancelling and timing out
//! requests. Every state change goes through [`Engine::apply`], which records the audit
//! event, tells SSE subscribers, wakes the vault writer and keeps Slack messages current.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use tokio::sync::{Notify, broadcast};
use tokio_util::sync::CancellationToken;

use crate::config::SlackConfig;
use crate::domain::{
    ActorType, Channel, CoordinationRequest, CreateRequest, Fallback, State, StateChange, event, new_id,
};
use crate::slack::{self, SlackClient};
use crate::store::{Inserted, Store, StoreError, Transition, Updates};

const SYSTEM: &str = "system";

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("request not found")]
    NotFound,
    #[error("idempotency key is already in use")]
    IdempotencyKeyTaken,
    #[error("cannot {action} a request in state {state}")]
    WrongState { action: &'static str, state: State },
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl From<sqlx::Error> for EngineError {
    fn from(err: sqlx::Error) -> Self {
        Self::Store(StoreError::Database(err))
    }
}

/// Where a request goes once it is routed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Destination {
    /// Posted to this Slack channel.
    Slack(String),
    /// Slack is not configured: the request can only be answered through the API.
    ApiOnly,
    /// A notification with no Slack destination: recorded in the audit log only.
    LogOnly,
}

/// Decides where a request is delivered. Anything that needs a human goes to Slack — the
/// channel named in the routing hints, or else the configured default. Notifications are
/// posted only when the request or the configuration names a channel for them.
pub fn destination(cr: &CoordinationRequest, slack: Option<&SlackConfig>) -> Destination {
    let Some(slack) = slack else {
        return Destination::ApiOnly;
    };
    let hinted = match cr.routing_hints.channel {
        Channel::Slack => cr.routing_hints.slack_channel_id.clone().filter(|id| !id.trim().is_empty()),
        Channel::Portal => None,
    };
    if cr.intent.needs_response() {
        Destination::Slack(hinted.unwrap_or_else(|| slack.channel.clone()))
    } else {
        match hinted.or_else(|| slack.notify_channel.clone()) {
            Some(channel) => Destination::Slack(channel),
            None => Destination::LogOnly,
        }
    }
}

/// Who is answering a request.
#[derive(Debug, Clone)]
pub struct Answer<'a> {
    pub response_data: Value,
    /// Audit actor, e.g. `slack:U123` or the responder name given to the API.
    pub actor: &'a str,
    /// Stored on the request as `responded_by`.
    pub responded_by: &'a str,
    /// Slack user ID, when the answer came from Slack.
    pub slack_user: Option<&'a str>,
    /// Extra fields for the audit event.
    pub audit: Value,
}

struct Inner {
    store: Store,
    slack: Option<SlackClient>,
    changes: broadcast::Sender<StateChange>,
    vault_wake: Arc<Notify>,
    routing_retry_after: Duration,
}

/// Cheap to clone; all clones share the same state.
#[derive(Clone)]
pub struct Engine {
    inner: Arc<Inner>,
}

impl Engine {
    pub fn new(store: Store, slack: Option<SlackClient>, routing_retry_after: Duration) -> Self {
        let (changes, _) = broadcast::channel(1024);
        Self {
            inner: Arc::new(Inner { store, slack, changes, vault_wake: Arc::new(Notify::new()), routing_retry_after }),
        }
    }

    pub fn store(&self) -> &Store {
        &self.inner.store
    }

    pub fn slack(&self) -> Option<&SlackClient> {
        self.inner.slack.as_ref()
    }

    /// State changes as they happen, for SSE.
    pub fn subscribe(&self) -> broadcast::Receiver<StateChange> {
        self.inner.changes.subscribe()
    }

    /// Notified whenever there may be new audit events for the vault.
    pub fn vault_wake(&self) -> Arc<Notify> {
        self.inner.vault_wake.clone()
    }

    fn announce(&self, cr: &CoordinationRequest) {
        // No subscribers is not an error.
        let _ = self.inner.changes.send(StateChange {
            request_id: cr.request_id.clone(),
            state: cr.state,
            agent_id: cr.agent_id.clone(),
        });
        self.inner.vault_wake.notify_one();
    }

    /// Performs a transition and its side effects. `slack_user` credits a Slack user in the
    /// updated message when a human answered from Slack.
    async fn apply(&self, t: Transition<'_>, slack_user: Option<&str>) -> Result<CoordinationRequest, StoreError> {
        let updated = self.inner.store.transition(t).await?;
        tracing::debug!(request_id = %updated.request_id, state = %updated.state, "state changed");
        self.announce(&updated);
        if matches!(updated.state, State::Responded | State::TimedOut | State::Cancelled) {
            let (engine, cr, user) = (self.clone(), updated.clone(), slack_user.map(str::to_string));
            tokio::spawn(async move { engine.refresh_slack_message(&cr, user.as_deref()).await });
        }
        Ok(updated)
    }

    async fn record(&self, request_id: &str, event_type: &str, actor: &str, actor_type: ActorType, payload: Value) {
        if let Err(err) = self.inner.store.append_audit(request_id, event_type, actor, actor_type, payload).await {
            tracing::error!(request_id, event_type, error = %err, "failed to record audit event");
        }
        self.inner.vault_wake.notify_one();
    }

    // ── Agent-facing operations ─────────────────────────────────────────────

    /// Stores a new request and starts routing it in the background. Returns the request
    /// and whether it was newly created (`false` for an idempotent replay).
    pub async fn submit(
        &self,
        agent_id: &str,
        input: CreateRequest,
    ) -> Result<(CoordinationRequest, bool), EngineError> {
        let now = Utc::now();
        let timeout_at = input.timeout_policy.deadline_from(now).unwrap_or(DateTime::<Utc>::MAX_UTC);
        let cr = CoordinationRequest {
            request_id: new_id(),
            agent_id: agent_id.to_string(),
            intent: input.intent,
            urgency: input.urgency,
            state: State::Submitted,
            context_package: input.context_package,
            response_schema: input.response_schema,
            timeout_policy: input.timeout_policy,
            responder_id: input.routing_hints.responder_id.clone(),
            routing_hints: input.routing_hints,
            trace_id: input.trace_id,
            idempotency_key: input.idempotency_key,
            response_data: None,
            responded_by: None,
            responded_at: None,
            submitted_at: now,
            updated_at: now,
            timeout_at,
            delivered_at: None,
        };

        match self.inner.store.insert_request(&cr).await? {
            Inserted::Existing(existing) if existing.agent_id == agent_id => Ok((*existing, false)),
            // Keys are unique across agents in the schema; never hand one agent another's request.
            Inserted::Existing(_) => Err(EngineError::IdempotencyKeyTaken),
            Inserted::Created => {
                self.announce(&cr);
                let (engine, submitted) = (self.clone(), cr.clone());
                tokio::spawn(async move { engine.route(submitted).await });
                Ok((cr, true))
            }
        }
    }

    pub async fn get(&self, request_id: &str) -> Result<CoordinationRequest, EngineError> {
        self.inner.store.get_request(request_id).await?.ok_or(EngineError::NotFound)
    }

    /// Marks an answered request as delivered to its agent. Losing the race to another
    /// reader is fine: the current row is returned either way.
    pub async fn mark_delivered(&self, cr: CoordinationRequest) -> Result<CoordinationRequest, EngineError> {
        if cr.state != State::Responded {
            return Ok(cr);
        }
        let delivered = self
            .apply(
                Transition {
                    request_id: &cr.request_id,
                    from: State::Responded,
                    to: State::Delivered,
                    actor: &cr.agent_id,
                    actor_type: ActorType::Agent,
                    payload: json!({}),
                    updates: Updates { delivered_at: Some(Utc::now()), ..Updates::default() },
                },
                None,
            )
            .await;
        match delivered {
            Ok(updated) => Ok(updated),
            Err(StoreError::Conflict { .. }) => self.get(&cr.request_id).await,
            Err(err) => Err(err.into()),
        }
    }

    pub async fn cancel(
        &self,
        cr: &CoordinationRequest,
        actor: &str,
        actor_type: ActorType,
    ) -> Result<CoordinationRequest, EngineError> {
        if !cr.state.is_cancellable() {
            return Err(EngineError::WrongState { action: "cancel", state: cr.state });
        }
        let cancelled = self
            .apply(
                Transition {
                    request_id: &cr.request_id,
                    from: cr.state,
                    to: State::Cancelled,
                    actor,
                    actor_type,
                    payload: json!({}),
                    updates: Updates::default(),
                },
                None,
            )
            .await;
        match cancelled {
            Ok(updated) => Ok(updated),
            Err(StoreError::Conflict { .. }) => {
                Err(EngineError::WrongState { action: "cancel", state: self.get(&cr.request_id).await?.state })
            }
            Err(err) => Err(err.into()),
        }
    }

    /// Records a human's answer. Only a request that is waiting on a human accepts one.
    pub async fn respond(&self, request_id: &str, answer: Answer<'_>) -> Result<CoordinationRequest, EngineError> {
        let cr = self.get(request_id).await?;
        if cr.state != State::PendingResponse {
            return Err(EngineError::WrongState { action: "respond to", state: cr.state });
        }
        let mut payload = json!({ "response_data": answer.response_data });
        if let (Some(payload), Some(extra)) = (payload.as_object_mut(), answer.audit.as_object()) {
            payload.extend(extra.clone());
        }
        let responded = self
            .apply(
                Transition {
                    request_id,
                    from: State::PendingResponse,
                    to: State::Responded,
                    actor: answer.actor,
                    actor_type: ActorType::Human,
                    payload,
                    updates: Updates {
                        response_data: Some(answer.response_data),
                        responded_by: Some(answer.responded_by.to_string()),
                        responded_at: Some(Utc::now()),
                        ..Updates::default()
                    },
                },
                answer.slack_user,
            )
            .await;
        match responded {
            Ok(updated) => Ok(updated),
            Err(StoreError::Conflict { .. }) => {
                Err(EngineError::WrongState { action: "respond to", state: self.get(request_id).await?.state })
            }
            Err(err) => Err(err.into()),
        }
    }

    // ── Routing ─────────────────────────────────────────────────────────────

    /// Takes a submitted request to the human. The `SUBMITTED -> ROUTING` transition doubles
    /// as the claim: whoever wins it is the only one delivering.
    pub async fn route(&self, cr: CoordinationRequest) {
        let routing = self
            .apply(
                Transition {
                    request_id: &cr.request_id,
                    from: State::Submitted,
                    to: State::Routing,
                    actor: SYSTEM,
                    actor_type: ActorType::System,
                    payload: json!({ "responder_id": cr.responder_id }),
                    updates: Updates::default(),
                },
                None,
            )
            .await;
        match routing {
            Ok(routing) => self.deliver(routing).await,
            // Cancelled, or another worker got there first.
            Err(StoreError::Conflict { .. }) => {}
            Err(err) => tracing::error!(request_id = %cr.request_id, error = %err, "failed to start routing"),
        }
    }

    /// Delivers a request that is in `ROUTING`. It only becomes `PENDING_RESPONSE` once it
    /// has actually been delivered; a failed Slack post leaves it in `ROUTING` for the
    /// scheduler to retry, so a request nobody saw can never be auto-approved.
    async fn deliver(&self, cr: CoordinationRequest) {
        let destination = destination(&cr, self.slack().map(SlackClient::config));
        let (payload, posted) = match (&destination, self.slack()) {
            (Destination::Slack(channel), Some(slack)) => match slack.post_request(&cr, channel).await {
                Ok(ts) => {
                    self.record(
                        &cr.request_id,
                        event::SLACK_NOTIFIED,
                        SYSTEM,
                        ActorType::System,
                        json!({ "channel_id": channel, "ts": ts }),
                    )
                    .await;
                    let payload =
                        json!({ "channel": "slack", "responder_id": cr.responder_id, "slack_channel_id": channel });
                    (payload, true)
                }
                Err(err) => {
                    self.delivery_failed(&cr, channel, &err).await;
                    return;
                }
            },
            (Destination::LogOnly, _) => (json!({ "channel": "log", "responder_id": cr.responder_id }), false),
            _ => (json!({ "channel": "api", "responder_id": cr.responder_id }), false),
        };

        let pending = self
            .apply(
                Transition {
                    request_id: &cr.request_id,
                    from: State::Routing,
                    to: State::PendingResponse,
                    actor: SYSTEM,
                    actor_type: ActorType::System,
                    payload,
                    updates: Updates::default(),
                },
                None,
            )
            .await;
        match pending {
            Ok(_) => {}
            Err(StoreError::Conflict { .. }) => {
                // Cancelled while the message was being posted: make the message say so.
                if posted && let Ok(current) = self.get(&cr.request_id).await {
                    self.refresh_slack_message(&current, None).await;
                }
            }
            Err(err) => tracing::error!(request_id = %cr.request_id, error = %err, "failed to mark request pending"),
        }
    }

    async fn delivery_failed(&self, cr: &CoordinationRequest, channel: &str, err: &slack::SlackError) {
        tracing::warn!(request_id = %cr.request_id, channel, error = %err, "Slack delivery failed; will retry");
        // Retries happen every scheduler pass; record the failure once, not once per retry.
        match self.inner.store.has_event(&cr.request_id, event::SLACK_NOTIFY_FAILED).await {
            Ok(true) => {}
            Ok(false) => {
                let payload = json!({ "channel_id": channel, "error": err.to_string() });
                self.record(&cr.request_id, event::SLACK_NOTIFY_FAILED, SYSTEM, ActorType::System, payload).await;
            }
            Err(err) => tracing::error!(request_id = %cr.request_id, error = %err, "failed to read audit log"),
        }
    }

    /// Rewrites the request's Slack message to show how it ended and removes the buttons.
    async fn refresh_slack_message(&self, cr: &CoordinationRequest, slack_user: Option<&str>) {
        let (Some(slack), Some(outcome)) = (self.slack(), slack::outcome_line(cr, slack_user)) else {
            return;
        };
        match self.inner.store.slack_message(&cr.request_id).await {
            Ok(Some((channel, ts))) => {
                if let Err(err) = slack.show_outcome(cr, &channel, &ts, &outcome).await {
                    tracing::warn!(request_id = %cr.request_id, error = %err, "failed to update Slack message");
                }
            }
            Ok(None) => {}
            Err(err) => tracing::error!(request_id = %cr.request_id, error = %err, "failed to look up Slack message"),
        }
    }

    // ── Timeouts ────────────────────────────────────────────────────────────

    /// One scheduler pass: applies fallbacks to expired requests, then retries delivery of
    /// requests that are stuck before reaching a human. `now` is a parameter so tests can
    /// move time instead of sleeping.
    pub async fn tick(&self, now: DateTime<Utc>) {
        match self.inner.store.expired_requests(now).await {
            Ok(expired) => {
                for cr in expired {
                    self.expire(cr, now).await;
                }
            }
            Err(err) => tracing::error!(error = %err, "failed to load expired requests"),
        }

        let retry_after =
            chrono::Duration::from_std(self.inner.routing_retry_after).unwrap_or(chrono::Duration::zero());
        let stale_before = now - retry_after;
        match self.inner.store.stale_unrouted(now, stale_before).await {
            Ok(stale) => {
                for cr in stale {
                    self.resume(cr, stale_before).await;
                }
            }
            Err(err) => tracing::error!(error = %err, "failed to load unrouted requests"),
        }
    }

    /// Applies the timeout policy to one request whose deadline has passed, without waiting
    /// for the next scheduler pass. An agent polling for its answer therefore sees the
    /// outcome on its first read after the deadline. Returns the request as it now stands.
    pub async fn settle_if_expired(
        &self,
        cr: CoordinationRequest,
        now: DateTime<Utc>,
    ) -> Result<CoordinationRequest, EngineError> {
        let open = matches!(cr.state, State::Submitted | State::Routing | State::Escalated | State::PendingResponse);
        if !open || cr.timeout_at > now {
            return Ok(cr);
        }
        let request_id = cr.request_id.clone();
        self.expire(cr, now).await;
        self.get(&request_id).await
    }

    pub async fn run_scheduler(self, interval: Duration, shutdown: CancellationToken) {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = ticker.tick() => self.tick(Utc::now()).await,
            }
        }
    }

    async fn system_transition(
        &self,
        cr: &CoordinationRequest,
        to: State,
        payload: Value,
        updates: Updates,
    ) -> Option<CoordinationRequest> {
        let result = self
            .apply(
                Transition {
                    request_id: &cr.request_id,
                    from: cr.state,
                    to,
                    actor: SYSTEM,
                    actor_type: ActorType::System,
                    payload,
                    updates,
                },
                None,
            )
            .await;
        match result {
            Ok(updated) => Some(updated),
            // Answered or cancelled in the meantime: nothing left to do.
            Err(StoreError::Conflict { .. }) => None,
            Err(err) => {
                tracing::error!(request_id = %cr.request_id, to = %to, error = %err, "scheduled transition failed");
                None
            }
        }
    }

    async fn expire(&self, cr: CoordinationRequest, now: DateTime<Utc>) {
        let fallback = cr.timeout_policy.fallback;
        match cr.state {
            State::PendingResponse => self.apply_fallback(cr, now).await,
            // Never reached a human, so the fallback does not apply: it simply times out.
            State::Routing | State::Escalated => {
                let payload = json!({ "fallback": fallback, "reason": "undelivered" });
                self.system_transition(&cr, State::TimedOut, payload, Updates::default()).await;
            }
            State::Submitted => {
                let payload = json!({ "responder_id": cr.responder_id, "reason": "expired_before_routing" });
                if let Some(routing) = self.system_transition(&cr, State::Routing, payload, Updates::default()).await {
                    let payload = json!({ "fallback": fallback, "reason": "undelivered" });
                    self.system_transition(&routing, State::TimedOut, payload, Updates::default()).await;
                }
            }
            State::Responded | State::Delivered | State::TimedOut | State::Cancelled => {}
        }
    }

    async fn apply_fallback(&self, cr: CoordinationRequest, now: DateTime<Utc>) {
        let fallback = cr.timeout_policy.fallback;
        match fallback {
            Fallback::AutoApprove | Fallback::AutoReject => {
                let (decision, by) = match fallback {
                    Fallback::AutoApprove => ("approved", "system:auto_approve"),
                    _ => ("rejected", "system:auto_reject"),
                };
                let updates = Updates {
                    response_data: Some(json!({ "decision": decision, "auto": true })),
                    responded_by: Some(by.to_string()),
                    responded_at: Some(now),
                    ..Updates::default()
                };
                let payload = json!({ "fallback": fallback, "auto": true });
                self.system_transition(&cr, State::Responded, payload, updates).await;
            }
            Fallback::Escalate => match cr.timeout_policy.escalation_responder_id.clone() {
                // Escalate once. The original service re-escalated to the same person forever.
                Some(target) if target != cr.responder_id => self.escalate(cr, target, now).await,
                Some(_) => self.time_out(&cr, Some("escalation_exhausted")).await,
                None => self.time_out(&cr, Some("no_escalation_target")).await,
            },
            Fallback::Block | Fallback::Fail | Fallback::Skip => self.time_out(&cr, None).await,
        }
    }

    async fn time_out(&self, cr: &CoordinationRequest, reason: Option<&str>) {
        let mut payload = json!({ "fallback": cr.timeout_policy.fallback });
        if let Some(reason) = reason {
            payload["reason"] = json!(reason);
        }
        self.system_transition(cr, State::TimedOut, payload, Updates::default()).await;
    }

    async fn escalate(&self, cr: CoordinationRequest, target: String, now: DateTime<Utc>) {
        let updates = Updates {
            responder_id: Some(target.clone()),
            timeout_at: Some(cr.timeout_policy.deadline_from(now).unwrap_or(DateTime::<Utc>::MAX_UTC)),
            ..Updates::default()
        };
        let payload = json!({ "fallback": Fallback::Escalate, "escalation_responder_id": target });
        let Some(escalated) = self.system_transition(&cr, State::Escalated, payload, updates).await else {
            return;
        };
        // Retire the original message before a new one is posted for the escalation target.
        self.refresh_slack_message(&escalated, None).await;
        self.reroute(escalated).await;
    }

    async fn reroute(&self, escalated: CoordinationRequest) {
        let payload = json!({ "responder_id": escalated.responder_id });
        if let Some(routing) = self.system_transition(&escalated, State::Routing, payload, Updates::default()).await {
            self.deliver(routing).await;
        }
    }

    /// Picks up a request that stalled before reaching a human (a crash, or Slack was down).
    async fn resume(&self, cr: CoordinationRequest, stale_before: DateTime<Utc>) {
        match cr.state {
            State::Submitted => self.route(cr).await,
            State::Routing | State::Escalated => {
                match self.inner.store.claim_stale(&cr.request_id, cr.state, stale_before).await {
                    Ok(true) if cr.state == State::Routing => self.deliver(cr).await,
                    Ok(true) => self.reroute(cr).await,
                    Ok(false) => {}
                    Err(err) => {
                        tracing::error!(request_id = %cr.request_id, error = %err, "failed to claim stalled request")
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Intent;
    use crate::slack::tests::{config, request};

    #[test]
    fn without_slack_requests_are_answerable_through_the_api_only() {
        assert_eq!(destination(&request(Intent::Approval), None), Destination::ApiOnly);
    }

    #[test]
    fn requests_needing_a_human_go_to_the_default_channel() {
        let cfg = config();
        assert_eq!(destination(&request(Intent::Approval), Some(&cfg)), Destination::Slack("C_APPROVALS".into()));
    }

    #[test]
    fn a_portal_hint_still_routes_to_slack() {
        let (cfg, mut cr) = (config(), request(Intent::Decision));
        cr.routing_hints.channel = Channel::Portal;
        cr.routing_hints.slack_channel_id = Some("C_IGNORED".into());
        assert_eq!(destination(&cr, Some(&cfg)), Destination::Slack("C_APPROVALS".into()));
    }

    #[test]
    fn a_slack_channel_hint_overrides_the_default() {
        let (cfg, mut cr) = (config(), request(Intent::Approval));
        cr.routing_hints.slack_channel_id = Some("C_OPS".into());
        assert_eq!(destination(&cr, Some(&cfg)), Destination::Slack("C_OPS".into()));
    }

    #[test]
    fn notifications_are_only_posted_when_a_channel_is_named() {
        let (mut cfg, mut cr) = (config(), request(Intent::Notification));
        assert_eq!(destination(&cr, Some(&cfg)), Destination::LogOnly);

        cfg.notify_channel = Some("C_FEED".into());
        assert_eq!(destination(&cr, Some(&cfg)), Destination::Slack("C_FEED".into()));

        cr.routing_hints.slack_channel_id = Some("C_OPS".into());
        assert_eq!(destination(&cr, Some(&cfg)), Destination::Slack("C_OPS".into()));
    }
}
