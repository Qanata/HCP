//! Handles what people do in Slack: button clicks and modal submissions.
//!
//! The payload is the same whether it arrived over Socket Mode or signed HTTP; the caller
//! is responsible for having authenticated it. Nothing in the payload is trusted beyond
//! "this Slack user did this": the request is always re-read from the database.

use serde_json::{Value, json};

use super::{ACTION_APPROVE, ACTION_REJECT, ACTION_RESPOND, MODAL_BLOCK, MODAL_CALLBACK, MODAL_INPUT};
use crate::domain::{ActorType, CoordinationRequest, Intent, State, event};
use crate::engine::{Answer, Engine, EngineError};

struct SlackUser<'a> {
    id: &'a str,
    name: &'a str,
}

impl<'a> SlackUser<'a> {
    fn from_payload(payload: &'a Value) -> Option<Self> {
        let user = payload.get("user")?;
        let id = user.get("id")?.as_str()?;
        let name = ["username", "name"].iter().find_map(|key| user.get(key)?.as_str()).unwrap_or(id);
        Some(Self { id, name })
    }
}

/// Processes one interaction payload. The returned value, if any, must be sent back to
/// Slack as the acknowledgement body (it is how a modal shows a validation error).
pub async fn handle(engine: &Engine, payload: &Value) -> Option<Value> {
    match payload.get("type").and_then(Value::as_str) {
        Some("block_actions") => {
            block_action(engine, payload).await;
            None
        }
        Some("view_submission") => view_submission(engine, payload).await,
        _ => None,
    }
}

/// Why this user may not answer this request right now, phrased for the user.
fn refusal(engine: &Engine, cr: &CoordinationRequest, user: &SlackUser<'_>) -> Option<String> {
    let responders = &engine.slack()?.config().responders;
    if let Some(allowed) = responders.get(&cr.responder_id).filter(|allowed| *allowed != user.id) {
        return Some(format!("Only <@{allowed}> can respond to this request."));
    }
    if cr.state != State::PendingResponse {
        return Some(format!("This request can no longer be answered (it is {}).", cr.state));
    }
    None
}

async fn tell(engine: &Engine, payload: &Value, text: &str) {
    let (Some(slack), Some(url)) = (engine.slack(), payload.get("response_url").and_then(Value::as_str)) else {
        return;
    };
    if let Err(err) = slack.reply_ephemeral(url, text).await {
        tracing::warn!(error = %err, "failed to send ephemeral Slack reply");
    }
}

async fn answer(
    engine: &Engine,
    cr: &CoordinationRequest,
    user: &SlackUser<'_>,
    response_data: Value,
    action_id: &str,
) -> Result<(), EngineError> {
    let decision = response_data.get("decision").cloned().unwrap_or(Value::Null);
    let actor = format!("slack:{}", user.id);
    let responded_by = format!("slack:{}", user.name);
    engine
        .respond(
            &cr.request_id,
            Answer {
                response_data,
                actor: &actor,
                responded_by: &responded_by,
                slack_user: Some(user.id),
                audit: json!({ "slack_user": user.name, "slack_user_id": user.id }),
            },
        )
        .await?;
    let payload = json!({ "action_id": action_id, "decision": decision });
    if let Err(err) =
        engine.store().append_audit(&cr.request_id, event::SLACK_INTERACTION, &actor, ActorType::Human, payload).await
    {
        tracing::error!(request_id = %cr.request_id, error = %err, "failed to record Slack interaction");
    }
    engine.vault_wake().notify_one();
    Ok(())
}

async fn block_action(engine: &Engine, payload: &Value) {
    let Some(action) = payload.get("actions").and_then(|actions| actions.get(0)) else {
        return;
    };
    let (Some(action_id), Some(request_id)) =
        (action.get("action_id").and_then(Value::as_str), action.get("value").and_then(Value::as_str))
    else {
        return;
    };
    if ![ACTION_APPROVE, ACTION_REJECT, ACTION_RESPOND].contains(&action_id) {
        return;
    }
    let Some(user) = SlackUser::from_payload(payload) else {
        return;
    };

    let cr = match engine.get(request_id).await {
        Ok(cr) => cr,
        Err(EngineError::NotFound) => return tell(engine, payload, "That request no longer exists.").await,
        Err(err) => {
            tracing::error!(request_id, error = %err, "failed to load request for Slack interaction");
            return tell(engine, payload, "HCP could not load that request. Please try again.").await;
        }
    };
    if let Some(reason) = refusal(engine, &cr, &user) {
        return tell(engine, payload, &reason).await;
    }

    // The buttons shown depend on the intent; do not let a forged or stale action through.
    let result = match (action_id, cr.intent) {
        (ACTION_APPROVE, Intent::Approval) => {
            answer(engine, &cr, &user, json!({ "decision": "approved" }), action_id).await
        }
        (ACTION_REJECT, Intent::Approval) => {
            answer(engine, &cr, &user, json!({ "decision": "rejected" }), action_id).await
        }
        (ACTION_RESPOND, intent) if intent != Intent::Approval && intent.needs_response() => {
            let (Some(slack), Some(trigger)) = (engine.slack(), payload.get("trigger_id").and_then(Value::as_str))
            else {
                return;
            };
            if let Err(err) = slack.open_response_modal(trigger, &cr).await {
                tracing::warn!(request_id, error = %err, "failed to open Slack response modal");
                tell(engine, payload, "HCP could not open the response dialog. Please try again.").await;
            }
            return;
        }
        _ => return,
    };

    match result {
        Ok(()) => {}
        Err(EngineError::WrongState { state, .. }) => {
            tell(engine, payload, &format!("This request can no longer be answered (it is {state}).")).await;
        }
        Err(err) => {
            tracing::error!(request_id, error = %err, "failed to record Slack response");
            tell(engine, payload, "HCP could not record your response. Please try again.").await;
        }
    }
}

fn modal_error(message: &str) -> Value {
    json!({ "response_action": "errors", "errors": { MODAL_BLOCK: message } })
}

async fn view_submission(engine: &Engine, payload: &Value) -> Option<Value> {
    let view = payload.get("view")?;
    if view.get("callback_id").and_then(Value::as_str) != Some(MODAL_CALLBACK) {
        return None;
    }
    let request_id = view.get("private_metadata").and_then(Value::as_str)?;
    let user = SlackUser::from_payload(payload)?;
    let text = view
        .pointer(&format!("/state/values/{MODAL_BLOCK}/{MODAL_INPUT}/value"))
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default();
    if text.is_empty() {
        return Some(modal_error("Please enter a response."));
    }

    let cr = match engine.get(request_id).await {
        Ok(cr) => cr,
        Err(EngineError::NotFound) => return Some(modal_error("That request no longer exists.")),
        Err(err) => {
            tracing::error!(request_id, error = %err, "failed to load request for Slack submission");
            return Some(modal_error("HCP could not load that request. Please try again."));
        }
    };
    if let Some(reason) = refusal(engine, &cr, &user) {
        return Some(modal_error(&reason));
    }
    if cr.intent == Intent::Approval || !cr.intent.needs_response() {
        return Some(modal_error("This request does not take a written response."));
    }

    match answer(engine, &cr, &user, json!({ "text": text }), ACTION_RESPOND).await {
        Ok(()) => None,
        Err(EngineError::WrongState { state, .. }) => {
            Some(modal_error(&format!("This request can no longer be answered (it is {state}).")))
        }
        Err(err) => {
            tracing::error!(request_id, error = %err, "failed to record Slack response");
            Some(modal_error("HCP could not record your response. Please try again."))
        }
    }
}
