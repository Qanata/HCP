//! `hcp mcp`: an MCP server over stdio that lets any agent harness route its questions,
//! approvals and notifications to a human through HCP.
//!
//! The protocol itself (the `initialize` handshake of older MCP revisions, the stateless
//! 2026-07-28 revision, cancellation, framing) is handled by the official `rmcp` SDK; this
//! module only defines the five tools and what they do.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation, JsonObject, ListToolsResult,
    PaginatedRequestParams, ProgressNotificationParam, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ErrorData as McpError, ServiceExt};
use serde_json::{Value, json};

use crate::client::{AgentEnv, ClientError, HcpClient, Waited};

const INTENTS: [&str; 7] = ["APPROVAL", "CLARIFICATION", "ESCALATION", "NOTIFICATION", "DECISION", "REVIEW", "INPUT"];
const URGENCIES: [&str; 4] = ["CRITICAL", "HIGH", "MEDIUM", "LOW"];
const FALLBACKS: [&str; 6] = ["AUTO_APPROVE", "AUTO_REJECT", "ESCALATE", "BLOCK", "FAIL", "SKIP"];

/// The requests `hcp_coordinate` is currently blocked on. Shared with `serve` so that they
/// can be withdrawn if the client goes away mid-wait.
type InFlight = Arc<Mutex<HashSet<String>>>;

#[derive(Clone)]
pub struct HcpTools {
    client: HcpClient,
    responder: String,
    slack_channel: Option<String>,
    poll: Duration,
    progress: Duration,
    in_flight: InFlight,
}

impl HcpTools {
    pub fn new(client: HcpClient, env: &AgentEnv) -> Self {
        Self {
            client,
            responder: env.responder.clone(),
            slack_channel: env.slack_channel.clone(),
            poll: env.poll,
            progress: env.progress,
            in_flight: InFlight::default(),
        }
    }

    /// Fills in who should answer, and where, when the caller did not say.
    fn with_default_routing(&self, mut args: JsonObject) -> Value {
        let hints = args.entry("routing_hints").or_insert_with(|| json!({}));
        if let Some(hints) = hints.as_object_mut() {
            let unset = |value: Option<&Value>| value.and_then(Value::as_str).is_none_or(|s| s.trim().is_empty());
            if unset(hints.get("responder_id")) {
                hints.insert("responder_id".to_string(), json!(self.responder));
            }
            if let Some(channel) = &self.slack_channel
                && unset(hints.get("slack_channel_id"))
            {
                hints.insert("slack_channel_id".to_string(), json!(channel));
            }
        }
        Value::Object(args)
    }

    async fn coordinate(&self, args: JsonObject, context: &RequestContext<RoleServer>) -> Result<Value, ClientError> {
        let submitted = self.client.submit(&self.with_default_routing(args)).await?;
        let request_id = submitted.request_id.clone();
        self.in_flight.lock().unwrap().insert(request_id.clone());

        let progress_token = context.meta.get_progress_token();
        let mut heartbeat = tokio::time::interval_at(tokio::time::Instant::now() + self.progress, self.progress);
        let mut beats = 0.0;
        let wait = self.client.wait(submitted, self.poll);
        tokio::pin!(wait);

        let waited = loop {
            tokio::select! {
                waited = &mut wait => break waited,
                // The agent stopped waiting (the user interrupted it, say): the question is
                // moot, so withdraw it rather than leave it for a human to answer into the void.
                _ = context.ct.cancelled() => {
                    let _ = self.client.cancel(&request_id).await;
                    break Err(ClientError::Malformed("the tool call was cancelled".to_string()));
                }
                _ = heartbeat.tick() => {
                    if let Some(token) = &progress_token {
                        beats += 1.0;
                        let note = ProgressNotificationParam::new(token.clone(), beats)
                            .with_message("Waiting for a human to respond in Slack");
                        let _ = context.peer.notify_progress(note).await;
                    }
                }
            }
        };
        self.in_flight.lock().unwrap().remove(&request_id);

        let cr = match waited? {
            Waited::Settled(cr) => cr,
            Waited::Overdue(cr) => {
                return Err(ClientError::Malformed(format!(
                    "request {} was still {} after its deadline; no answer is available",
                    cr.request_id, cr.state
                )));
            }
        };
        Ok(json!({
            "request_id": cr.request_id,
            "state": cr.state,
            "response_data": cr.response_data,
            "responded_by": cr.responded_by,
            "responded_at": cr.responded_at.as_ref().map(crate::domain::timestamp::format),
        }))
    }

