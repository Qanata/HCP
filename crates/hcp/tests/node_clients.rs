//! The Claude Code hook and the MCP server now exist in Rust (`hcp hook`, `hcp mcp`; see
//! `agents.rs`). The Node originals (`scripts/hcp-hook.js`, `src/mcp-server.ts`) are still in
//! the repository, and these tests drive them, unmodified, against this server to show the
//! API stays compatible with them. They are skipped when Node or the scripts are absent,
//! and should be deleted together with the Node code.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use common::{CHANNEL, Call, Harness, Options, block_text, eventually};
use hcp::slack::{ACTION_APPROVE, ACTION_REJECT, ACTION_RESPOND};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

/// Whether the Node pieces a test needs are available; says why not when skipping.
fn available(test: &str, paths: &[&str]) -> bool {
    let node = Command::new("node").arg("--version").output().is_ok_and(|output| output.status.success());
    let missing: Vec<_> = paths.iter().filter(|path| !repo_root().join(path).exists()).collect();
    if node && missing.is_empty() {
        return true;
    }
    eprintln!("skipping {test}: node available = {node}, missing files = {missing:?}");
    false
}

fn request_id(posted: &Call) -> String {
    posted.body["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|block| block["type"] == "actions")
        .and_then(|block| block["elements"][0]["value"].as_str())
        .expect("posted message has no button")
        .to_string()
}

/// Runs the Claude Code hook for one Bash command and returns how it exited.
fn run_hook(hcp: &Harness, key: &str, command: &str) -> std::thread::JoinHandle<Output> {
    let mut child = Command::new("node")
        .arg(repo_root().join("scripts/hcp-hook.js"))
        .env("HCP_BASE_URL", hcp.app.base_url())
        .env("HCP_API_KEY", key)
        .env("HCP_RESPONDER", "isaac")
        .env("HCP_HOOK_TIMEOUT", "60")
        .env("HCP_HOOK_MODE", "gate_only")
        .env_remove("HCP_SLACK_CHANNEL")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let event = json!({ "hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_input": { "command": command } });
    child.stdin.take().unwrap().write_all(event.to_string().as_bytes()).unwrap();
    std::thread::spawn(move || child.wait_with_output().unwrap())
}

#[tokio::test(flavor = "multi_thread")]
async fn the_existing_claude_code_hook_gates_commands_through_slack() {
    if !available("hook test", &["scripts/hcp-hook.js"]) {
        return;
    }
    let hcp = Harness::start(Options::default()).await;
    let key = hcp.key("claude-code", &[]).await;

    let allowed = run_hook(&hcp, &key, "rm -rf build/");
    let blocked = run_hook(&hcp, &key, "git push --force origin main");

    // The hook asks for the portal (it has no Slack channel configured); the request goes
    // to the Slack approvals channel regardless.
    let posts = hcp.slack.wait_calls("chat.postMessage", 2).await;
    for post in &posts {
        assert_eq!(post.body["channel"], CHANNEL);
        let action = if block_text(&post.body).contains("rm -rf build/") { ACTION_APPROVE } else { ACTION_REJECT };
        hcp.slack_interaction(&hcp.click(action, &request_id(post), "U_ISAAC", "isaac")).await;
    }

    let allowed = tokio::task::spawn_blocking(move || allowed.join().unwrap()).await.unwrap();
    assert!(allowed.status.success(), "hook should allow: {}", String::from_utf8_lossy(&allowed.stderr));
    assert!(String::from_utf8_lossy(&allowed.stderr).contains("Approved by slack:isaac"));

    let blocked = tokio::task::spawn_blocking(move || blocked.join().unwrap()).await.unwrap();
    assert_eq!(blocked.status.code(), Some(1));
    let verdict: Value = serde_json::from_slice(&blocked.stdout).unwrap();
    assert_eq!(verdict["decision"], "block");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_existing_mcp_server_coordinates_through_slack() {
    if !available("MCP test", &["src/mcp-server.ts", "node_modules/.bin/tsx"]) {
        return;
    }
    let hcp = Harness::start(Options::default()).await;
    let key = hcp.key("hermes", &[]).await;

    let mut server = Command::new(repo_root().join("node_modules/.bin/tsx"))
        .arg("src/mcp-server.ts")
        .current_dir(repo_root())
        .env("HCP_BASE_URL", hcp.app.base_url())
        .env("HCP_API_KEY", &key)
        .env("HCP_RESPONDER", "isaac")
        .env_remove("HCP_SLACK_CHANNEL")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = server.stdin.take().unwrap();
    let replies = Arc::new(Mutex::new(Vec::<Value>::new()));
    let (sink, stdout) = (replies.clone(), server.stdout.take().unwrap());
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Ok(reply) = serde_json::from_str(&line) {
                sink.lock().unwrap().push(reply);
            }
        }
    });
    let reply = |id: u64| {
        let replies = replies.clone();
        async move {
            eventually(|| async { replies.lock().unwrap().iter().find(|reply| reply["id"] == id).cloned() })
                .await
                .unwrap_or_else(|| panic!("no MCP reply to request {id}"))
        }
    };

    let call = json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": { "name": "hcp_coordinate", "arguments": {
            "intent": "CLARIFICATION",
            "urgency": "MEDIUM",
            "context_package": { "summary": "Which database should we migrate to?" },
            "timeout_policy": { "timeout_seconds": 60, "fallback": "AUTO_REJECT" },
            "routing_hints": { "responder_id": "isaac" },
        } },
    });
    writeln!(stdin, "{}", json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" })).unwrap();
    writeln!(stdin, "{call}").unwrap();
    assert!(reply(1).await["result"]["tools"].as_array().unwrap().iter().any(|tool| tool["name"] == "hcp_coordinate"));

    // The question appears in Slack; a human answers it in the modal.
    let posted = hcp.slack.wait_calls("chat.postMessage", 1).await.remove(0);
    assert!(block_text(&posted.body).contains("Which database should we migrate to?"));
    let id = request_id(&posted);
    hcp.slack_interaction(&hcp.click(ACTION_RESPOND, &id, "U_ISAAC", "isaac")).await;
    hcp.slack_interaction(&hcp.modal_submission(&id, "U_ISAAC", "isaac", "Stay on Postgres.")).await;

    // The blocked tool call returns the human's answer to the agent.
    let result = reply(2).await;
    assert!(result["result"]["isError"].is_null(), "{result}");
    let returned: Value = serde_json::from_str(result["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(returned["state"], "DELIVERED");
    assert_eq!(returned["response_data"], json!({ "text": "Stay on Postgres." }));
    assert_eq!(returned["responded_by"], "slack:isaac");

    drop(stdin);
    let _ = server.kill();
    let _ = server.wait();
}
