//! Logs to an Obsidian vault.
//!
//! Three kinds of Markdown files are kept under one folder of the vault (`HCP/` by default):
//!
//! - `Requests/YYYY-MM/<request_id>.md` — one note per coordination request, regenerated
//!   from the database whenever the request changes: properties, context, response and the
//!   full audit trail.
//! - `Log/HCP Log YYYY-MM-DD.md` — the append-only audit log, one line per event, linking
//!   to the request notes.
//! - `Service/HCP Service YYYY-MM-DD.md` — the server's own operational log.
//!
//! Audit events reach the vault through an outbox table written in the same transaction as
//! the event, so a crash or an unavailable vault delays the log but cannot lose entries.
//!
//! Everything an agent supplied is written as inert text (fenced blocks, code spans or
//! escaped inline text): a request must not be able to plant links, embeds, tags or
//! plugin code blocks in the vault.

use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local, NaiveDate, Utc};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing::field::{Field, Visit};

use crate::config::VaultConfig;
use crate::domain::{AuditEvent, CoordinationRequest, timestamp};
use crate::store::Store;

const BATCH: i64 = 200;
const RETRY_EVERY: Duration = Duration::from_secs(30);

pub struct Vault {
    root: PathBuf,
    /// Events appended to the daily log whose outbox rows are not yet confirmed deleted.
    appended: Mutex<HashSet<String>>,
}

impl Vault {
    /// Opens the vault and creates HCP's folders inside it. The vault directory itself must
    /// already exist: a missing path is far more likely a typo or an unmounted volume than
    /// a vault that should be created.
    pub fn open(config: &VaultConfig) -> Result<Self> {
        if !config.path.is_dir() {
            bail!("Obsidian vault {} does not exist or is not a directory", config.path.display());
        }
        let root = config.root();
        for folder in ["Requests", "Log", "Service"] {
            let path = root.join(folder);
            std::fs::create_dir_all(&path).with_context(|| format!("cannot create {}", path.display()))?;
        }
        Ok(Self { root, appended: Mutex::new(HashSet::new()) })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Whether the directory looks like a vault Obsidian has opened.
    pub fn looks_like_obsidian(config: &VaultConfig) -> bool {
        config.path.join(".obsidian").is_dir()
    }

    pub fn request_note_path(&self, cr: &CoordinationRequest) -> PathBuf {
        // UTC, so a note never moves if the machine's time zone changes.
        let month = cr.submitted_at.format("%Y-%m").to_string();
        self.root.join("Requests").join(month).join(format!("{}.md", file_stem(&cr.request_id)))
    }

    pub fn audit_log_path(&self, date: NaiveDate) -> PathBuf {
        self.root.join("Log").join(format!("HCP Log {date}.md"))
    }

    pub fn service_log_path(&self, date: NaiveDate) -> PathBuf {
        self.root.join("Service").join(format!("HCP Service {date}.md"))
    }

    /// Rewrites the request's note from its current state. Atomic: readers (Obsidian, sync
    /// tools) see either the old note or the new one.
    fn write_request_note(&self, cr: &CoordinationRequest, events: &[AuditEvent]) -> std::io::Result<()> {
        let path = self.request_note_path(cr);
        let dir = path.parent().unwrap_or(&self.root);
        std::fs::create_dir_all(dir)?;
        let temp = dir.join(format!(".{}.tmp", file_stem(&cr.request_id)));
        std::fs::write(&temp, render_request_note(cr, events))?;
        std::fs::rename(&temp, &path)
    }

    /// Appends one event to its day's audit log. `check_file` additionally looks for the
    /// event in the file, which is needed when replaying after a restart.
    fn append_audit_line(&self, cr: &CoordinationRequest, audit: &AuditEvent, check_file: bool) -> std::io::Result<()> {
        if self.appended.lock().unwrap().contains(&audit.event_id) {
            return Ok(());
        }
        let date = audit.created_at.with_timezone(&Local).date_naive();
        let path = self.audit_log_path(date);
        let existing = match std::fs::read_to_string(&path) {
            Ok(text) => Some(text),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => return Err(err),
        };
        let already_logged =
            check_file && existing.as_deref().is_some_and(|text| text.contains(&block_id(&audit.event_id)));
        if !already_logged {
            let mut out = String::new();
            if existing.is_none() {
                out.push_str(&daily_header("HCP audit log", "hcp/log", date));
            }
            out.push_str(&render_audit_line(cr, audit));
            append(&path, &out)?;
        }
        self.appended.lock().unwrap().insert(audit.event_id.clone());
        Ok(())
    }

    fn confirm(&self, event_ids: &[String]) {
        let mut appended = self.appended.lock().unwrap();
        for id in event_ids {
            appended.remove(id);
        }
    }
}

fn append(path: &Path, text: &str) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(text.as_bytes())
}

