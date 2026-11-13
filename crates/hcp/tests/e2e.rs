//! End-to-end behaviour of the server: the agent API, routing to Slack, timeouts, and the
//! Obsidian log. Everything is real except Slack itself, which is a recording mock.

mod common;

use chrono::{Duration, Utc};
use reqwest::StatusCode;
use serde_json::{Value, json};

use common::{BOT_TOKEN, CHANNEL, Harness, Options, SlackMode, action_ids, approval, block_text, eventually, with};
use hcp::domain::State;
use hcp::slack::{ACTION_APPROVE, ACTION_REJECT, ACTION_RESPOND, MODAL_BLOCK};

fn types(events: &[&str]) -> Vec<String> {
    events.iter().map(|e| e.to_string()).collect()
}

// ── The main path ───────────────────────────────────────────────────────────

#[tokio::test]
async fn an_approval_is_routed_to_slack_answered_there_and_logged_to_the_vault() {
    let hcp = Harness::start(Options::default()).await;
    let agent = hcp.key("claude-code", &[]).await;

    let id = hcp.submit(&agent, &approval()).await;

    // It is posted to the approvals channel, with buttons, using the bot token.
    let posted = hcp.slack.wait_calls("chat.postMessage", 1).await.remove(0);
    assert_eq!(posted.authorization, format!("Bearer {BOT_TOKEN}"));
    assert_eq!(posted.body["channel"], CHANNEL);
    assert_eq!(action_ids(&posted.body), [ACTION_APPROVE, ACTION_REJECT]);
    assert!(block_text(&posted.body).contains("Deploy v2.1.0 to production"));
    hcp.wait_state(&id, State::PendingResponse).await;

    // A human clicks Approve in Slack.
    let (status, _) = hcp.slack_interaction(&hcp.click(ACTION_APPROVE, &id, "U_ISAAC", "isaac")).await;
    assert_eq!(status, StatusCode::OK);
    let responded = hcp.wait_state(&id, State::Responded).await;
    assert_eq!(responded.response_data, Some(json!({ "decision": "approved" })));
    assert_eq!(responded.responded_by.as_deref(), Some("slack:isaac"));

    // The Slack message is replaced by the outcome and loses its buttons.
    let updated = hcp.slack.wait_calls("chat.update", 1).await.remove(0);
    assert_eq!(updated.body["channel"], CHANNEL);
    assert_eq!(updated.body["ts"], posted_ts(&hcp, &id).await);
    assert!(action_ids(&updated.body).is_empty());
    assert!(block_text(&updated.body).contains("*Approved* by <@U_ISAAC>"));

    // The agent polls and takes delivery.
    let (status, delivered) = hcp.get(&format!("/v1/requests/{id}"), &agent).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(delivered["state"], "DELIVERED");
    assert_eq!(delivered["response_data"], json!({ "decision": "approved" }));
    assert_eq!(delivered["responded_by"], "slack:isaac");
    assert!(delivered["delivered_at"].is_string());

    // Every step is in the audit trail, in order.
    assert_eq!(
        hcp.audit_types(&agent, &id).await,
        types(&[
            "CR_SUBMITTED",
            "CR_ROUTING",
            "SLACK_NOTIFIED",
            "CR_PENDING_RESPONSE",
            "CR_RESPONDED",
            "SLACK_INTERACTION",
            "CR_DELIVERED",
        ])
    );

    // And in the Obsidian vault: the request note reflects the final state...
    let note = hcp.request_note(&id).await;
    assert!(note.contains("\nstate: DELIVERED\n"));
    assert!(note.contains("\ndecision: \"approved\"\n"));
    assert!(note.contains("\nresponded_by: \"slack:isaac\"\n"));
    assert!(note.contains("Includes the new auth flow and 3 bug fixes."));
    for event in ["CR_SUBMITTED", "SLACK_NOTIFIED", "CR_RESPONDED", "SLACK_INTERACTION", "CR_DELIVERED"] {
        assert!(note.contains(&format!("`{event}`")), "note is missing {event}");
    }
    // ...and the daily log has one line per event, each linking to the note.
    let log = hcp.audit_log().await;
    assert_eq!(log.matches(&format!("[[{id}|APPROVAL: Deploy v2.1.0 to production]]")).count(), 7);

    hcp.app.shutdown().await;
}

async fn posted_ts(hcp: &Harness, id: &str) -> String {
    hcp.app.engine.store().slack_message(id).await.unwrap().expect("no Slack message recorded").1
}

#[tokio::test]
async fn rejecting_in_slack_is_delivered_as_a_rejection() {
    let hcp = Harness::start(Options::default()).await;
    let agent = hcp.key("claude-code", &[]).await;
    let id = hcp.submit(&agent, &approval()).await;
    hcp.wait_state(&id, State::PendingResponse).await;

    hcp.slack_interaction(&hcp.click(ACTION_REJECT, &id, "U_ISAAC", "isaac")).await;

    let (_, delivered) = hcp.get(&format!("/v1/requests/{id}"), &agent).await;
    assert_eq!(delivered["state"], "DELIVERED");
    assert_eq!(delivered["response_data"]["decision"], "rejected");
    let updated = hcp.slack.wait_calls("chat.update", 1).await.remove(0);
    assert!(block_text(&updated.body).contains("*Rejected* by <@U_ISAAC>"));
}

// ── Slack authenticity and authorisation ────────────────────────────────────

