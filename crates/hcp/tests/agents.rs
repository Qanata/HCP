//! The agent-side commands, `hcp mcp` and `hcp hook`, run as real processes against a real
//! server, the way an agent harness runs them.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Output, Stdio};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use common::{CHANNEL, Call, Harness, Options, SlackMode, block_text, eventually};
use hcp::domain::{CoordinationRequest, Intent, State};
use hcp::slack::{ACTION_APPROVE, ACTION_REJECT, ACTION_RESPOND};
use hcp::store::RequestFilter;

/// The binary with only the environment a test gives it.
fn hcp_command(hcp: &Harness, key: Option<&str>) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hcp"));
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HCP_BASE_URL", hcp.app.base_url())
        .env("HCP_RESPONDER", "isaac")
        .env("HCP_POLL_INTERVAL_MS", "50");
    if let Some(key) = key {
        command.env("HCP_API_KEY", key);
    }
    command
}

async fn all_requests(hcp: &Harness) -> Vec<CoordinationRequest> {
    let filter = RequestFilter { limit: 500, ..RequestFilter::default() };
    hcp.app.engine.store().list_requests(&filter).await.unwrap()
}

fn posted_request_id(posted: &Call) -> String {
    posted.body["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|block| block["type"] == "actions")
        .and_then(|block| block["elements"][0]["value"].as_str())
        .expect("posted message has no button")
        .to_string()
}

// ── hcp hook ────────────────────────────────────────────────────────────────

fn bash_event(command: &str) -> Value {
    json!({
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_input": { "command": command },
        "session_id": "sess-42",
        "cwd": "/Users/isaac/project",
        "tool_use_id": "toolu_01",
    })
}

/// Starts `hcp hook` with an event on stdin; join the handle for its exit status and output.
fn start_hook(mut command: Command, event: &Value) -> std::thread::JoinHandle<Output> {
    let mut child =
        command.arg("hook").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    child.stdin.take().unwrap().write_all(event.to_string().as_bytes()).unwrap();
    std::thread::spawn(move || child.wait_with_output().unwrap())
}

async fn finish(hook: std::thread::JoinHandle<Output>) -> (Option<i32>, String, String) {
    let output = tokio::task::spawn_blocking(move || hook.join().unwrap()).await.unwrap();
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

async fn run_hook(command: Command, event: &Value) -> (Option<i32>, String, String) {
    finish(start_hook(command, event)).await
}

#[tokio::test(flavor = "multi_thread")]
async fn the_hook_gates_destructive_commands_on_a_human_decision() {
    let hcp = Harness::start(Options::default()).await;
    let key = hcp.key("claude-code", &[]).await;

    let approved = start_hook(hcp_command(&hcp, Some(&key)), &bash_event("rm -rf build/"));
    // A harmless prefix must not let the dangerous half through unasked.
    let rejected = start_hook(hcp_command(&hcp, Some(&key)), &bash_event("echo deploying && git push --force"));

    let posts = hcp.slack.wait_calls("chat.postMessage", 2).await;
    for post in &posts {
        assert_eq!(post.body["channel"], CHANNEL);
        let action = if block_text(&post.body).contains("rm -rf build/") { ACTION_APPROVE } else { ACTION_REJECT };
        hcp.slack_interaction(&hcp.click(action, &posted_request_id(post), "U_ISAAC", "isaac")).await;
    }

    // Approved: exit 0 and nothing on stdout, so Claude Code's own permission flow continues.
    let (code, stdout, stderr) = finish(approved).await;
    assert_eq!((code, stdout.as_str()), (Some(0), ""), "{stderr}");
    assert!(stderr.contains("approved by slack:isaac"));

    // Rejected: exit 2, with the reason on stderr for Claude.
    let (code, stdout, stderr) = finish(rejected).await;
    assert_eq!((code, stdout.as_str()), (Some(2), ""), "{stderr}");
    assert!(stderr.contains("a human reviewer (slack:isaac) did not approve this command"), "{stderr}");

    // The request tells the reviewer which session and directory it came from.
    let requests = all_requests(&hcp).await;
    assert_eq!(requests.len(), 2);
    for cr in &requests {
        assert_eq!(
            (cr.intent, cr.agent_id.as_str(), cr.responder_id.as_str()),
            (Intent::Approval, "claude-code", "isaac")
        );
        assert_eq!(cr.trace_id.as_deref(), Some("sess-42"));
        let metadata = cr.context_package.metadata.as_ref().unwrap();
        assert_eq!(metadata["cwd"], "/Users/isaac/project");
        assert_eq!(metadata["tool_use_id"], "toolu_01");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_hook_blocks_when_nobody_answers_in_time() {
    let hcp = Harness::start(Options::default()).await;
    let key = hcp.key("claude-code", &[]).await;
    // Settings come from an env file named on the command line, which is how the key is
    // kept out of Claude Code's settings.json.
    let dir = tempfile::tempdir().unwrap();
    let env_file = dir.path().join("claude-code.env");
    std::fs::write(&env_file, format!("HCP_API_KEY={key}\nHCP_HOOK_TIMEOUT=1\n")).unwrap();
    let mut command = hcp_command(&hcp, None);
    command.arg("--env-file").arg(&env_file);

    let (code, _, stderr) = run_hook(command, &bash_event("terraform destroy")).await;
    assert_eq!(code, Some(2), "{stderr}");
    assert!(stderr.contains("nobody approved this command within 1s"), "{stderr}");
    // The Slack message no longer offers buttons for a decision that has been made.
    let updated = hcp.slack.wait_calls("chat.update", 1).await.remove(0);
    assert!(block_text(&updated.body).contains("Rejected automatically"));
}

#[tokio::test(flavor = "multi_thread")]
async fn read_only_commands_pass_without_contacting_anyone() {
    let hcp = Harness::start(Options::default()).await;
    let key = hcp.key("claude-code", &[]).await;

    for command in ["ls -la", "git status && git diff", "cat README.md | head -20"] {
        let (code, stdout, stderr) = run_hook(hcp_command(&hcp, Some(&key)), &bash_event(command)).await;
        assert_eq!((code, stdout.as_str(), stderr.as_str()), (Some(0), "", ""), "{command}");
    }
    assert!(all_requests(&hcp).await.is_empty());
    assert!(hcp.slack.calls("chat.postMessage").is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn ordinary_work_is_reported_but_never_waited_on() {
    let hcp = Harness::start(Options::default()).await;
    let key = hcp.key("claude-code", &[]).await;
    let events = [
        bash_event("cargo build --release"),
        json!({ "hook_event_name": "PreToolUse", "tool_name": "Write", "tool_input": { "file_path": "/tmp/a.rs", "content": "fn main() {}" } }),
        json!({ "hook_event_name": "PostToolUse", "tool_name": "Bash", "tool_input": { "command": "cargo build --release" }, "tool_response": { "stdout": "Finished" } }),
        json!({ "hook_event_name": "Notification", "message": "Claude needs your permission", "notification_type": "permission_prompt" }),
        json!({ "hook_event_name": "Stop", "last_assistant_message": "The build passes." }),
    ];
    for event in &events {
        let (code, stdout, _) = run_hook(hcp_command(&hcp, Some(&key)), event).await;
        assert_eq!((code, stdout.as_str()), (Some(0), ""), "{event}");
    }

    let mut summaries: Vec<String> = all_requests(&hcp)
        .await
        .into_iter()
        .inspect(|cr| assert_eq!(cr.intent, Intent::Notification))
        .map(|cr| cr.context_package.summary)
        .collect();
    summaries.sort();
    assert_eq!(
        summaries,
        [
            "Agent notification: Claude needs your permission",
            "Claude Code finished responding: The build passes.",
            "Completed Bash: `cargo build --release`",
            "Running: `cargo build --release`",
            "write /tmp/a.rs",
        ]
    );
    // Notifications are logged, not posted, unless a channel is named for them.
    assert!(hcp.slack.calls("chat.postMessage").is_empty());

    // In gate_only mode none of that is sent.
    let before = all_requests(&hcp).await.len();
    for event in &events {
        let mut command = hcp_command(&hcp, Some(&key));
        command.env("HCP_HOOK_MODE", "gate_only");
        assert_eq!(run_hook(command, event).await.0, Some(0));
    }
    assert_eq!(all_requests(&hcp).await.len(), before);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_gate_stays_closed_when_approval_cannot_be_requested() {
    let hcp = Harness::start(Options { slack: SlackMode::Off, ..Options::default() }).await;
    let key = hcp.key("claude-code", &[]).await;
    let gated = bash_event("rm -rf /var/data");
    let unreachable = |hcp: &Harness| {
        let mut command = hcp_command(hcp, Some(&key));
        command.env("HCP_BASE_URL", "http://127.0.0.1:1");
        command
    };

    // Server unreachable: blocked, and the reason says why.
    let (code, _, stderr) = run_hook(unreachable(&hcp), &gated).await;
    assert_eq!(code, Some(2), "{stderr}");
    assert!(stderr.contains("approval could not be requested"), "{stderr}");

    // No API key, or a revoked one: blocked.
    assert_eq!(run_hook(hcp_command(&hcp, None), &gated).await.0, Some(2));
    assert_eq!(run_hook(hcp_command(&hcp, Some("hcp_not_a_real_key")), &gated).await.0, Some(2));

    // An env file that cannot be read: blocked, not crashed.
    let mut missing_env = hcp_command(&hcp, Some(&key));
    missing_env.args(["--env-file", "/nonexistent/hcp.env"]);
    assert_eq!(run_hook(missing_env, &gated).await.0, Some(2));

    // Failing open is possible, but only by asking for it.
    let mut fail_open = unreachable(&hcp);
    fail_open.env("HCP_HOOK_ON_ERROR", "allow");
    assert_eq!(run_hook(fail_open, &gated).await.0, Some(0));

    // None of this gets in the way of work that needs no approval, and a Stop event is
    // never answered with exit 2 (which would tell Claude to keep going).
    assert_eq!(run_hook(unreachable(&hcp), &bash_event("ls")).await.0, Some(0));
    assert_eq!(run_hook(unreachable(&hcp), &bash_event("cargo build")).await.0, Some(0));
    assert_eq!(run_hook(unreachable(&hcp), &json!({ "hook_event_name": "Stop" })).await.0, Some(0));
    assert_eq!(run_hook(hcp_command(&hcp, None), &json!({ "hook_event_name": "Stop" })).await.0, Some(0));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_supervised_project_cannot_reconfigure_the_hook() {
    let hcp = Harness::start(Options { slack: SlackMode::Off, ..Options::default() }).await;
    let key = hcp.key("claude-code", &[]).await;

    // The agent plants a .env in its working directory to make the gate fail open.
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join(".env"), "HCP_HOOK_ON_ERROR=allow\nHCP_HOOK_MODE=gate_only\n").unwrap();
    let mut command = hcp_command(&hcp, Some(&key));
    command.current_dir(project.path()).env("HCP_BASE_URL", "http://127.0.0.1:1");

    assert_eq!(run_hook(command, &bash_event("rm -rf ~")).await.0, Some(2));
}

#[tokio::test(flavor = "multi_thread")]
async fn hook_mistakes_never_exit_with_the_blocking_code() {
    let hcp = Harness::start(Options { slack: SlackMode::Off, ..Options::default() }).await;
    let run = |args: &'static [&'static str], stdin: &'static str| {
        let mut command = hcp_command(&hcp, None);
        command.args(args).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null());
        async move {
            tokio::task::spawn_blocking(move || {
                let mut child = command.spawn().unwrap();
                child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
                child.wait().unwrap().code()
            })
            .await
            .unwrap()
        }
    };
    // Exit code 2 means "block" (or, for Stop, "keep working"). Typos and garbage are errors,
    // not decisions.
    assert_eq!(run(&["hok"], "").await, Some(64));
    assert_eq!(run(&["hook", "--no-such-flag"], "").await, Some(64));
    assert_eq!(run(&["hook"], "this is not json").await, Some(1));
    assert_eq!(run(&["--version"], "").await, Some(0));
}

// ── hcp mcp ─────────────────────────────────────────────────────────────────

/// A running `hcp mcp` process and everything it has written to stdout.
struct Mcp {
    child: Child,
    stdin: Option<ChildStdin>,
    messages: Arc<Mutex<Vec<Value>>>,
}

impl Mcp {
    fn start(hcp: &Harness, key: &str, cwd: Option<&Path>) -> Mcp {
        let mut command = hcp_command(hcp, Some(key));
        command.arg("mcp").env("HCP_PROGRESS_INTERVAL_MS", "100");
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        let mut child = command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
        let stdin = child.stdin.take();
        let messages = Arc::new(Mutex::new(Vec::new()));
        let (sink, stdout) = (messages.clone(), child.stdout.take().unwrap());
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                // Everything on stdout must be a protocol message.
                sink.lock().unwrap().push(serde_json::from_str(&line).expect("non-JSON output on stdout"));
            }
        });
        Mcp { child, stdin, messages }
    }

    fn send(&mut self, message: Value) {
        writeln!(self.stdin.as_mut().unwrap(), "{message}").unwrap();
    }

    /// Performs the `initialize` handshake of the older protocol revisions.
    async fn handshake(&mut self, version: &str) -> Value {
        self.send(json!({
            "jsonrpc": "2.0", "id": "init", "method": "initialize",
            "params": { "protocolVersion": version, "capabilities": {}, "clientInfo": { "name": "test", "version": "1" } },
        }));
        let reply = self.reply(&json!("init")).await;
        self.send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
        reply
    }

    fn call(&mut self, id: i64, tool: &str, arguments: Value) {
        self.send(json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": { "name": tool, "arguments": arguments, "_meta": { "progressToken": format!("progress-{id}") } },
        }));
    }

    async fn reply(&self, id: &Value) -> Value {
        eventually(|| async { self.messages.lock().unwrap().iter().find(|m| m.get("id") == Some(id)).cloned() })
            .await
            .unwrap_or_else(|| panic!("no reply to request {id}"))
    }

    /// The JSON a successful tool call returned.
    async fn result(&self, id: i64) -> Value {
        let reply = self.reply(&json!(id)).await;
        assert!(reply["result"]["isError"].as_bool() != Some(true), "tool failed: {reply}");
        serde_json::from_str(reply["result"]["content"][0]["text"].as_str().expect("no text content")).unwrap()
    }

    fn progress_notes(&self, id: i64) -> usize {
        let token = json!(format!("progress-{id}"));
        self.messages
            .lock()
            .unwrap()
            .iter()
            .filter(|m| m["method"] == "notifications/progress" && m["params"]["progressToken"] == token)
            .count()
    }

    /// Closes stdin, as a client does to shut the server down, and waits for it to exit.
    async fn close(mut self) -> Option<i32> {
        drop(self.stdin.take());
        for _ in 0..200 {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.code();
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("hcp mcp did not exit after stdin closed");
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn question() -> Value {
    json!({
        "intent": "CLARIFICATION",
        "urgency": "MEDIUM",
        "context_package": { "summary": "Which database should we migrate to?" },
        "timeout_policy": { "timeout_seconds": 600, "fallback": "AUTO_REJECT" },
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn coordinate_blocks_until_a_human_answers_and_stays_responsive_meanwhile() {
    let hcp = Harness::start(Options::default()).await;
    let key = hcp.key("hermes", &[]).await;
    let mut mcp = Mcp::start(&hcp, &key, None);

    // The version the client asks for is the version it gets.
    let init = mcp.handshake("2025-06-18").await;
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(init["result"]["serverInfo"]["name"], "hcp");
    assert!(init["result"]["capabilities"]["tools"].is_object());

    mcp.send(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }));
    let listed = mcp.reply(&json!(1)).await;
    let names: Vec<&str> =
        listed["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["hcp_coordinate", "hcp_notify", "hcp_submit", "hcp_poll", "hcp_cancel"]);

    // No routing given: the question goes to the configured responder in the default channel.
    mcp.call(2, "hcp_coordinate", question());
    let posted = hcp.slack.wait_calls("chat.postMessage", 1).await.remove(0);
    assert_eq!(posted.body["channel"], CHANNEL);
    assert!(block_text(&posted.body).contains("Which database should we migrate to?"));
    assert!(block_text(&posted.body).contains("`isaac`"));

    // While that call is blocked, the server still answers other requests...
    mcp.send(json!({ "jsonrpc": "2.0", "id": 3, "method": "ping" }));
    assert_eq!(mcp.reply(&json!(3)).await["result"], json!({}));
    // ...and reports that it is alive, so the client does not give up on a long wait.
    eventually(|| async { (mcp.progress_notes(2) >= 2).then_some(()) }).await.expect("no progress notifications");
    assert!(mcp.messages.lock().unwrap().iter().all(|m| m.get("id") != Some(&json!(2))));

    let id = posted_request_id(&posted);
    hcp.slack_interaction(&hcp.click(ACTION_RESPOND, &id, "U_ISAAC", "isaac")).await;
    hcp.slack_interaction(&hcp.modal_submission(&id, "U_ISAAC", "isaac", "Stay on Postgres.")).await;

    let answer = mcp.result(2).await;
    assert_eq!(answer["request_id"], id);
    assert_eq!(answer["state"], "DELIVERED");
    assert_eq!(answer["response_data"], json!({ "text": "Stay on Postgres." }));
    assert_eq!(answer["responded_by"], "slack:isaac");

    assert_eq!(mcp.close().await, Some(0));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_other_tools_notify_submit_poll_and_cancel() {
    let hcp = Harness::start(Options { slack: SlackMode::Off, ..Options::default() }).await;
    let key = hcp.key("hermes", &[]).await;
    let responder = hcp.key("console", &["respond"]).await;
    let mut mcp = Mcp::start(&hcp, &key, None);
    // The oldest revision, which is all the previous Node server spoke.
    assert_eq!(mcp.handshake("2024-11-05").await["result"]["protocolVersion"], "2024-11-05");

    mcp.call(1, "hcp_notify", json!({ "summary": "Starting the migration", "urgency": "MEDIUM" }));
    let notified = mcp.result(1).await;
    let notification = hcp.peek(notified["request_id"].as_str().unwrap()).await;
    assert_eq!((notification.intent, notification.responder_id.as_str()), (Intent::Notification, "isaac"));
    assert_eq!(notification.context_package.summary, "Starting the migration");

    mcp.call(2, "hcp_submit", question());
    let submitted = mcp.result(2).await;
    let id = submitted["request_id"].as_str().unwrap().to_string();
    hcp.wait_state(&id, State::PendingResponse).await;

    mcp.call(3, "hcp_poll", json!({ "request_id": id }));
    assert_eq!(mcp.result(3).await["state"], "PENDING_RESPONSE");

    let answer = json!({ "response_data": { "text": "MySQL" }, "responded_by": "ops-lead" });
    hcp.post(&format!("/v1/requests/{id}/respond"), &responder, &answer).await;
    mcp.call(4, "hcp_poll", json!({ "request_id": id }));
    let polled = mcp.result(4).await;
    assert_eq!((&polled["state"], &polled["response_data"]), (&json!("DELIVERED"), &json!({ "text": "MySQL" })));

    mcp.call(5, "hcp_submit", question());
    let second = mcp.result(5).await["request_id"].as_str().unwrap().to_string();
    mcp.call(6, "hcp_cancel", json!({ "request_id": second }));
    assert_eq!(mcp.result(6).await, json!({ "status": "cancelled", "request_id": second }));
    assert_eq!(hcp.peek(&second).await.state, State::Cancelled);
}

#[tokio::test(flavor = "multi_thread")]
async fn tool_failures_are_results_the_model_can_read_and_protocol_misuse_is_an_error() {
    let hcp = Harness::start(Options { slack: SlackMode::Off, ..Options::default() }).await;
    let key = hcp.key("hermes", &[]).await;
    let mut mcp = Mcp::start(&hcp, &key, None);
    mcp.handshake("2025-11-25").await;

    // The API's validation message reaches the model, so it can fix its own call.
    let mut invalid = question();
    invalid["intent"] = json!("QUESTION");
    mcp.call(1, "hcp_coordinate", invalid);
    let failed = mcp.reply(&json!(1)).await;
    assert_eq!(failed["result"]["isError"], true);
    let message = failed["result"]["content"][0]["text"].as_str().unwrap();
    assert!(message.contains("HCP API error 400") && message.contains("Validation error"), "{message}");

    mcp.call(2, "hcp_poll", json!({ "request_id": "01DOESNOTEXIST" }));
    assert_eq!(mcp.reply(&json!(2)).await["result"]["isError"], true);

    // Calling a tool that does not exist, or without its required argument, is a protocol error.
    mcp.call(3, "hcp_nonexistent", json!({}));
    assert_eq!(mcp.reply(&json!(3)).await["error"]["code"], -32602);
    mcp.call(4, "hcp_cancel", json!({}));
    assert_eq!(mcp.reply(&json!(4)).await["error"]["code"], -32602);
    mcp.send(json!({ "jsonrpc": "2.0", "id": 5, "method": "resources/list" }));
    assert!(
        mcp.reply(&json!(5)).await.get("error").is_some()
            || mcp.reply(&json!(5)).await["result"]["resources"] == json!([])
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_question_nobody_is_waiting_on_is_withdrawn() {
    let hcp = Harness::start(Options::default()).await;
    let key = hcp.key("hermes", &[]).await;

    // The client cancels the tool call (the user interrupted the agent, say).
    let mut mcp = Mcp::start(&hcp, &key, None);
    mcp.handshake("2025-06-18").await;
    mcp.call(7, "hcp_coordinate", question());
    let first = posted_request_id(&hcp.slack.wait_calls("chat.postMessage", 1).await[0]);
    mcp.send(json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": 7, "reason": "user interrupt" } }));
    hcp.wait_state(&first, State::Cancelled).await;

    // The client goes away entirely while a call is blocked.
    mcp.call(8, "hcp_coordinate", question());
    let second = posted_request_id(&hcp.slack.wait_calls("chat.postMessage", 2).await[1]);
    assert_eq!(mcp.close().await, Some(0));
    hcp.wait_state(&second, State::Cancelled).await;

    // Either way the Slack message stops asking.
    let updates = hcp.slack.wait_calls("chat.update", 2).await;
    assert!(updates.iter().all(|update| block_text(&update.body).contains("*Cancelled*")));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_stateless_protocol_revision_works_without_a_handshake() {
    let hcp = Harness::start(Options { slack: SlackMode::Off, ..Options::default() }).await;
    let key = hcp.key("hermes", &[]).await;
    // Run from a directory with a hostile .env, which must be ignored.
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join(".env"), "HCP_SLACK_CHANNEL=C_ELSEWHERE\n").unwrap();
    let mut mcp = Mcp::start(&hcp, &key, Some(project.path()));

    let meta = |version: &str| {
        json!({
            "io.modelcontextprotocol/protocolVersion": version,
            "io.modelcontextprotocol/clientInfo": { "name": "test", "version": "1" },
            "io.modelcontextprotocol/clientCapabilities": {},
        })
    };

    // A client that speaks both eras probes with server/discover first.
    mcp.send(json!({ "jsonrpc": "2.0", "id": "probe", "method": "server/discover", "params": { "_meta": meta("2026-07-28") } }));
    let discovered = mcp.reply(&json!("probe")).await;
    assert_eq!(discovered["result"]["resultType"], "complete");
    assert!(discovered["result"]["supportedVersions"].as_array().unwrap().contains(&json!("2026-07-28")));
    assert!(discovered["result"]["capabilities"]["tools"].is_object());

    mcp.send(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": { "_meta": meta("2026-07-28") } }));
    let listed = mcp.reply(&json!(1)).await;
    assert_eq!(listed["result"]["resultType"], "complete");
    assert_eq!(listed["result"]["tools"].as_array().unwrap().len(), 5);
    assert!(listed["result"]["ttlMs"].is_number() && listed["result"]["cacheScope"].is_string());

    mcp.send(json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": { "name": "hcp_notify", "arguments": { "summary": "Stateless hello" }, "_meta": meta("2026-07-28") },
    }));
    let called = mcp.reply(&json!(2)).await;
    assert_eq!(called["result"]["resultType"], "complete");
    let result: Value = serde_json::from_str(called["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    let created = hcp.peek(result["request_id"].as_str().unwrap()).await;
    assert_eq!((created.context_package.summary.as_str(), created.responder_id.as_str()), ("Stateless hello", "isaac"));
    assert_eq!(created.routing_hints.slack_channel_id, None);

    // A version the server does not speak is refused with the list of ones it does.
    mcp.send(json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/list", "params": { "_meta": meta("2099-01-01") } }));
    let refused = mcp.reply(&json!(3)).await;
    assert_eq!(refused["error"]["code"], -32022);
    assert!(refused["error"]["data"]["supported"].as_array().unwrap().contains(&json!("2026-07-28")));
}
