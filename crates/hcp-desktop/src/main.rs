//! HCP Desktop: a dashboard for the HCP server running on this Mac.
//!
//! It shows metrics, the requests waiting on a person, recent activity, and the health of
//! the Slack, Obsidian and Neon connectors. It is deliberately read-only: requests are
//! answered in Slack, where HCP checks who clicked, and settings stay in
//! `~/.config/hcp/hcp.env`. The admin key it reads with lives in the Keychain and is used
//! only by this Rust side; the web view calls the commands below and receives JSON.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod keychain;
mod server;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, RunEvent, State, WindowEvent};
use tokio::sync::{Notify, OnceCell};

use server::{DEFAULT_BASE_URL, Server, encode};

const TRAY: &str = "hcp";
const SERVICE_LABEL: &str = "com.hcp.server";

struct Backend {
    server: Server,
    /// Set once the key has been read from the Keychain. The read can block on a Keychain
    /// prompt (after the app is rebuilt, macOS asks again), so it happens off the main
    /// thread, after the window is up.
    key_loaded: OnceCell<()>,
    /// Wakes the event follower and the menu-bar counter after the key changes.
    key_changed: Notify,
    /// Wakes the menu-bar counter after a state change.
    changed: Notify,
    /// The last "live" state sent to the web view, for a page that starts listening late.
    live: Mutex<Value>,
}

type Shared<'a> = State<'a, Arc<Backend>>;

impl Backend {
    fn set_live(&self, app: &AppHandle, live: Value) {
        *self.live.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = live.clone();
        let _ = app.emit("hcp-live", live);
    }