#[tokio::test]
async fn slack_interactions_without_a_valid_signature_are_rejected() {
    let hcp = Harness::start(Options::default()).await;
    let agent = hcp.key("claude-code", &[]).await;
    let id = hcp.submit(&agent, &approval()).await;
    hcp.wait_state(&id, State::PendingResponse).await;

    let payload = hcp.click(ACTION_APPROVE, &id, "U_MALLORY", "mallory");
    let body = serde_urlencoded::to_string([("payload", payload.to_string())]).unwrap();
    let now = Utc::now().timestamp().to_string();
    let stale = (Utc::now().timestamp() - 3600).to_string();

    let forged = hcp::slack::sign("not-the-secret", &now, body.as_bytes());
    assert_eq!(hcp.raw_slack_interaction(body.clone(), &now, &forged).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(hcp.raw_slack_interaction(body.clone(), &now, "").await.0, StatusCode::UNAUTHORIZED);
    // A correctly signed but old request is a replay.
    let replayed = hcp::slack::sign(common::SIGNING_SECRET, &stale, body.as_bytes());
    assert_eq!(hcp.raw_slack_interaction(body, &stale, &replayed).await.0, StatusCode::UNAUTHORIZED);

    assert_eq!(hcp.peek(&id).await.state, State::PendingResponse);
}

#[tokio::test]
async fn only_the_mapped_responder_may_answer_in_slack() {
    let hcp = Harness::start(Options { responders: vec![("ops-lead", "U_OPS")], ..Options::default() }).await;
    let agent = hcp.key("claude-code", &[]).await;
    let id = hcp.submit(&agent, &approval()).await;
    hcp.wait_state(&id, State::PendingResponse).await;

    // The mapped responder is mentioned in the message.
    let posted = hcp.slack.wait_calls("chat.postMessage", 1).await.remove(0);
    assert!(block_text(&posted.body).contains("<@U_OPS>"));

    // Someone else clicking gets a private explanation and changes nothing.
    hcp.slack_interaction(&hcp.click(ACTION_APPROVE, &id, "U_OTHER", "other")).await;
    let told = hcp.slack.wait_calls("response_url", 1).await.remove(0);
    assert_eq!(told.body["response_type"], "ephemeral");
    assert!(told.body["text"].as_str().unwrap().contains("Only <@U_OPS>"));
    assert_eq!(hcp.peek(&id).await.state, State::PendingResponse);

    hcp.slack_interaction(&hcp.click(ACTION_APPROVE, &id, "U_OPS", "ops")).await;
    assert_eq!(hcp.peek(&id).await.state, State::Responded);
}

// ── Who may do what through the API ─────────────────────────────────────────

#[tokio::test]
async fn an_agent_cannot_answer_its_own_request() {
    let hcp = Harness::start(Options { slack: SlackMode::Off, ..Options::default() }).await;
    let agent = hcp.key("claude-code", &[]).await;
    let responder = hcp.key("ops-console", &["respond"]).await;
    let id = hcp.submit(&agent, &approval()).await;
    hcp.wait_state(&id, State::PendingResponse).await;

    let answer = json!({ "response_data": { "decision": "approved" }, "responded_by": "ops-lead" });
    let (status, body) = hcp.post(&format!("/v1/requests/{id}/respond"), &agent, &answer).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(hcp.peek(&id).await.state, State::PendingResponse);

    // A key that was explicitly granted the `respond` scope can.
    let (status, body) = hcp.post(&format!("/v1/requests/{id}/respond"), &responder, &answer).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({ "status": "responded", "request_id": id }));

    // Answering twice is a conflict.
    let (status, _) = hcp.post(&format!("/v1/requests/{id}/respond"), &responder, &answer).await;
    assert_eq!(status, StatusCode::CONFLICT);

    let (_, delivered) = hcp.get(&format!("/v1/requests/{id}"), &agent).await;
    assert_eq!(delivered["state"], "DELIVERED");
    assert_eq!(delivered["responded_by"], "ops-lead");
}

#[tokio::test]
async fn agents_cannot_see_cancel_or_take_delivery_of_each_others_requests() {
    let hcp = Harness::start(Options { slack: SlackMode::Off, ..Options::default() }).await;
    let alice = hcp.key("alice", &[]).await;
    let bob = hcp.key("bob", &[]).await;
    let admin = hcp.key("dashboard", &["admin", "respond"]).await;
    let id = hcp.submit(&alice, &approval()).await;
    hcp.wait_state(&id, State::PendingResponse).await;

    assert_eq!(hcp.get(&format!("/v1/requests/{id}"), &bob).await.0, StatusCode::NOT_FOUND);
    assert_eq!(hcp.delete(&format!("/v1/requests/{id}"), &bob).await.0, StatusCode::NOT_FOUND);
    let (_, listed) = hcp.get("/v1/requests?agent_id=alice", &bob).await;
    assert_eq!(listed["requests"], json!([]));
    let (_, audit) = hcp.get(&format!("/v1/audit?request_id={id}"), &bob).await;
    assert_eq!(audit["events"], json!([]));
    assert_eq!(hcp.peek(&id).await.state, State::PendingResponse);

    // An admin key sees everything, but reading as admin does not count as delivery.
    let answer = json!({ "response_data": { "decision": "approved" }, "responded_by": "ops-lead" });
    hcp.post(&format!("/v1/requests/{id}/respond"), &admin, &answer).await;
    let (status, seen) = hcp.get(&format!("/v1/requests/{id}"), &admin).await;
    assert_eq!((status, &seen["state"]), (StatusCode::OK, &json!("RESPONDED")));
    let (_, listed) = hcp.get("/v1/requests?agent_id=alice", &admin).await;
    assert_eq!(listed["requests"].as_array().unwrap().len(), 1);

    let (_, delivered) = hcp.get(&format!("/v1/requests/{id}"), &alice).await;
    assert_eq!(delivered["state"], "DELIVERED");
}