/// Writes every queued audit event to the vault. Returns how many were written.
/// `replay` must be true for the first drain after start-up, when queued events may already
/// have been written by a previous run that stopped before clearing them.
pub async fn drain(vault: &Arc<Vault>, store: &Store, replay: bool) -> Result<usize> {
    let mut written = 0;
    loop {
        let batch = store.outbox_batch(BATCH).await?;
        if batch.is_empty() {
            return Ok(written);
        }

        // Group by request so each note is rendered once per batch.
        let mut by_request: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for entry in &batch {
            by_request.entry(entry.request_id.clone()).or_default().push(entry.event_id.clone());
        }
        let mut done = Vec::with_capacity(batch.len());
        for (request_id, event_ids) in by_request {
            let Some(cr) = store.get_request(&request_id).await? else {
                done.extend(event_ids);
                continue;
            };
            let events = store.request_events(&request_id).await?;
            let vault = vault.clone();
            let ids = event_ids.clone();
            tokio::task::spawn_blocking(move || -> std::io::Result<()> {
                for audit in events.iter().filter(|e| ids.contains(&e.event_id)) {
                    vault.append_audit_line(&cr, audit, replay)?;
                }
                vault.write_request_note(&cr, &events)
            })
            .await
            .context("vault writer task failed")?
            .with_context(|| format!("cannot write request {request_id} to the vault"))?;
            done.extend(event_ids);
        }

        store.outbox_done(&done).await?;
        vault.confirm(&done);
        written += done.len();
    }
}

/// Drains the outbox whenever woken, and periodically in case a wake-up was missed or an
/// earlier attempt failed. Drains once more on shutdown.
pub async fn run(vault: Arc<Vault>, store: Store, wake: Arc<Notify>, shutdown: CancellationToken) {
    let mut replay = true;
    loop {
        match drain(&vault, &store, replay).await {
            Ok(_) => replay = false,
            Err(err) => {
                tracing::warn!(error = %format!("{err:#}"), "could not write to the Obsidian vault; will retry")
            }
        }
        if shutdown.is_cancelled() {
            return;
        }
        tokio::select! {
            _ = shutdown.cancelled() => {}
            _ = wake.notified() => {}
            _ = tokio::time::sleep(RETRY_EVERY) => {}
        }
    }
}

// ── Rendering ───────────────────────────────────────────────────────────────

/// Request IDs are ULIDs, but never let one become a path.
fn file_stem(id: &str) -> String {
    id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect()
}

/// An Obsidian block reference for an event (`^<id>`): marks the line and lets notes link to it.
fn block_id(event_id: &str) -> String {
    format!("^{}", file_stem(event_id).replace('_', "-"))
}

fn local(at: &DateTime<Utc>) -> String {
    at.with_timezone(&Local).format("%Y-%m-%d %H:%M:%S %:z").to_string()
}

fn longest_run(text: &str, of: char) -> usize {
    let (mut longest, mut current) = (0, 0);
    for c in text.chars() {
        current = if c == of { current + 1 } else { 0 };
        longest = longest.max(current);
    }
    longest
}