    async fn load_key(&self) {
        self.key_loaded
            .get_or_init(|| async {
                let key = tauri::async_runtime::spawn_blocking(keychain::key)
                    .await
                    .unwrap_or_else(|err| Err(err.to_string()));
                match key {
                    Ok(key) => self.server.set_key(key),
                    Err(err) => eprintln!("[hcp-desktop] {err}"),
                }
            })
            .await;
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

/// The `hcp` binary and server env file, as `deploy/macos/install.sh` installs them.
fn hcp_paths() -> (PathBuf, PathBuf) {
    let bin = std::env::var_os("HCP_BIN").map(PathBuf::from).unwrap_or_else(|| home().join(".local/bin/hcp"));
    let env = std::env::var_os("HCP_ENV_FILE").map(PathBuf::from).unwrap_or_else(|| home().join(".config/hcp/hcp.env"));
    (bin, env)
}

// ── Setup ───────────────────────────────────────────────────────────────────

/// Where the dashboard stands: is the server up, and is there a key to read it with.
#[tauri::command]
async fn setup_state(backend: Shared<'_>) -> Result<Value, String> {
    backend.load_key().await;
    let (bin, env) = hcp_paths();
    let health = backend.server.health().await;
    Ok(json!({
        "base_url": backend.server.base_url,
        "has_key": backend.server.key().is_some(),
        "server": match health {
            Ok(health) => json!({ "reachable": true, "health": health }),
            Err(error) => json!({ "reachable": false, "error": error }),
        },
        "hcp_bin": bin, "hcp_bin_found": bin.is_file(),
        "env_file": env, "env_file_found": env.is_file(),
    }))
}

fn adopt_key(backend: &Backend, key: String, key_id: Option<&str>) -> Result<(), String> {
    keychain::store(&key, key_id)?;
    let _ = backend.key_loaded.set(());
    backend.server.set_key(Some(key));
    backend.key_changed.notify_waiters();
    Ok(())
}

/// Creates an admin key with `hcp keys create` and keeps it in the Keychain. The secret
/// goes from the command's output straight to the Keychain; it is never shown.
#[tauri::command]
async fn create_key(backend: Shared<'_>) -> Result<String, String> {
    let (bin, env) = hcp_paths();
    if !bin.is_file() {
        return Err(format!("{} was not found. Run deploy/macos/install.sh first.", bin.display()));
    }
    let output = tokio::process::Command::new(&bin)
        .arg("--env-file")
        .arg(&env)
        .args(["keys", "create", "hcp-desktop", "--scope", "admin", "--label", "HCP Desktop"])
        .output()
        .await
        .map_err(|err| format!("cannot run {}: {err}", bin.display()))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let field = |label: &str| {
        stdout.lines().find_map(|line| line.trim().strip_prefix(label).map(|rest| rest.trim().to_string()))
    };
    let (Some(key), Some(key_id)) = (field("API Key:"), field("Key ID:")) else {
        return Err("`hcp keys create` did not print a key".to_string());
    };
    backend.server.check_key(&key).await?;

    let previous = keychain::key_id().ok().flatten();
    adopt_key(&backend, key, Some(&key_id))?;
    // The dashboard needs one key; retire the one this replaces.
    if let Some(previous) = previous.filter(|previous| *previous != key_id) {
        let _ = tokio::process::Command::new(&bin)
            .arg("--env-file")
            .arg(&env)
            .args(["keys", "revoke", &previous])
            .output()
            .await;
    }
    Ok(key_id)
}

/// Uses a key made elsewhere (`hcp keys create … --scope admin`), after checking it.
#[tauri::command]
async fn save_key(backend: Shared<'_>, key: String) -> Result<(), String> {
    let key = key.trim().to_string();
    if !key.starts_with("hcp_") {
        return Err("That does not look like an HCP API key (they start with hcp_).".to_string());
    }
    backend.server.check_key(&key).await?;
    adopt_key(&backend, key, None)
}

#[tauri::command]
async fn forget_key(backend: Shared<'_>) -> Result<(), String> {
    keychain::forget()?;
    backend.server.set_key(None);
    backend.key_changed.notify_waiters();
    Ok(())
}

// ── Reading ─────────────────────────────────────────────────────────────────

/// Whether the live event stream is connected right now.
#[tauri::command]
fn live(backend: Shared<'_>) -> Value {
    backend.live.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
}

#[tauri::command]
async fn status(backend: Shared<'_>) -> Result<Value, String> {
    backend.server.get("/v1/status").await
}

#[tauri::command]
async fn metrics(backend: Shared<'_>, days: u32, tz: String) -> Result<Value, String> {
    backend.server.get(&format!("/v1/metrics?days={}&tz={}", days.clamp(1, 90), encode(&tz))).await
}

fn valid_id(id: &str) -> Result<&str, String> {
    if !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric()) {
        Ok(id)
    } else {
        Err("not a request ID".to_string())
    }
}

#[tauri::command]
async fn request_detail(backend: Shared<'_>, id: String) -> Result<Value, String> {
    let id = valid_id(&id)?;
    let request = backend.server.get(&format!("/v1/requests/{id}")).await?;
    let events = backend.server.get(&format!("/v1/audit?request_id={id}&limit=500")).await?;
    Ok(json!({ "request": request, "events": events["events"] }))
}

#[tauri::command]
async fn test_connector(backend: Shared<'_>, name: String) -> Result<Value, String> {
    if !["database", "slack", "vault"].contains(&name.as_str()) {
        return Err("unknown connector".to_string());
    }
    backend.server.post(&format!("/v1/status/{name}/test")).await
}

/// The launchd service that runs the server.
#[tauri::command]
async fn service() -> Result<Value, String> {
    let uid = tokio::process::Command::new("id").arg("-u").output().await.map_err(|e| e.to_string())?;
    let target = format!("gui/{}/{SERVICE_LABEL}", String::from_utf8_lossy(&uid.stdout).trim());
    let output =
        tokio::process::Command::new("launchctl").args(["print", &target]).output().await.map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Ok(json!({ "loaded": false, "label": SERVICE_LABEL }));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    // Top-level fields are indented by one tab.
    let field = |name: &str| {
        text.lines().find_map(|line| {
            line.strip_prefix('\t')?.strip_prefix(name)?.strip_prefix(" = ").map(|v| v.trim().to_string())
        })
    };
    Ok(json!({
        "loaded": true,
        "label": SERVICE_LABEL,
        "state": field("state"),
        "pid": field("pid"),
        "runs": field("runs"),
        "last_exit": field("last exit code"),
    }))
}

// ── Opening things ──────────────────────────────────────────────────────────

fn slack_id(text: &str) -> Option<&str> {
    (!text.is_empty() && text.chars().all(|c| c.is_ascii_alphanumeric())).then_some(text)
}

/// Opens one of a fixed set of places. Paths and links are worked out here from the
/// server's own status, never taken from the web view.
#[tauri::command]
async fn open_target(backend: Shared<'_>, target: String, id: Option<String>) -> Result<(), String> {
    let status = || async { backend.server.get("/v1/status").await };
    let argument = match target.as_str() {
        "logs" => home().join("Library/Logs/hcp/stderr.log").to_string_lossy().into_owned(),
        "neon" => "https://console.neon.tech/app/projects".to_string(),
        "vault" => {
            let status = status().await?;
            let path = status["vault"]["vault_path"].as_str().ok_or("No vault is configured")?.to_string();
            format!("obsidian://open?path={}", encode(&path))
        }
        "request_note" => {
            let id = valid_id(id.as_deref().unwrap_or_default())?.to_string();
            let status = status().await?;
            let path = status["vault"]["vault_path"].as_str().ok_or("No vault is configured")?;
            let vault =
                std::path::Path::new(path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            format!("obsidian://search?vault={}&query={}", encode(&vault), encode(&id))
        }
        "slack" => {
            let status = status().await?;
            let slack = &status["slack"];
            let team =
                slack["identity"]["team_id"].as_str().and_then(slack_id).ok_or("Slack has not been reached yet")?;
            let channel = slack["channel"].as_str().and_then(slack_id).ok_or("No Slack channel is configured")?;
            format!("https://app.slack.com/client/{team}/{channel}")
        }
        _ => return Err("unknown target".to_string()),
    };
    let mut open = tokio::process::Command::new("open");
    if target == "logs" {
        open.args(["-a", "Console"]);
    }
    let result = open.arg(&argument).output().await.map_err(|e| e.to_string())?;
    if result.status.success() { Ok(()) } else { Err(String::from_utf8_lossy(&result.stderr).trim().to_string()) }
}

// ── Background work ─────────────────────────────────────────────────────────

/// Follows the server's event stream and tells the web view about every change.
async fn follow_events(app: AppHandle, backend: Arc<Backend>) {
    backend.load_key().await;
    let mut backoff = Duration::from_secs(1);
    loop {
        if backend.server.key().is_none() {
            backend.set_live(&app, json!({ "live": false, "reason": "no key" }));
            backend.key_changed.notified().await;
            continue;
        }
        backend.set_live(&app, json!({ "live": true }));
        let emitter = app.clone();
        let changed = &backend.changed;
        let followed = backend
            .server
            .follow_events(|change| {
                let _ = emitter.emit("hcp-change", change);
                changed.notify_one();
            })
            .await;
        let reason = followed.err().unwrap_or_else(|| "the server closed the stream".to_string());
        backend.set_live(&app, json!({ "live": false, "reason": reason }));
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = backend.key_changed.notified() => {}
        }
        backoff = (backoff * 2).min(Duration::from_secs(30));
        if backend.server.health().await.is_ok() {
            backoff = Duration::from_secs(1);
        }
    }
}

/// Keeps the menu-bar title showing how many requests are waiting on a person.
async fn count_waiting(app: AppHandle, backend: Arc<Backend>) {
    backend.load_key().await;
    loop {
        let waiting = match backend.server.get("/v1/metrics?days=1&tz=UTC").await {
            Ok(metrics) => metrics["open"].as_array().map(Vec::len),
            Err(_) => None,
        };
        if let Some(tray) = app.tray_by_id(TRAY) {
            let (title, tooltip) = match waiting {
                Some(0) => (String::new(), "HCP: nothing waiting".to_string()),
                Some(n) => (n.to_string(), format!("HCP: {n} waiting on you")),
                None => ("–".to_string(), "HCP: server or key unavailable".to_string()),
            };
            let _ = tray.set_title(Some(title));
            let _ = tray.set_tooltip(Some(tooltip));
        }
        // Coalesce bursts of changes; refresh at least every 30 seconds.
        tokio::select! {
            _ = backend.changed.notified() => tokio::time::sleep(Duration::from_millis(400)).await,
            _ = backend.key_changed.notified() => {}
            _ = tokio::time::sleep(Duration::from_secs(30)) => {}
        }
    }
}

fn show_dashboard(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

fn tray(app: &tauri::App) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Open dashboard", true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, "quit", "Quit HCP Desktop", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &separator, &quit])?;
    TrayIconBuilder::with_id(TRAY)
        .icon(tauri::image::Image::from_bytes(include_bytes!("../icons/tray.png"))?)
        .icon_as_template(true)
        .tooltip("HCP")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "open" => show_dashboard(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .build(app)?;
    Ok(())
}

fn main() {
    let base_url = std::env::var("HCP_BASE_URL").ok().filter(|url| !url.trim().is_empty());
    let backend = Arc::new(Backend {
        server: Server::new(base_url.unwrap_or_else(|| DEFAULT_BASE_URL.to_string()), None),
        key_loaded: OnceCell::new(),
        live: Mutex::new(json!({ "live": false, "reason": "starting" })),
        key_changed: Notify::new(),
        changed: Notify::new(),
    });

    let app = tauri::Builder::default()
        .manage(backend.clone())
        .invoke_handler(tauri::generate_handler![
            setup_state,
            live,
            create_key,
            save_key,
            forget_key,
            status,
            metrics,
            request_detail,
            test_connector,
            service,
            open_target,
        ])
        .setup(move |app| {
            tray(app)?;
            tauri::async_runtime::spawn(follow_events(app.handle().clone(), backend.clone()));
            tauri::async_runtime::spawn(count_waiting(app.handle().clone(), backend.clone()));
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the window keeps the menu-bar icon (and its count) running.
            if let WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .build(tauri::generate_context!())
        .expect("failed to start HCP Desktop");

    app.run(|app, event| {
        // Clicking the Dock icon brings the window back.
        if let RunEvent::Reopen { .. } = event {
            show_dashboard(app);
        }
    });
}