    async fn notify(&self, args: JsonObject) -> Result<Value, ClientError> {
        let body = self.with_default_routing(
            json!({
                "intent": "NOTIFICATION",
                "urgency": args.get("urgency").and_then(Value::as_str).unwrap_or("LOW"),
                "context_package": {
                    "summary": args.get("summary").cloned().unwrap_or(Value::Null),
                    "detail": args.get("detail").filter(|v| !v.is_null()),
                    "metadata": args.get("metadata").filter(|v| !v.is_null()),
                },
                "timeout_policy": { "timeout_seconds": 3600, "fallback": "AUTO_APPROVE" },
            })
            .as_object()
            .cloned()
            .unwrap_or_default(),
        );
        let cr = self.client.submit(&strip_nulls(body)).await?;
        Ok(json!({ "request_id": cr.request_id, "state": cr.state }))
    }

    async fn submit(&self, args: JsonObject) -> Result<Value, ClientError> {
        let cr = self.client.submit(&self.with_default_routing(args)).await?;
        Ok(json!({ "request_id": cr.request_id, "state": cr.state }))
    }

    async fn poll(&self, request_id: &str) -> Result<Value, ClientError> {
        let cr = self.client.get(request_id).await?;
        Ok(json!({
            "request_id": cr.request_id,
            "state": cr.state,
            "response_data": cr.response_data,
            "responded_by": cr.responded_by,
        }))
    }

    async fn cancel(&self, request_id: &str) -> Result<Value, ClientError> {
        self.client.cancel(request_id).await?;
        Ok(json!({ "status": "cancelled", "request_id": request_id }))
    }
}

/// Removes `null` members from the context package, which the API treats as invalid.
fn strip_nulls(mut body: Value) -> Value {
    if let Some(context) = body.get_mut("context_package").and_then(Value::as_object_mut) {
        context.retain(|_, value| !value.is_null());
    }
    body
}

fn required_string<'a>(args: &'a JsonObject, name: &str) -> Result<&'a str, McpError> {
    args.get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| McpError::invalid_params(format!("missing required argument: {name}"), None))
}

impl ServerHandler for HcpTools {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("hcp", env!("CARGO_PKG_VERSION")))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(tools()))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        tools().into_iter().find(|tool| tool.name == name)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let args = request.arguments.unwrap_or_default();
        let outcome = match request.name.as_ref() {
            "hcp_coordinate" => self.coordinate(args, &context).await,
            "hcp_notify" => {
                required_string(&args, "summary")?;
                self.notify(args).await
            }
            "hcp_submit" => self.submit(args).await,
            "hcp_poll" => self.poll(required_string(&args, "request_id")?).await,
            "hcp_cancel" => self.cancel(required_string(&args, "request_id")?).await,
            // An unknown tool is a protocol error; a tool that ran and failed is a result.
            other => return Err(McpError::invalid_params(format!("Unknown tool: {other}"), None)),
        };
        // Failures are reported as tool results, not protocol errors, so the model can read
        // the reason (a validation message, say) and correct itself.
        let result = match outcome {
            Ok(value) => CallToolResult::success(vec![ContentBlock::text(
                serde_json::to_string_pretty(&value).unwrap_or_default(),
            )]),
            Err(err) => CallToolResult::error(vec![ContentBlock::text(format!("Error: {err}"))]),
        };
        Ok(result.into())
    }
}

/// Runs the MCP server on stdin/stdout until the client disconnects.
pub async fn serve(env: AgentEnv) -> anyhow::Result<()> {
    let api_key = env.require_api_key()?;
    // Generous per-request timeout: individual calls are quick, the long wait is polling.
    let client = HcpClient::new(&env.base_url, api_key, Duration::from_secs(30))?;
    let tools = HcpTools::new(client.clone(), &env);
    let in_flight = tools.in_flight.clone();

    // stdout carries the protocol; anything for a human goes to stderr.
    eprintln!("HCP MCP server ready ({})", env.base_url);
    let service = tools.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;

    // The client is gone. Nobody can receive answers to questions still being waited on.
    let abandoned: Vec<String> = in_flight.lock().unwrap().drain().collect();
    for request_id in abandoned {
        let _ = tokio::time::timeout(Duration::from_secs(3), client.cancel(&request_id)).await;
    }
    Ok(())
}

// ── Tool definitions ────────────────────────────────────────────────────────

fn schema(value: Value) -> Arc<JsonObject> {
    Arc::new(value.as_object().cloned().unwrap_or_default())
}