#[tokio::test]
async fn requests_are_validated_and_authenticated() {
    let hcp = Harness::start(Options { slack: SlackMode::Off, ..Options::default() }).await;
    let agent = hcp.key("claude-code", &[]).await;
    let revoked = hcp.key("old-agent", &[]).await;
    let revoked_id = hcp.app.engine.store().list_api_keys().await.unwrap().pop().unwrap().key_id;
    assert!(hcp.app.engine.store().revoke_api_key(&revoked_id).await.unwrap());

    let post = |key: Option<&'static str>, body: Value| {
        let hcp = &hcp;
        async move { hcp.request(reqwest::Method::POST, "/v1/requests", key, Some(&body)).await }
    };
    assert_eq!(post(None, approval()).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(post(Some("hcp_not_a_real_key"), approval()).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(hcp.post("/v1/requests", &revoked, &approval()).await.0, StatusCode::UNAUTHORIZED);

    let (status, body) = hcp.post("/v1/requests", &agent, &json!({ "intent": "INVALID" })).await;
    assert_eq!((status, &body["error"]), (StatusCode::BAD_REQUEST, &json!("Validation error")));
    let empty_summary = with(approval(), "/context_package/summary", json!(""));
    assert_eq!(hcp.post("/v1/requests", &agent, &empty_summary).await.0, StatusCode::BAD_REQUEST);
    let zero_timeout = with(approval(), "/timeout_policy/timeout_seconds", json!(0));
    assert_eq!(hcp.post("/v1/requests", &agent, &zero_timeout).await.0, StatusCode::BAD_REQUEST);

    assert_eq!(hcp.get("/v1/requests/nonexistent", &agent).await.0, StatusCode::NOT_FOUND);
    assert_eq!(hcp.get("/v1/requests?limit=abc", &agent).await.0, StatusCode::BAD_REQUEST);

    let (status, health) = hcp.request(reqwest::Method::GET, "/health", None, None).await;
    assert_eq!((status, &health["status"]), (StatusCode::OK, &json!("ok")));
}

#[tokio::test]
async fn an_idempotency_key_returns_the_original_request_and_is_private_to_its_agent() {
    let hcp = Harness::start(Options { slack: SlackMode::Off, ..Options::default() }).await;
    let alice = hcp.key("alice", &[]).await;
    let bob = hcp.key("bob", &[]).await;
    let body = with(approval(), "/idempotency_key", json!("deploy-2.1.0"));

    let (first_status, first) = hcp.post("/v1/requests", &alice, &body).await;
    let (second_status, second) = hcp.post("/v1/requests", &alice, &body).await;
    assert_eq!((first_status, second_status), (StatusCode::CREATED, StatusCode::OK));
    assert_eq!(first["request_id"], second["request_id"]);

    // Another agent reusing the key must not be handed Alice's request.
    let (status, leaked) = hcp.post("/v1/requests", &bob, &body).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(leaked.get("request_id").is_none());

    let (_, listed) = hcp.get("/v1/requests", &alice).await;
    assert_eq!(listed["requests"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn listing_filters_and_pages() {
    let hcp = Harness::start(Options { slack: SlackMode::Off, ..Options::default() }).await;
    let agent = hcp.key("claude-code", &[]).await;
    let first = hcp.submit(&agent, &approval()).await;
    let second = hcp.submit(&agent, &with(approval(), "/urgency", json!("LOW"))).await;
    hcp.wait_state(&first, State::PendingResponse).await;
    hcp.wait_state(&second, State::PendingResponse).await;
    hcp.delete(&format!("/v1/requests/{second}"), &agent).await;

    let ids = |body: &Value| -> Vec<String> {
        body["requests"].as_array().unwrap().iter().map(|r| r["request_id"].as_str().unwrap().to_string()).collect()
    };
    // Newest first.
    let (first, second) = (first.as_str(), second.as_str());
    assert_eq!(ids(&hcp.get("/v1/requests", &agent).await.1), [second, first]);
    assert_eq!(ids(&hcp.get("/v1/requests?state=PENDING_RESPONSE", &agent).await.1), [first]);
    assert_eq!(ids(&hcp.get("/v1/requests?urgency=LOW&responder_id=ops-lead", &agent).await.1), [second]);
    assert_eq!(ids(&hcp.get("/v1/requests?limit=1&offset=1", &agent).await.1), [first]);
}

// ── Cancellation ────────────────────────────────────────────────────────────

#[tokio::test]
async fn cancelling_retires_the_slack_message_and_refuses_late_answers() {
    let hcp = Harness::start(Options::default()).await;
    let agent = hcp.key("claude-code", &[]).await;
    let id = hcp.submit(&agent, &approval()).await;
    hcp.wait_state(&id, State::PendingResponse).await;

    let (status, body) = hcp.delete(&format!("/v1/requests/{id}"), &agent).await;
    assert_eq!((status, body), (StatusCode::OK, json!({ "status": "cancelled", "request_id": id })));

    let updated = hcp.slack.wait_calls("chat.update", 1).await.remove(0);
    assert!(block_text(&updated.body).contains("*Cancelled*"));
    assert!(action_ids(&updated.body).is_empty());

    // A click that was already in flight is refused, and the person is told why.
    hcp.slack_interaction(&hcp.click(ACTION_APPROVE, &id, "U_ISAAC", "isaac")).await;
    let told = hcp.slack.wait_calls("response_url", 1).await.remove(0);
    assert!(told.body["text"].as_str().unwrap().contains("CANCELLED"));
    assert_eq!(hcp.peek(&id).await.state, State::Cancelled);

    assert_eq!(hcp.delete(&format!("/v1/requests/{id}"), &agent).await.0, StatusCode::CONFLICT);
    assert!(hcp.request_note(&id).await.contains("\nstate: CANCELLED\n"));
}

// ── Timeouts ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn an_unanswered_request_follows_its_fallback() {
    let hcp = Harness::start(Options::default()).await;
    let agent = hcp.key("claude-code", &[]).await;
    let fallback = |name: &str| with(approval(), "/timeout_policy/fallback", json!(name));
    let rejects = hcp.submit(&agent, &fallback("AUTO_REJECT")).await;
    let approves = hcp.submit(&agent, &fallback("AUTO_APPROVE")).await;
    let blocks = hcp.submit(&agent, &fallback("BLOCK")).await;
    for id in [&rejects, &approves, &blocks] {
        hcp.wait_state(id, State::PendingResponse).await;
    }

    // Before the deadline nothing happens.
    hcp.app.engine.tick(Utc::now() + Duration::seconds(599)).await;
    assert_eq!(hcp.peek(&rejects).await.state, State::PendingResponse);

    hcp.app.engine.tick(Utc::now() + Duration::seconds(601)).await;

    let rejected = hcp.peek(&rejects).await;
    assert_eq!(rejected.state, State::Responded);
    assert_eq!(rejected.response_data, Some(json!({ "decision": "rejected", "auto": true })));
    assert_eq!(rejected.responded_by.as_deref(), Some("system:auto_reject"));

    let approved = hcp.peek(&approves).await;
    assert_eq!(approved.response_data, Some(json!({ "decision": "approved", "auto": true })));
    assert_eq!(approved.responded_by.as_deref(), Some("system:auto_approve"));

    assert_eq!(hcp.peek(&blocks).await.state, State::TimedOut);

    // Slack shows what happened to each, and nobody can click any more.
    let updates = hcp.slack.wait_calls("chat.update", 3).await;
    let texts: Vec<String> = updates.iter().map(|call| block_text(&call.body)).collect();
    assert!(texts.iter().any(|t| t.contains("Rejected automatically")));
    assert!(texts.iter().any(|t| t.contains("Approved automatically")));
    assert!(texts.iter().any(|t| t.contains("Timed out")));
    assert!(updates.iter().all(|call| action_ids(&call.body).is_empty()));

    // The agent receives the automatic decision like any other.
    let (_, delivered) = hcp.get(&format!("/v1/requests/{rejects}"), &agent).await;
    assert_eq!(
        (&delivered["state"], &delivered["response_data"]["decision"]),
        (&json!("DELIVERED"), &json!("rejected"))
    );
}

#[tokio::test]
async fn escalation_reaches_the_second_responder_once_and_then_times_out() {
    let hcp = Harness::start(Options::default()).await;
    let agent = hcp.key("claude-code", &[]).await;
    let body = with(
        approval(),
        "/timeout_policy",
        json!({ "timeout_seconds": 600, "fallback": "ESCALATE", "escalation_responder_id": "cto" }),
    );
    let id = hcp.submit(&agent, &body).await;
    hcp.wait_state(&id, State::PendingResponse).await;
    let first_ts = posted_ts(&hcp, &id).await;

    let first_deadline = Utc::now() + Duration::seconds(601);
    hcp.app.engine.tick(first_deadline).await;

    // Re-routed to the escalation responder with a fresh deadline and a fresh message.
    let escalated = hcp.peek(&id).await;
    assert_eq!(escalated.state, State::PendingResponse);
    assert_eq!(escalated.responder_id, "cto");
    assert!(escalated.timeout_at > first_deadline + Duration::seconds(590));
    let posts = hcp.slack.wait_calls("chat.postMessage", 2).await;
    assert!(block_text(&posts[1].body).contains("`cto`"));
    assert_eq!(action_ids(&posts[1].body), [ACTION_APPROVE, ACTION_REJECT]);
    // The original message is retired rather than left with live buttons.
    let retired = hcp.slack.wait_calls("chat.update", 1).await.remove(0);
    assert_eq!(retired.body["ts"], first_ts);
    assert!(block_text(&retired.body).contains("*Escalated* to `cto`"));
    assert!(action_ids(&retired.body).is_empty());

    // If the escalation responder does not answer either, it ends: no endless re-escalation.
    hcp.app.engine.tick(first_deadline + Duration::seconds(601)).await;
    assert_eq!(hcp.peek(&id).await.state, State::TimedOut);
    assert_eq!(
        hcp.audit_types(&agent, &id).await,
        types(&[
            "CR_SUBMITTED",
            "CR_ROUTING",
            "SLACK_NOTIFIED",
            "CR_PENDING_RESPONSE",
            "CR_ESCALATED",
            "CR_ROUTING",
            "SLACK_NOTIFIED",
            "CR_PENDING_RESPONSE",
            "CR_TIMED_OUT",
        ])
    );

    // The escalated request can still be answered in the window between.
    let second = hcp.submit(&agent, &body).await;
    hcp.wait_state(&second, State::PendingResponse).await;
    hcp.app.engine.tick(Utc::now() + Duration::seconds(601)).await;
    hcp.slack_interaction(&hcp.click(ACTION_APPROVE, &second, "U_CTO", "cto")).await;
    assert_eq!(hcp.peek(&second).await.state, State::Responded);
}

#[tokio::test]
async fn escalating_without_a_target_times_out() {
    let hcp = Harness::start(Options { slack: SlackMode::Off, ..Options::default() }).await;
    let agent = hcp.key("claude-code", &[]).await;
    let id = hcp.submit(&agent, &with(approval(), "/timeout_policy/fallback", json!("ESCALATE"))).await;
    hcp.wait_state(&id, State::PendingResponse).await;

    hcp.app.engine.tick(Utc::now() + Duration::seconds(601)).await;
    assert_eq!(hcp.peek(&id).await.state, State::TimedOut);
}

// ── Delivery failures ───────────────────────────────────────────────────────

#[tokio::test]
async fn a_request_slack_never_showed_anyone_is_never_auto_approved() {
    let hcp = Harness::start(Options::default()).await;
    let agent = hcp.key("claude-code", &[]).await;
    hcp.slack.fail_posts(true);

    let id = hcp.submit(&agent, &with(approval(), "/timeout_policy/fallback", json!("AUTO_APPROVE"))).await;
    hcp.slack.wait_calls("chat.postMessage", 1).await;
    eventually(|| async {
        hcp.audit_types(&agent, &id).await.contains(&"SLACK_NOTIFY_FAILED".to_string()).then_some(())
    })
    .await
    .expect("the delivery failure was not recorded");
    // Undelivered means not pending: nobody has been asked yet.
    assert_eq!(hcp.peek(&id).await.state, State::Routing);

    // Delivery is retried on later scheduler passes; the failure is recorded once.
    hcp.app.engine.tick(Utc::now() + Duration::seconds(31)).await;
    hcp.app.engine.tick(Utc::now() + Duration::seconds(62)).await;
    assert!(hcp.slack.calls("chat.postMessage").len() >= 2);
    assert_eq!(hcp.peek(&id).await.state, State::Routing);

    // At the deadline it times out. It is not approved.
    hcp.app.engine.tick(Utc::now() + Duration::seconds(601)).await;
    let ended = hcp.peek(&id).await;
    assert_eq!(ended.state, State::TimedOut);
    assert_eq!(ended.response_data, None);
    assert_eq!(
        hcp.audit_types(&agent, &id).await,
        types(&["CR_SUBMITTED", "CR_ROUTING", "SLACK_NOTIFY_FAILED", "CR_TIMED_OUT"])
    );
}

#[tokio::test]
async fn delivery_is_retried_until_slack_recovers() {
    let hcp = Harness::start(Options::default()).await;
    let agent = hcp.key("claude-code", &[]).await;
    hcp.slack.fail_posts(true);
    let id = hcp.submit(&agent, &approval()).await;
    hcp.slack.wait_calls("chat.postMessage", 1).await;

    // Too soon to retry: the first attempt may still be in flight.
    hcp.app.engine.tick(Utc::now() + Duration::seconds(5)).await;
    assert_eq!(hcp.slack.calls("chat.postMessage").len(), 1);

    hcp.slack.fail_posts(false);
    hcp.app.engine.tick(Utc::now() + Duration::seconds(31)).await;
    assert_eq!(hcp.peek(&id).await.state, State::PendingResponse);

    hcp.slack_interaction(&hcp.click(ACTION_APPROVE, &id, "U_ISAAC", "isaac")).await;
    assert_eq!(hcp.peek(&id).await.state, State::Responded);
}

// ── Other intents ───────────────────────────────────────────────────────────

#[tokio::test]
async fn a_clarification_is_answered_through_a_modal() {
    let hcp = Harness::start(Options::default()).await;
    let agent = hcp.key("claude-code", &[]).await;
    let body = with(approval(), "/intent", json!("CLARIFICATION"));
    let id = hcp.submit(&agent, &body).await;
    hcp.wait_state(&id, State::PendingResponse).await;

    let posted = hcp.slack.wait_calls("chat.postMessage", 1).await.remove(0);
    assert_eq!(action_ids(&posted.body), [ACTION_RESPOND]);

    // Clicking Respond opens the modal; it does not answer anything by itself.
    hcp.slack_interaction(&hcp.click(ACTION_RESPOND, &id, "U_ISAAC", "isaac")).await;
    let opened = hcp.slack.wait_calls("views.open", 1).await.remove(0);
    assert_eq!(opened.body["trigger_id"], "trigger-123");
    assert_eq!(opened.body["view"]["private_metadata"], id);
    assert_eq!(hcp.peek(&id).await.state, State::PendingResponse);

    // An approve click forged against a non-approval request is ignored.
    hcp.slack_interaction(&hcp.click(ACTION_APPROVE, &id, "U_ISAAC", "isaac")).await;
    assert_eq!(hcp.peek(&id).await.state, State::PendingResponse);

    // An empty submission is bounced back into the modal.
    let (status, reply) = hcp.slack_interaction(&hcp.modal_submission(&id, "U_ISAAC", "isaac", "   ")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(reply["response_action"], "errors");
    assert!(reply["errors"][MODAL_BLOCK].is_string());

    let (status, reply) =
        hcp.slack_interaction(&hcp.modal_submission(&id, "U_ISAAC", "isaac", "Use the staging cluster.")).await;
    assert_eq!((status, reply), (StatusCode::OK, json!("")));

    let (_, delivered) = hcp.get(&format!("/v1/requests/{id}"), &agent).await;
    assert_eq!(delivered["state"], "DELIVERED");
    assert_eq!(delivered["response_data"], json!({ "text": "Use the staging cluster." }));
    let updated = hcp.slack.wait_calls("chat.update", 1).await.remove(0);
    assert!(block_text(&updated.body).contains("Use the staging cluster."));

    // Submitting again once it is answered shows an error in the modal.
    let (_, reply) = hcp.slack_interaction(&hcp.modal_submission(&id, "U_ISAAC", "isaac", "Again")).await;
    assert_eq!(reply["response_action"], "errors");
}

#[tokio::test]
async fn notifications_are_logged_and_only_posted_when_a_channel_is_named() {
    let hcp = Harness::start(Options::default()).await;
    let agent = hcp.key("claude-code", &[]).await;
    let notification = with(approval(), "/intent", json!("NOTIFICATION"));

    // With no channel named, a notification goes to the log, not to Slack.
    let quiet = hcp.submit(&agent, &notification).await;
    hcp.wait_state(&quiet, State::PendingResponse).await;
    assert!(hcp.slack.calls("chat.postMessage").is_empty());
    assert!(hcp.request_note(&quiet).await.contains("intent: NOTIFICATION"));

    // Naming a channel posts it there, without buttons.
    let loud = hcp.submit(&agent, &with(notification, "/routing_hints/slack_channel_id", json!("C_FEED"))).await;
    hcp.wait_state(&loud, State::PendingResponse).await;
    let posted = hcp.slack.wait_calls("chat.postMessage", 1).await.remove(0);
    assert_eq!(posted.body["channel"], "C_FEED");
    assert!(action_ids(&posted.body).is_empty());
}

#[tokio::test]
async fn a_request_asking_for_the_portal_is_routed_to_slack() {
    let hcp = Harness::start(Options::default()).await;
    let agent = hcp.key("claude-code", &[]).await;
    let id = hcp.submit(&agent, &with(approval(), "/routing_hints/channel", json!("portal"))).await;
    hcp.wait_state(&id, State::PendingResponse).await;

    let posted = hcp.slack.wait_calls("chat.postMessage", 1).await.remove(0);
    assert_eq!(posted.body["channel"], CHANNEL);
}

// ── Socket Mode ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn socket_mode_delivers_interactions_acknowledges_them_and_reconnects() {
    let hcp = Harness::start(Options { slack: SlackMode::Socket, ..Options::default() }).await;
    let socket = hcp.socket.as_ref().unwrap();
    let agent = hcp.key("claude-code", &[]).await;

    // The connection is opened with the app-level token, not the bot token.
    let opened = hcp.slack.wait_calls("apps.connections.open", 1).await.remove(0);
    assert_eq!(opened.authorization, format!("Bearer {}", common::APP_TOKEN));

    // In Socket Mode there is no HTTP interactions endpoint to forge requests against.
    let id = hcp.submit(&agent, &approval()).await;
    hcp.wait_state(&id, State::PendingResponse).await;
    assert_eq!(hcp.slack_interaction(&hcp.click(ACTION_APPROVE, &id, "U_X", "x")).await.0, StatusCode::NOT_FOUND);
    assert_eq!(hcp.peek(&id).await.state, State::PendingResponse);

    // Envelopes that are not interactions are acknowledged and otherwise ignored.
    socket.send(json!({ "type": "events_api", "envelope_id": "env-0", "payload": {} }));
    assert_eq!(socket.wait_ack("env-0").await, json!({ "envelope_id": "env-0" }));

    socket.send(json!({
        "type": "interactive",
        "envelope_id": "env-1",
        "payload": hcp.click(ACTION_APPROVE, &id, "U_ISAAC", "isaac"),
    }));
    assert_eq!(socket.wait_ack("env-1").await, json!({ "envelope_id": "env-1" }));
    let responded = hcp.peek(&id).await;
    assert_eq!(responded.state, State::Responded);
    assert_eq!(responded.responded_by.as_deref(), Some("slack:isaac"));

    // A modal validation error travels back inside the acknowledgement.
    let question = hcp.submit(&agent, &with(approval(), "/intent", json!("INPUT"))).await;
    hcp.wait_state(&question, State::PendingResponse).await;
    socket.send(json!({
        "type": "interactive",
        "envelope_id": "env-2",
        "payload": hcp.modal_submission(&question, "U_ISAAC", "isaac", ""),
    }));
    assert_eq!(socket.wait_ack("env-2").await["payload"]["response_action"], "errors");

    // When Slack asks for a reconnect, a new connection is opened and still works.
    assert_eq!(socket.connections(), 1);
    socket.send(json!({ "type": "disconnect", "reason": "refresh_requested" }));
    eventually(|| async { (socket.connections() == 2).then_some(()) }).await.expect("did not reconnect");
    socket.send(json!({
        "type": "interactive",
        "envelope_id": "env-3",
        "payload": hcp.modal_submission(&question, "U_ISAAC", "isaac", "42"),
    }));
    assert_eq!(socket.wait_ack("env-3").await, json!({ "envelope_id": "env-3" }));
    assert_eq!(hcp.peek(&question).await.response_data, Some(json!({ "text": "42" })));

    hcp.app.shutdown().await;
}

// ── Events ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_event_stream_carries_only_the_callers_own_state_changes() {
    let hcp = Harness::start(Options { slack: SlackMode::Off, ..Options::default() }).await;
    let alice = hcp.key("alice", &[]).await;
    let bob = hcp.key("bob", &[]).await;

    let unauthenticated = hcp.http.get(format!("{}/v1/events", hcp.app.base_url())).send().await.unwrap();
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);

    let mut stream =
        hcp.http.get(format!("{}/v1/events", hcp.app.base_url())).bearer_auth(&alice).send().await.unwrap();
    assert_eq!(stream.status(), StatusCode::OK);
    let mut seen = String::new();
    while !seen.contains("event: connected") {
        seen.push_str(std::str::from_utf8(&stream.chunk().await.unwrap().unwrap()).unwrap());
    }

    let bobs = hcp.submit(&bob, &approval()).await;
    let alices = hcp.submit(&alice, &approval()).await;
    hcp.wait_state(&bobs, State::PendingResponse).await;
    hcp.wait_state(&alices, State::PendingResponse).await;

    while !seen.contains("PENDING_RESPONSE") {
        seen.push_str(std::str::from_utf8(&stream.chunk().await.unwrap().unwrap()).unwrap());
    }
    assert!(seen.contains(&format!("\"request_id\":\"{alices}\"")));
    assert!(!seen.contains(&bobs));
    assert!(seen.contains("event: state_change"));

    // An open stream must not keep the server from shutting down.
    tokio::time::timeout(std::time::Duration::from_secs(5), hcp.app.shutdown()).await.expect("shutdown hung");
}

// ── Concurrency ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn simultaneous_answers_have_exactly_one_winner() {
    let hcp = Harness::start(Options::default()).await;
    let agent = hcp.key("claude-code", &[]).await;
    let id = hcp.submit(&agent, &approval()).await;
    hcp.wait_state(&id, State::PendingResponse).await;

    let approve = hcp.click(ACTION_APPROVE, &id, "U_A", "ann");
    let reject = hcp.click(ACTION_REJECT, &id, "U_B", "bob");
    let cancel_path = format!("/v1/requests/{id}");
    tokio::join!(hcp.slack_interaction(&approve), hcp.slack_interaction(&reject), hcp.delete(&cancel_path, &agent));

    // Whichever landed first is the outcome, and it was recorded exactly once.
    let outcome = hcp.peek(&id).await;
    let events = hcp.audit_types(&agent, &id).await;
    let endings = events.iter().filter(|e| ["CR_RESPONDED", "CR_CANCELLED"].contains(&e.as_str())).count();
    assert_eq!(endings, 1, "{events:?}");
    match outcome.state {
        State::Responded => {
            let by = outcome.responded_by.as_deref().unwrap();
            let expected = if outcome.decision() == Some("approved") { "slack:ann" } else { "slack:bob" };
            assert_eq!(by, expected);
        }
        State::Cancelled => assert_eq!(outcome.response_data, None),
        other => panic!("unexpected final state {other}"),
    }
}