/// Untrusted text as a single line of plain prose: whitespace collapsed, Markdown and
/// Obsidian syntax escaped, length capped.
pub fn inline(text: &str, max_chars: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out = String::new();
    for (index, c) in collapsed.chars().enumerate() {
        if index == max_chars {
            out.push('…');
            break;
        }
        if matches!(
            c,
            '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '#' | '|' | '!' | '$' | '%' | '~' | '=' | '^' | '{' | '}'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Untrusted text as an inline code span that cannot be closed from the inside.
pub fn code_span(text: &str) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        return "` `".to_string();
    }
    let fence = "`".repeat(longest_run(&text, '`') + 1);
    if text.starts_with('`') || text.ends_with('`') {
        format!("{fence} {text} {fence}")
    } else {
        format!("{fence}{text}{fence}")
    }
}

/// Untrusted text as a fenced block. The language is chosen here, never by the content, so
/// it cannot become a block that a plugin executes.
pub fn fenced(language: &str, text: &str) -> String {
    let fence = "`".repeat((longest_run(text, '`') + 1).max(3));
    format!("{fence}{language}\n{}\n{fence}\n", text.trim_end_matches('\n'))
}

/// A YAML double-quoted scalar.
pub fn yaml_string(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn daily_header(title: &str, tag: &str, date: NaiveDate) -> String {
    format!("---\ntags:\n  - hcp\n  - {tag}\ndate: {date}\n---\n\n# {title} — {date}\n\n")
}

fn title(cr: &CoordinationRequest) -> String {
    format!("{}: {}", cr.intent, cr.context_package.summary.split_whitespace().collect::<Vec<_>>().join(" "))
}

pub fn render_audit_line(cr: &CoordinationRequest, audit: &AuditEvent) -> String {
    let link_text: String =
        title(cr).chars().filter(|c| !matches!(c, '[' | ']' | '|' | '#' | '^' | '`' | '\\')).take(80).collect();
    format!(
        "- `{}` {} · [[{}|{}]] · {} ({}) · {} {}\n",
        audit.created_at.with_timezone(&Local).format("%H:%M:%S%.3f"),
        code_span(&audit.event_type),
        file_stem(&cr.request_id),
        link_text.trim(),
        code_span(&audit.actor),
        audit.actor_type,
        code_span(&audit.payload.to_string()),
        block_id(&audit.event_id),
    )
}

pub fn render_request_note(cr: &CoordinationRequest, events: &[AuditEvent]) -> String {
    let mut out = String::new();
    let intent_tag = cr.intent.as_str().to_lowercase();
    let state_tag = cr.state.as_str().to_lowercase();

    // Properties.
    out.push_str("---\n");
    let _ = writeln!(out, "hcp_request_id: {}", yaml_string(&cr.request_id));
    let _ = writeln!(out, "intent: {}", cr.intent);
    let _ = writeln!(out, "urgency: {}", cr.urgency);
    let _ = writeln!(out, "state: {}", cr.state);
    let _ = writeln!(out, "agent: {}", yaml_string(&cr.agent_id));
    let _ = writeln!(out, "responder: {}", yaml_string(&cr.responder_id));
    let _ = writeln!(out, "submitted: {}", yaml_string(&timestamp::format(&cr.submitted_at)));
    let _ = writeln!(out, "deadline: {}", yaml_string(&timestamp::format(&cr.timeout_at)));
    let _ = writeln!(out, "fallback: {}", cr.timeout_policy.fallback);
    if let Some(decision) = cr.decision() {
        let _ = writeln!(out, "decision: {}", yaml_string(decision));
    }
    if let Some(by) = &cr.responded_by {
        let _ = writeln!(out, "responded_by: {}", yaml_string(by));
    }
    if let Some(at) = &cr.responded_at {
        let _ = writeln!(out, "responded: {}", yaml_string(&timestamp::format(at)));
    }
    if let Some(trace) = &cr.trace_id {
        let _ = writeln!(out, "trace_id: {}", yaml_string(trace));
    }
    let _ = writeln!(out, "tags:\n  - hcp\n  - hcp/{intent_tag}\n  - hcp/state/{state_tag}");
    let _ = writeln!(out, "aliases:\n  - {}", yaml_string(&title(cr).chars().take(120).collect::<String>()));
    out.push_str("---\n\n");

    let _ = writeln!(out, "# {}\n", inline(&title(cr), 120));
    let _ = writeln!(out, "> [!abstract] {} · {} urgency", cr.state, cr.urgency);
    let _ = writeln!(out, "> Agent {} → responder {}", code_span(&cr.agent_id), code_span(&cr.responder_id));
    let _ = writeln!(
        out,
        "> Submitted {} · deadline {} · on timeout {}\n",
        local(&cr.submitted_at),
        local(&cr.timeout_at),
        cr.timeout_policy.fallback
    );
    out.push_str("*Generated by HCP. This note is rewritten whenever the request changes; edits are not kept.*\n\n");

    let _ = writeln!(out, "## Summary\n\n{}", fenced("text", &cr.context_package.summary));
    if let Some(detail) = cr.context_package.detail.as_deref().filter(|d| !d.trim().is_empty()) {
        let _ = writeln!(out, "## Detail\n\n{}", fenced("text", detail));
    }
    if let Some(metadata) = &cr.context_package.metadata {
        let pretty = serde_json::to_string_pretty(metadata).unwrap_or_default();
        let _ = writeln!(out, "## Metadata\n\n{}", fenced("json", &pretty));
    }
    if let Some(attachments) = cr.context_package.attachments.as_deref().filter(|a| !a.is_empty()) {
        out.push_str("## Attachments\n\n");
        for attachment in attachments {
            let _ = writeln!(out, "### {} ({})\n", inline(&attachment.name, 120), inline(&attachment.kind, 40));
            let _ = writeln!(out, "{}", fenced("text", &attachment.content));
        }
    }

    if let Some(response) = &cr.response_data {
        out.push_str("## Response\n\n");
        if let Some(by) = &cr.responded_by {
            let when = cr.responded_at.as_ref().map(local).unwrap_or_default();
            let _ = writeln!(out, "By {} at {when}\n", code_span(by));
        }
        let pretty = serde_json::to_string_pretty(response).unwrap_or_default();
        let _ = writeln!(out, "{}", fenced("json", &pretty));
    }

    out.push_str("## Audit trail\n\n");
    for audit in events {
        let _ = writeln!(
            out,
            "- `{}` {} · {} ({}) · {} {}",
            audit.created_at.with_timezone(&Local).format("%Y-%m-%d %H:%M:%S%.3f %:z"),
            code_span(&audit.event_type),
            code_span(&audit.actor),
            audit.actor_type,
            code_span(&audit.payload.to_string()),
            block_id(&audit.event_id),
        );
    }
    out
}

// ── Service log ─────────────────────────────────────────────────────────────

enum ServiceLog {
    Line { at: DateTime<Local>, text: String },
    Flush(mpsc::SyncSender<()>),
}

/// A `tracing` layer that appends the server's log lines to the vault's daily service note.
/// It keeps HCP's own events at INFO and above, so dependency chatter and debug output stay
/// out of the notes whatever the console is set to show. Writing happens on a dedicated
/// thread so logging never blocks request handling.
pub struct ServiceLogLayer {
    lines: mpsc::Sender<ServiceLog>,
}

/// Lets the process wait for queued service-log lines to reach disk before it exits.
#[derive(Clone)]
pub struct ServiceLogHandle {
    lines: mpsc::Sender<ServiceLog>,
}

impl ServiceLogHandle {
    pub fn flush(&self) {
        let (done, wait) = mpsc::sync_channel(1);
        if self.lines.send(ServiceLog::Flush(done)).is_ok() {
            let _ = wait.recv_timeout(Duration::from_secs(5));
        }
    }
}

pub fn service_log(vault: Arc<Vault>) -> (ServiceLogLayer, ServiceLogHandle) {
    let (lines, queue) = mpsc::channel();
    std::thread::Builder::new()
        .name("hcp-service-log".to_string())
        .spawn(move || {
            for message in queue {
                match message {
                    ServiceLog::Flush(done) => {
                        let _ = done.send(());
                    }
                    ServiceLog::Line { at, text } => {
                        let path = vault.service_log_path(at.date_naive());
                        let mut out = String::new();
                        if !path.exists() {
                            out.push_str(&daily_header("HCP service log", "hcp/service", at.date_naive()));
                        }
                        out.push_str(&text);
                        // Not `tracing`: a failure here must not feed back into this log.
                        if let Err(err) = append(&path, &out) {
                            eprintln!("hcp: cannot write service log to {}: {err}", path.display());
                        }
                    }
                }
            }
        })
        .expect("failed to start the service log thread");
    (ServiceLogLayer { lines: lines.clone() }, ServiceLogHandle { lines })
}

#[derive(Default)]
struct LineVisitor {
    message: String,
    fields: Vec<(&'static str, String)>,
}

impl LineVisitor {
    fn put(&mut self, field: &Field, value: String) {
        if field.name() == "message" {
            self.message = value;
        } else {
            self.fields.push((field.name(), value));
        }
    }
}

impl Visit for LineVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.put(field, value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.put(field, format!("{value:?}"));
    }
}

pub fn render_service_line(
    at: &DateTime<Local>,
    level: &tracing::Level,
    target: &str,
    message: &str,
    fields: &[(&str, String)],
) -> String {
    let mut line = format!("- `{}` **{level}** `{target}` {}", at.format("%H:%M:%S%.3f"), inline(message, 500));
    for (name, value) in fields {
        let _ = write!(line, " · {}", code_span(&format!("{name}={value}")));
    }
    line.push('\n');
    line
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for ServiceLogLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: tracing_subscriber::layer::Context<'_, S>) {
        let meta = event.metadata();
        let ours = meta.target() == "hcp" || meta.target().starts_with("hcp::");
        if !ours || *meta.level() > tracing::Level::INFO {
            return;
        }
        let mut visitor = LineVisitor::default();
        event.record(&mut visitor);
        let at = Local::now();
        let text = render_service_line(&at, meta.level(), meta.target(), &visitor.message, &visitor.fields);
        let _ = self.lines.send(ServiceLog::Line { at, text });
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use serde_json::json;
    use tracing_subscriber::layer::SubscriberExt;

    use super::*;
    use crate::domain::{ActorType, Attachment, Intent, State};
    use crate::slack::tests::request;

    fn audit(id: &str, kind: &str, payload: serde_json::Value) -> AuditEvent {
        AuditEvent {
            event_id: id.to_string(),
            request_id: "01TESTREQUEST".to_string(),
            event_type: kind.to_string(),
            actor: "claude-code".to_string(),
            actor_type: ActorType::Agent,
            payload,
            created_at: Utc.with_ymd_and_hms(2026, 1, 15, 12, 0, 0).unwrap(),
        }
    }

    fn open_vault(dir: &Path) -> Arc<Vault> {
        Arc::new(Vault::open(&VaultConfig { path: dir.to_path_buf(), folder: "HCP".to_string() }).unwrap())
    }

    #[test]
    fn inline_text_cannot_form_obsidian_syntax() {
        assert_eq!(inline("see [[Secret]] and ![[embed]]", 100), "see \\[\\[Secret\\]\\] and \\!\\[\\[embed\\]\\]");
        assert_eq!(inline("#tag <b>x</b> %%hide%%", 100), "\\#tag \\<b\\>x\\</b\\> \\%\\%hide\\%\\%");
        assert_eq!(inline("line one\n\n# heading", 100), "line one \\# heading");
        assert_eq!(inline("abcdef", 3), "abc…");
    }

    #[test]
    fn code_spans_and_fences_cannot_be_closed_from_inside() {
        assert_eq!(code_span("plain"), "`plain`");
        assert_eq!(code_span("a `b` ``c``"), "``` a `b` ``c`` ```");
        assert_eq!(code_span("a `b` c"), "``a `b` c``");
        assert_eq!(code_span("two\nlines"), "`two lines`");

        let hostile = "ok\n```\n```dataviewjs\nalert(1)\n```";
        let block = fenced("text", hostile);
        assert!(block.starts_with("````text\n") && block.ends_with("\n````\n"));
        assert_eq!(fenced("json", "{}"), "```json\n{}\n```\n");
    }

    #[test]
    fn yaml_strings_are_quoted_and_escaped() {
        assert_eq!(yaml_string("plain"), "\"plain\"");
        assert_eq!(yaml_string("a\"b\\c\nd: e"), "\"a\\\"b\\\\c\\nd: e\"");
    }

    #[test]
    fn request_note_has_properties_context_response_and_audit_trail() {
        let mut cr = request(Intent::Approval);
        cr.state = State::Delivered;
        cr.context_package.detail = Some("Includes the new auth flow.".to_string());
        cr.context_package.metadata = Some(json!({ "version": "2.1.0" }).as_object().unwrap().clone());
        cr.context_package.attachments =
            Some(vec![Attachment { kind: "diff".into(), name: "auth.patch".into(), content: "+ line".into() }]);
        cr.response_data = Some(json!({ "decision": "approved" }));
        cr.responded_by = Some("slack:isaac".to_string());
        cr.responded_at = Some(cr.submitted_at);
        let events = [
            audit("01EVENTA", "CR_SUBMITTED", json!({ "intent": "APPROVAL" })),
            audit("01EVENTB", "CR_DELIVERED", json!({})),
        ];

        let note = render_request_note(&cr, &events);
        assert!(note.starts_with("---\nhcp_request_id: \"01TESTREQUEST\"\nintent: APPROVAL\n"));
        assert!(note.contains("\nstate: DELIVERED\n"));
        assert!(note.contains("\ndecision: \"approved\"\n"));
        assert!(note.contains("  - hcp/approval\n  - hcp/state/delivered\n"));
        assert!(note.contains("aliases:\n  - \"APPROVAL: Deploy v2.1.0 to production\"\n"));
        assert!(note.contains("# APPROVAL: Deploy v2.1.0 to production\n"));
        assert!(note.contains("## Detail\n\n```text\nIncludes the new auth flow.\n```\n"));
        assert!(note.contains("\"version\": \"2.1.0\""));
        assert!(note.contains("### auth.patch (diff)"));
        assert!(note.contains("## Response\n\nBy `slack:isaac` at "));
        assert!(note.contains("`CR_SUBMITTED` · `claude-code` (AGENT) · `{\"intent\":\"APPROVAL\"}` ^01EVENTA\n"));
        assert!(note.contains("`CR_DELIVERED`"));
    }

    #[test]
    fn hostile_request_text_stays_inert_in_the_note() {
        let mut cr = request(Intent::Approval);
        cr.context_package.summary = "ok\n---\nstate: APPROVED\n[[Passwords]] #pwned".to_string();
        cr.context_package.detail = Some("```dataviewjs\napp.vault.delete()\n```".to_string());
        cr.agent_id = "evil\"\nstate: DELIVERED".to_string();

        let note = render_request_note(&cr, &[]);
        let properties = note.strip_prefix("---\n").unwrap().split("\n---\n").next().unwrap();
        // Exactly one `state:` property, and it is the real one.
        assert_eq!(properties.lines().filter(|l| l.starts_with("state:")).count(), 1);
        assert!(properties.contains("state: PENDING_RESPONSE"));
        assert!(properties.contains("agent: \"evil\\\"\\nstate: DELIVERED\""));
        // The heading is escaped, and the code block is wrapped in a longer fence as text.
        assert!(note.contains("\\[\\[Passwords\\]\\] \\#pwned"));
        assert!(note.contains("````text\n```dataviewjs\napp.vault.delete()\n```\n````"));
    }

    #[test]
    fn audit_log_lines_link_to_the_request_note() {
        let cr = request(Intent::Approval);
        let line = render_audit_line(&cr, &audit("01EVENTA", "CR_SUBMITTED", json!({ "urgency": "HIGH" })));
        assert!(line.contains("`CR_SUBMITTED` · [[01TESTREQUEST|APPROVAL: Deploy v2.1.0 to production]]"));
        assert!(line.ends_with(" ^01EVENTA\n"));
        assert_eq!(line.lines().count(), 1);
    }

    #[test]
    fn audit_log_link_text_cannot_break_out_of_the_link() {
        let mut cr = request(Intent::Approval);
        cr.context_package.summary = "x]] [[Other|y".to_string();
        let line = render_audit_line(&cr, &audit("01EVENTA", "CR_SUBMITTED", json!({})));
        assert!(line.contains("[[01TESTREQUEST|APPROVAL: x Othery]]"));
    }

    #[test]
    fn open_requires_an_existing_vault_and_creates_the_hcp_folders() {
        let dir = tempfile::tempdir().unwrap();
        let missing = VaultConfig { path: dir.path().join("nope"), folder: "HCP".to_string() };
        assert!(Vault::open(&missing).is_err());

        let vault = open_vault(dir.path());
        for folder in ["Requests", "Log", "Service"] {
            assert!(vault.root().join(folder).is_dir());
        }
    }

    #[test]
    fn audit_lines_are_appended_once_even_when_replayed() {
        let dir = tempfile::tempdir().unwrap();
        let cr = request(Intent::Approval);
        let first = audit("01EVENTA", "CR_SUBMITTED", json!({}));
        let second = audit("01EVENTB", "CR_ROUTING", json!({}));

        let vault = open_vault(dir.path());
        vault.append_audit_line(&cr, &first, true).unwrap();
        vault.append_audit_line(&cr, &first, false).unwrap();
        vault.append_audit_line(&cr, &second, false).unwrap();

        // A new process (empty in-memory set) replaying the same event must not duplicate it.
        let restarted = open_vault(dir.path());
        restarted.append_audit_line(&cr, &first, true).unwrap();

        let date = first.created_at.with_timezone(&Local).date_naive();
        let log = std::fs::read_to_string(vault.audit_log_path(date)).unwrap();
        assert!(log.starts_with("---\ntags:\n  - hcp\n  - hcp/log\n"));
        assert_eq!(log.matches("^01EVENTA").count(), 1);
        assert_eq!(log.matches("^01EVENTB").count(), 1);
    }

    #[test]
    fn request_notes_are_replaced_atomically_and_leave_no_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let vault = open_vault(dir.path());
        let mut cr = request(Intent::Approval);
        vault.write_request_note(&cr, &[]).unwrap();
        cr.state = State::Cancelled;
        vault.write_request_note(&cr, &[]).unwrap();

        let path = vault.request_note_path(&cr);
        assert!(std::fs::read_to_string(&path).unwrap().contains("state: CANCELLED"));
        let names: Vec<_> =
            std::fs::read_dir(path.parent().unwrap()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, ["01TESTREQUEST.md"]);
    }

    #[test]
    fn request_ids_cannot_escape_the_requests_folder() {
        let dir = tempfile::tempdir().unwrap();
        let vault = open_vault(dir.path());
        let mut cr = request(Intent::Approval);
        cr.request_id = "../../../etc/passwd".to_string();
        assert!(vault.request_note_path(&cr).starts_with(vault.root().join("Requests")));
        assert!(!vault.request_note_path(&cr).to_string_lossy().contains(".."));
    }

    #[test]
    fn service_log_layer_writes_events_to_the_daily_note() {
        let dir = tempfile::tempdir().unwrap();
        let vault = open_vault(dir.path());
        let (layer, handle) = service_log(vault.clone());

        tracing::subscriber::with_default(tracing_subscriber::registry().with(layer), || {
            tracing::info!(port = 3100, "HCP listening");
            tracing::warn!(error = "channel_not_found", "Slack delivery failed");
            // Not kept: debug output, and anything a dependency logs.
            tracing::debug!("state changed");
            tracing::warn!(target: "sqlx::query", "slow statement");
            tracing::error!(target: "hcpx", "not ours either");
        });
        handle.flush();

        let log = std::fs::read_to_string(vault.service_log_path(Local::now().date_naive())).unwrap();
        assert!(log.starts_with("---\ntags:\n  - hcp\n  - hcp/service\n"));
        assert!(log.contains("**INFO** `hcp::vault::tests` HCP listening · `port=3100`"));
        assert!(log.contains("**WARN** `hcp::vault::tests` Slack delivery failed · `error=channel_not_found`"));
        assert_eq!(log.matches("# HCP service log").count(), 1);
        assert_eq!(log.lines().filter(|line| line.starts_with("- ")).count(), 2, "{log}");
    }
}