fn routing_hints() -> Value {
    json!({
        "type": "object",
        "description": "Optional. Leave out to use the configured default responder and channel.",
        "properties": {
            "responder_id": { "type": "string", "description": "Who should respond. Use the configured default unless you know a better person." },
            "slack_channel_id": { "type": "string", "description": "Slack channel ID to post in, instead of the default approvals channel." },
        },
    })
}

fn request_properties(described: bool) -> Value {
    let describe = |text: &str| if described { json!(text) } else { Value::Null };
    let mut properties = json!({
        "intent": {
            "type": "string",
            "enum": INTENTS,
            "description": describe(concat!(
                "APPROVAL — you need explicit go/no-go before acting\n",
                "CLARIFICATION — you have ambiguity you cannot resolve alone\n",
                "DECISION — you need the human to choose between options\n",
                "INPUT — you need information only the human can provide\n",
                "REVIEW — you want human eyes on something before continuing\n",
                "ESCALATION — situation exceeds your confidence threshold\n",
                "NOTIFICATION — informational, no response required (use hcp_notify instead for this)",
            )),
        },
        "urgency": {
            "type": "string",
            "enum": URGENCIES,
            "description": describe("CRITICAL: blocks all progress. HIGH: needed soon. MEDIUM: needed before next major step. LOW: convenient but not urgent."),
        },
        "context_package": {
            "type": "object",
            "required": ["summary"],
            "properties": {
                "summary": { "type": "string", "description": describe("One clear sentence or paragraph: what the situation is and exactly what you need from the human. Write as if they have no other context.") },
                "detail": { "type": "string", "description": describe("Full context, relevant excerpts, options considered, and why you can't proceed without input.") },
                "metadata": { "type": "object", "description": describe("Structured data: options list, file paths, command being considered, etc.") },
            },
        },
        "timeout_policy": {
            "type": "object",
            "required": ["timeout_seconds", "fallback"],
            "properties": {
                "timeout_seconds": { "type": "number", "description": describe("How long to wait. 300 (5m) for CRITICAL, 1800 (30m) for HIGH, 3600 (1h) for MEDIUM/LOW.") },
                "fallback": {
                    "type": "string",
                    "enum": FALLBACKS,
                    "description": describe("What to do if no response. Default to AUTO_REJECT for approvals (safe), AUTO_APPROVE for low-risk clarifications."),
                },
                "escalation_responder_id": { "type": "string", "description": describe("Who to escalate to if fallback=ESCALATE.") },
            },
        },
        "routing_hints": routing_hints(),
        "trace_id": { "type": "string", "description": describe("ID linking this CR to the current agent run/trace.") },
        "idempotency_key": { "type": "string", "description": describe("Prevents duplicate CRs on retry.") },
    });
    drop_null_descriptions(&mut properties);
    properties
}

fn drop_null_descriptions(value: &mut Value) {
    if let Some(object) = value.as_object_mut() {
        object.retain(|key, member| !(key == "description" && member.is_null()));
        object.values_mut().for_each(drop_null_descriptions);
    }
}