// ── The vault as a log that does not lose entries ───────────────────────────

#[cfg(unix)]
#[tokio::test]
async fn events_wait_in_the_outbox_while_the_vault_is_unwritable() {
    use std::os::unix::fs::PermissionsExt;

    let hcp = Harness::start(Options { slack: SlackMode::Off, ..Options::default() }).await;
    let agent = hcp.key("claude-code", &[]).await;
    let log_dir = hcp.vault.root().join("Log");
    std::fs::set_permissions(&log_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

    let id = hcp.submit(&agent, &approval()).await;
    hcp.wait_state(&id, State::PendingResponse).await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // Nothing could be written, so everything is still queued in the database.
    let store = hcp.app.engine.store();
    assert_eq!(store.outbox_len().await.unwrap(), 3);
    assert!(std::fs::read_dir(&log_dir).unwrap().next().is_none());

    // Once the vault is writable again the backlog is written, each event exactly once.
    std::fs::set_permissions(&log_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    hcp.app.engine.vault_wake().notify_one();
    let log = hcp.audit_log().await;
    assert_eq!(log.matches(&format!("[[{id}|")).count(), 3);
    for event in ["CR_SUBMITTED", "CR_ROUTING", "CR_PENDING_RESPONSE"] {
        assert_eq!(log.matches(&format!("`{event}`")).count(), 1, "{event}");
    }
    assert!(hcp.request_note(&id).await.contains("\nstate: PENDING_RESPONSE\n"));
    assert_eq!(store.outbox_len().await.unwrap(), 0);
}

// ── Running against a database the TypeScript service created ───────────────

/// The schema exactly as `src/db/schema.ts` (schema version 1) created it, with rows written
/// the way that service wrote them.
fn typescript_database(key_hash: &str) -> String {
    format!(
        r#"
        CREATE TABLE schema_version (version INTEGER NOT NULL);
        INSERT INTO schema_version (version) VALUES (1);
        CREATE TABLE coordination_requests (
          request_id TEXT PRIMARY KEY, agent_id TEXT NOT NULL, intent TEXT NOT NULL, urgency TEXT NOT NULL,
          state TEXT NOT NULL DEFAULT 'SUBMITTED', context_package JSONB NOT NULL, response_schema JSONB,
          timeout_policy JSONB NOT NULL, routing_hints JSONB NOT NULL, trace_id TEXT, idempotency_key TEXT UNIQUE,
          responder_id TEXT NOT NULL, response_data JSONB, responded_by TEXT, responded_at TIMESTAMPTZ,
          submitted_at TIMESTAMPTZ NOT NULL, updated_at TIMESTAMPTZ NOT NULL, timeout_at TIMESTAMPTZ NOT NULL,
          delivered_at TIMESTAMPTZ
        );
        CREATE INDEX idx_cr_state ON coordination_requests(state);
        CREATE TABLE audit_events (
          event_id TEXT PRIMARY KEY, request_id TEXT NOT NULL REFERENCES coordination_requests(request_id),
          event_type TEXT NOT NULL, actor TEXT NOT NULL, actor_type TEXT NOT NULL,
          payload JSONB NOT NULL DEFAULT '{{}}', created_at TIMESTAMPTZ NOT NULL
        );
        CREATE TABLE api_keys (
          key_id TEXT PRIMARY KEY, key_hash TEXT NOT NULL UNIQUE, agent_id TEXT NOT NULL, label TEXT NOT NULL,
          scopes JSONB NOT NULL DEFAULT '[]', created_at TIMESTAMPTZ NOT NULL, revoked_at TIMESTAMPTZ
        );

        INSERT INTO api_keys (key_id, key_hash, agent_id, label, scopes, created_at)
        VALUES ('01OLDKEY', '{key_hash}', 'nanoclaw', 'NanoClaw Production', '[]', '2026-06-19T07:00:00.000Z');

        INSERT INTO coordination_requests
          (request_id, agent_id, intent, urgency, state, context_package, response_schema, timeout_policy,
           routing_hints, trace_id, idempotency_key, responder_id, response_data, responded_by, responded_at,
           submitted_at, updated_at, timeout_at, delivered_at)
        VALUES
          ('01OLDDELIVERED', 'nanoclaw', 'APPROVAL', 'HIGH', 'DELIVERED',
           '{{"summary":"Delete 500 inactive accounts","metadata":{{"count":500}}}}', NULL,
           '{{"timeout_seconds":600,"fallback":"BLOCK"}}', '{{"responder_id":"ops-lead","channel":"portal"}}',
           NULL, 'old-key-1', 'ops-lead', '{{"decision":"approved","comment":"LGTM"}}', 'ops-lead',
           '2026-06-19T07:05:00.000Z', '2026-06-19T07:00:00.000Z', '2026-06-19T07:06:00.000Z',
           '2026-06-19T07:10:00.000Z', '2026-06-19T07:06:00.000Z'),
          ('01OLDPENDING', 'nanoclaw', 'APPROVAL', 'MEDIUM', 'PENDING_RESPONSE',
           '{{"summary":"Rotate the signing keys"}}', NULL,
           '{{"timeout_seconds":86400,"fallback":"AUTO_REJECT"}}',
           '{{"responder_id":"ops-lead","channel":"slack","slack_channel_id":"C0123456789"}}',
           'trace-9', NULL, 'ops-lead', NULL, NULL, NULL,
           now(), now(), now() + interval '1 day', NULL);

        INSERT INTO audit_events (event_id, request_id, event_type, actor, actor_type, payload, created_at)
        VALUES ('01OLDEVENT', '01OLDDELIVERED', 'CR_SUBMITTED', 'nanoclaw', 'AGENT',
                '{{"intent":"APPROVAL","urgency":"HIGH"}}', '2026-06-19T07:00:00.000Z');
        "#
    )
}

#[tokio::test]
async fn it_runs_against_a_database_created_by_the_typescript_service() {
    let old_key = "hcp_0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let hcp = Harness::start(Options {
        seed_sql: Some(typescript_database(&hcp::store::hash_api_key(old_key))),
        ..Options::default()
    })
    .await;

    // Existing keys still authenticate, and existing requests read back unchanged.
    let (status, old) = hcp.get("/v1/requests/01OLDDELIVERED", old_key).await;
    assert_eq!(status, StatusCode::OK, "{old}");
    assert_eq!(old["state"], "DELIVERED");
    assert_eq!(
        old["context_package"],
        json!({ "summary": "Delete 500 inactive accounts", "metadata": { "count": 500 } })
    );
    assert_eq!(old["routing_hints"], json!({ "responder_id": "ops-lead", "channel": "portal" }));
    assert_eq!(old["response_data"], json!({ "decision": "approved", "comment": "LGTM" }));
    assert_eq!(old["submitted_at"], "2026-06-19T07:00:00.000Z");
    assert_eq!(old["delivered_at"], "2026-06-19T07:06:00.000Z");
    assert_eq!(hcp.audit_types(old_key, "01OLDDELIVERED").await, types(&["CR_SUBMITTED"]));
    let (_, listed) = hcp.get("/v1/requests", old_key).await;
    assert_eq!(listed["requests"].as_array().unwrap().len(), 2);

    // A request that was already pending under the old server can be answered in Slack and
    // collected by its agent under the new one.
    hcp.slack_interaction(&hcp.click(ACTION_APPROVE, "01OLDPENDING", "U_ISAAC", "isaac")).await;
    let (_, delivered) = hcp.get("/v1/requests/01OLDPENDING", old_key).await;
    assert_eq!(delivered["state"], "DELIVERED");
    assert_eq!(delivered["trace_id"], "trace-9");
    assert!(hcp.request_note("01OLDPENDING").await.contains("\nstate: DELIVERED\n"));

    // The schema was upgraded in place.
    let pool = sqlx::PgPool::connect(&hcp.db_url).await.unwrap();
    let version: i32 = sqlx::query_scalar("SELECT version FROM schema_version").fetch_one(&pool).await.unwrap();
    assert_eq!(version, 2);
}

#[tokio::test]
async fn an_expired_request_is_settled_when_its_agent_reads_it() {
    let hcp = Harness::start(Options::default()).await;
    let agent = hcp.key("claude-code", &[]).await;
    let body = with(approval(), "/timeout_policy", json!({ "timeout_seconds": 1, "fallback": "AUTO_REJECT" }));
    let id = hcp.submit(&agent, &body).await;
    hcp.wait_state(&id, State::PendingResponse).await;

    // Before the deadline a read changes nothing.
    assert_eq!(hcp.get(&format!("/v1/requests/{id}"), &agent).await.1["state"], "PENDING_RESPONSE");

    // After it, the agent gets the outcome on its next read, without waiting for a scheduler
    // pass (the harness never runs one on its own).
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let (_, settled) = hcp.get(&format!("/v1/requests/{id}"), &agent).await;
    assert_eq!(settled["state"], "DELIVERED");
    assert_eq!(settled["response_data"], json!({ "decision": "rejected", "auto": true }));
    let updated = hcp.slack.wait_calls("chat.update", 1).await.remove(0);
    assert!(block_text(&updated.body).contains("Rejected automatically"));
}
