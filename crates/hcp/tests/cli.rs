//! Runs the real `hcp` binary the way it runs on the Mac: configured from an env file,
//! talking to Slack over Socket Mode, logging to a vault, stopped with SIGTERM.

mod common;

use std::net::{Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

use chrono::Local;
use reqwest::StatusCode;
use serde_json::{Value, json};

use common::{APP_TOKEN, BOT_TOKEN, CHANNEL, MockSlack, MockSocket, TestDb, approval, eventually};
use hcp::slack::ACTION_APPROVE;

/// A running server process, killed if the test ends early.
struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The binary with a clean environment, so nothing leaks in from the developer's shell.
fn hcp(env_file: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_hcp"));
    command.env_clear().env("PATH", std::env::var_os("PATH").unwrap_or_default()).arg("--env-file").arg(env_file);
    command
}

fn stdout(output: &Output) -> String {
    assert!(output.status.success(), "command failed: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn free_port() -> u16 {
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap().local_addr().unwrap().port()
}

fn new_vault(dir: &Path) -> PathBuf {
    let vault = dir.join("My Vault");
    std::fs::create_dir_all(vault.join(".obsidian")).unwrap();
    vault
}

fn write_env(dir: &Path, lines: &[String]) -> PathBuf {
    let path = dir.join("hcp.env");
    std::fs::write(&path, lines.join("\n")).unwrap();
    path
}

#[tokio::test(flavor = "multi_thread")]
async fn the_binary_serves_requests_end_to_end_and_shuts_down_cleanly() {
    let db = TestDb::start().await;
    let slack = MockSlack::start().await;
    let socket = MockSocket::start(&slack).await;
    let dir = tempfile::tempdir().unwrap();
    let vault = new_vault(dir.path());
    let port = free_port();
    let base = format!("http://127.0.0.1:{port}");
    let env_file = write_env(
        dir.path(),
        &[
            format!("DATABASE_URL=\"{}\"", db.url),
            format!("HCP_PORT={port}"),
            format!("HCP_OBSIDIAN_VAULT=\"{}\"", vault.display()),
            format!("SLACK_BOT_TOKEN={BOT_TOKEN}"),
            format!("SLACK_APP_TOKEN={APP_TOKEN}"),
            format!("HCP_SLACK_CHANNEL={CHANNEL}"),
            format!("SLACK_API_BASE_URL={}", slack.api_base),
        ],
    );

    // Keys can be created before the server has ever run.
    let created =
        stdout(&hcp(&env_file).args(["keys", "create", "claude-code", "--label", "Claude Code"]).output().unwrap());
    let key = created.split_whitespace().find(|word| word.starts_with("hcp_")).expect("no key printed").to_string();
    assert_eq!(key.len(), 4 + 64);

    let stderr_path = dir.path().join("stderr.log");
    let mut server = Server(
        hcp(&env_file)
            .arg("serve")
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&stderr_path).unwrap())
            .spawn()
            .unwrap(),
    );

    let http = reqwest::Client::new();
    let healthy = eventually(|| async {
        let response = http.get(format!("{base}/health")).send().await.ok()?;
        (response.status() == StatusCode::OK).then_some(())
    })
    .await;
    assert!(healthy.is_some(), "server did not start: {}", std::fs::read_to_string(&stderr_path).unwrap());

    // Submit, approve over Socket Mode, collect.
    let submitted: Value = http
        .post(format!("{base}/v1/requests"))
        .bearer_auth(&key)
        .json(&approval())
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = submitted["request_id"].as_str().unwrap().to_string();
    slack.wait_calls("chat.postMessage", 1).await;
    socket.send(json!({
        "type": "interactive",
        "envelope_id": "env-1",
        "payload": {
            "type": "block_actions",
            "user": { "id": "U_ISAAC", "username": "isaac" },
            "actions": [{ "action_id": ACTION_APPROVE, "value": id }],
        },
    }));
    socket.wait_ack("env-1").await;
    let delivered: Value =
        http.get(format!("{base}/v1/requests/{id}")).bearer_auth(&key).send().await.unwrap().json().await.unwrap();
    assert_eq!(delivered["state"], "DELIVERED");
    assert_eq!(delivered["response_data"]["decision"], "approved");

    // Keys can be listed and revoked while the server runs; revocation takes effect at once.
    let listed = stdout(&hcp(&env_file).args(["keys", "list"]).output().unwrap());
    assert!(listed.contains("claude-code") && listed.contains("active") && !listed.contains(&key));
    let key_id = listed.split_whitespace().next().unwrap().to_string();
    stdout(&hcp(&env_file).args(["keys", "revoke", &key_id]).output().unwrap());
    let refused = http.get(format!("{base}/v1/requests/{id}")).bearer_auth(&key).send().await.unwrap();
    assert_eq!(refused.status(), StatusCode::UNAUTHORIZED);
    assert!(!hcp(&env_file).args(["keys", "revoke", &key_id]).output().unwrap().status.success());

    // SIGTERM is what launchd sends: the server must exit cleanly with its logs written.
    assert!(Command::new("kill").args(["-TERM", &server.0.id().to_string()]).status().unwrap().success());
    let mut exited = None;
    for _ in 0..400 {
        exited = server.0.try_wait().unwrap();
        if exited.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let exited = exited.expect("server did not exit within 20 seconds");
    assert!(exited.success(), "server exited with {exited}: {}", std::fs::read_to_string(&stderr_path).unwrap());

    let stderr = std::fs::read_to_string(&stderr_path).unwrap();
    let today = Local::now().date_naive();
    let service = std::fs::read_to_string(vault.join(format!("HCP/Service/HCP Service {today}.md"))).unwrap();
    for line in [
        "logging to the Obsidian vault",
        "Slack connected",
        "HCP listening",
        "Slack Socket Mode connected",
        "HCP stopped",
    ] {
        assert!(service.contains(line), "service log is missing {line:?}:\n{service}\nstderr:\n{stderr}");
    }
    // Secrets never reach the logs.
    for secret in [BOT_TOKEN, APP_TOKEN, key.as_str()] {
        assert!(!service.contains(secret) && !stderr.contains(secret));
    }

    let audit = std::fs::read_to_string(vault.join(format!("HCP/Log/HCP Log {today}.md"))).unwrap();
    assert_eq!(audit.matches(&format!("[[{id}|")).count(), 7);
    let month = chrono::Utc::now().format("%Y-%m");
    let note = std::fs::read_to_string(vault.join(format!("HCP/Requests/{month}/{id}.md"))).unwrap();
    assert!(note.contains("\nstate: DELIVERED\n") && note.contains("`CR_DELIVERED`"));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_server_refuses_to_start_without_a_usable_vault() {
    let dir = tempfile::tempdir().unwrap();

    // The vault is where the logs go, so it is not optional.
    let unset = write_env(dir.path(), &["DATABASE_URL=postgresql://127.0.0.1:1/hcp".to_string()]);
    let output = hcp(&unset).arg("serve").output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("HCP_OBSIDIAN_VAULT is not set"));

    // A path that does not exist is reported, not silently created.
    let missing = dir.path().join("No Such Vault");
    let wrong = write_env(
        dir.path(),
        &[
            "DATABASE_URL=postgresql://127.0.0.1:1/hcp".to_string(),
            format!("HCP_OBSIDIAN_VAULT=\"{}\"", missing.display()),
        ],
    );
    let output = hcp(&wrong).arg("serve").output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("does not exist"));
    assert!(!missing.exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_start_is_reported_and_recorded_in_the_vault() {
    let dir = tempfile::tempdir().unwrap();
    let vault = new_vault(dir.path());
    // Nothing listens on port 1, so the database connection is refused.
    let env_file = write_env(
        dir.path(),
        &[
            "DATABASE_URL=postgresql://127.0.0.1:1/hcp".to_string(),
            format!("HCP_OBSIDIAN_VAULT=\"{}\"", vault.display()),
        ],
    );

    let output = tokio::time::timeout(Duration::from_secs(30), async { hcp(&env_file).arg("serve").output().unwrap() })
        .await
        .expect("serve hung instead of failing");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot connect to the database"));

    let today = Local::now().date_naive();
    let service = std::fs::read_to_string(vault.join(format!("HCP/Service/HCP Service {today}.md"))).unwrap();
    assert!(service.contains("**ERROR**") && service.contains("cannot connect to the database"));
}