/// The tools, in a fixed order (clients cache the list).
pub fn tools() -> Vec<Tool> {
    let request_schema = |described: bool| {
        schema(json!({
            "type": "object",
            "required": ["intent", "urgency", "context_package", "timeout_policy"],
            "properties": request_properties(described),
        }))
    };
    let by_request_id = |description: &str| {
        schema(json!({
            "type": "object",
            "required": ["request_id"],
            "properties": { "request_id": { "type": "string", "description": description } },
        }))
    };

    vec![
        Tool::new(
            "hcp_coordinate",
            concat!(
                "Route a human interaction moment through HCP and WAIT for the response before continuing.\n",
                "\n",
                "Use this tool — instead of asking in the chat — whenever you would:\n",
                "  • Ask the user a clarifying question before proceeding\n",
                "  • Request permission to take an action\n",
                "  • Surface ambiguity you cannot resolve autonomously\n",
                "  • Ask the user to choose between options\n",
                "  • Flag a risk or concern and ask how to proceed\n",
                "  • Need a piece of information only the human can supply\n",
                "\n",
                "This is the PRIMARY tool for all human interaction. When in doubt, use this.\n",
                "Blocks until the human responds in Slack, then returns their answer.",
            ),
            request_schema(true),
        ),
        Tool::new(
            "hcp_notify",
            concat!(
                "Send a one-way notification to the human and return immediately. Do NOT wait for a response.\n",
                "\n",
                "Use this for:\n",
                "  • Progress updates during long-running tasks ('Starting database migration...')\n",
                "  • Completion announcements ('Finished. 42 files processed.')\n",
                "  • Informational flags that don't require action ('Found 3 deprecated dependencies, continuing.')\n",
                "  • Session start/end announcements\n",
                "\n",
                "If you need the human to DO something or ANSWER something, use hcp_coordinate instead.",
            ),
            schema(json!({
                "type": "object",
                "required": ["summary"],
                "properties": {
                    "summary": { "type": "string", "description": "What happened or what you're about to do. One or two sentences." },
                    "detail": { "type": "string", "description": "Optional extended context." },
                    "metadata": { "type": "object", "description": "Optional structured data (file counts, durations, etc.)." },
                    "urgency": {
                        "type": "string",
                        "enum": ["HIGH", "MEDIUM", "LOW"],
                        "description": "LOW for routine updates. MEDIUM for notable events. HIGH for unexpected situations that are FYI but not blocking.",
                        "default": "LOW",
                    },
                },
            })),
        ),
        Tool::new(
            "hcp_submit",
            "Submit a coordination request and return the request_id immediately without waiting. Use when you want to surface something to the human but continue working in parallel. Check the response later with hcp_poll.",
            request_schema(false),
        ),
        Tool::new(
            "hcp_poll",
            "Check the current state and response of a previously submitted coordination request (from hcp_submit). Returns the response_data once the human has responded.",
            by_request_id("The request_id returned by hcp_submit."),
        ),
        Tool::new(
            "hcp_cancel",
            "Cancel a pending coordination request. Use if the question is no longer relevant (e.g. the agent found another way to proceed).",
            by_request_id("The request_id to cancel."),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tools_with(slack_channel: Option<&str>) -> HcpTools {
        let env = AgentEnv {
            base_url: "http://127.0.0.1:1".to_string(),
            api_key: Some("hcp_test".to_string()),
            responder: "isaac".to_string(),
            slack_channel: slack_channel.map(str::to_string),
            poll: Duration::from_millis(10),
            progress: Duration::from_secs(30),
        };
        HcpTools::new(HcpClient::new(&env.base_url, "hcp_test", Duration::from_secs(1)).unwrap(), &env)
    }

    fn object(value: Value) -> JsonObject {
        value.as_object().cloned().unwrap()
    }

    #[test]
    fn the_five_tools_are_listed_in_a_fixed_order_with_object_schemas() {
        let names: Vec<_> = tools().iter().map(|tool| tool.name.to_string()).collect();
        assert_eq!(names, ["hcp_coordinate", "hcp_notify", "hcp_submit", "hcp_poll", "hcp_cancel"]);
        for tool in tools() {
            assert_eq!(tool.input_schema["type"], "object", "{}", tool.name);
            assert!(tool.description.is_some_and(|d| !d.is_empty()));
        }
    }

    #[test]
    fn routing_is_optional_for_the_model_and_the_schema_has_no_null_descriptions() {
        let submit = tools().into_iter().find(|tool| tool.name == "hcp_submit").unwrap();
        let required = submit.input_schema["required"].as_array().unwrap();
        assert!(!required.contains(&json!("routing_hints")));
        assert!(!serde_json::to_string(&submit.input_schema).unwrap().contains("null"));

        let coordinate = tools().into_iter().find(|tool| tool.name == "hcp_coordinate").unwrap();
        assert!(coordinate.input_schema["properties"]["intent"]["description"].as_str().unwrap().contains("APPROVAL"));
    }

    #[test]
    fn missing_routing_is_filled_from_the_environment() {
        let filled = tools_with(Some("C_OPS")).with_default_routing(object(json!({ "intent": "APPROVAL" })));
        assert_eq!(filled["routing_hints"], json!({ "responder_id": "isaac", "slack_channel_id": "C_OPS" }));

        let no_channel =
            tools_with(None).with_default_routing(object(json!({ "routing_hints": { "responder_id": " " } })));
        assert_eq!(no_channel["routing_hints"], json!({ "responder_id": "isaac" }));
    }

    #[test]
    fn routing_the_caller_chose_is_kept() {
        let chosen = json!({ "routing_hints": { "responder_id": "cto", "slack_channel_id": "C_EXEC" } });
        let kept = tools_with(Some("C_OPS")).with_default_routing(object(chosen.clone()));
        assert_eq!(kept, chosen);
    }

    #[test]
    fn null_context_members_are_not_sent() {
        let body = strip_nulls(json!({ "context_package": { "summary": "s", "detail": null, "metadata": null } }));
        assert_eq!(body, json!({ "context_package": { "summary": "s" } }));
    }
}
