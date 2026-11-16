// HCP Desktop. Everything comes from the app's own commands (see src/main.rs); the
// admin key stays on the Rust side.

import { compact, el, hideTooltip, legend, rankedBars, stackedColumns, table } from "./charts.js";

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);
const css = (name) => `var(${name})`;
const tz = Intl.DateTimeFormat().resolvedOptions().timeZone || "UTC";

/** The stacked series, bottom to top, in fixed palette order. "Other" folds the rest. */
const SERIES = [
  { key: "approved", name: "Approved", color: css("--series-1"), outcomes: ["approved"] },
  { key: "rejected", name: "Rejected", color: css("--series-2"), outcomes: ["rejected"] },
  { key: "answered", name: "Answered", color: css("--series-3"), outcomes: ["answered"] },
  {
    key: "missed",
    name: "No answer in time",
    color: css("--series-4"),
    outcomes: ["auto_approved", "auto_rejected", "timed_out"],
  },
  { key: "other", name: "Open or cancelled", color: css("--series-other"), outcomes: ["open", "cancelled", "other"] },
];

const state = {
  days: Number(localStorage.getItem("hcp.days")) || 14,
  metrics: null,
  status: null,
  service: null,
  tables: new Set(),
  tests: {},
};

// ── Formatting ─────────────────────────────────────────────────────────────

function duration(seconds) {
  if (seconds === null || seconds === undefined) return "–";
  const s = Math.max(0, Math.round(seconds));
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m ${String(s % 60).padStart(2, "0")}s`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ${String(Math.floor((s % 3600) / 60)).padStart(2, "0")}m`;
  return `${Math.floor(s / 86400)}d ${Math.floor((s % 86400) / 3600)}h`;
}

function ago(iso) {
  if (!iso) return "never";
  const seconds = (Date.now() - Date.parse(iso)) / 1000;
  if (seconds < 5) return "just now";
  return `${duration(seconds).split(" ")[0]} ago`;
}

function until(iso) {
  const seconds = (Date.parse(iso) - Date.now()) / 1000;
  return seconds <= 0 ? "overdue" : `in ${duration(seconds)}`;
}

function clock(iso) {
  const date = new Date(iso);
  const today = new Date().toDateString() === date.toDateString();
  return today
    ? date.toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit" })
    : date.toLocaleDateString(undefined, { day: "numeric", month: "short" });
}

function bytes(n) {
  if (n === null || n === undefined) return "–";
  const units = ["B", "KB", "MB", "GB"];
  let i = 0;
  while (n >= 1024 && i < units.length - 1) { n /= 1024; i += 1; }
  return `${n.toFixed(i ? 1 : 0)} ${units[i]}`;
}

const title = (text) => (text ? text.charAt(0) + text.slice(1).toLowerCase().replaceAll("_", " ") : "");

function sum(counts, outcomes) {
  return outcomes.reduce((total, outcome) => total + (counts[outcome] || 0), 0);
}

function folded(counts) {
  return Object.fromEntries(SERIES.map((s) => [s.key, sum(counts, s.outcomes)]));
}

// ── Loading ────────────────────────────────────────────────────────────────

async function refresh({ withStatus = true } = {}) {
  $("main").classList.add("loading");
  try {
    const jobs = [invoke("metrics", { days: state.days, tz })];
    if (withStatus) jobs.push(invoke("status"), invoke("service"));
    const [metrics, status, service] = await Promise.all(jobs);
    state.metrics = metrics;
    if (withStatus) Object.assign(state, { status, service });
    $("problem").hidden = true;
    render();
    $("updated").textContent = `Updated ${new Date().toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit", second: "2-digit" })}`;
  } catch (error) {
    problem(String(error));
    if (String(error).includes("Set it up again")) await boot();
  } finally {
    $("main").classList.remove("loading");
  }
}

function problem(message) {
  const banner = $("problem");
  banner.replaceChildren(el("strong", { text: "Could not refresh." }), el("span", { class: "secondary", text: message }));
  banner.hidden = false;
}

let pending = null;
function refreshSoon() {
  clearTimeout(pending);
  pending = setTimeout(() => refresh({ withStatus: false }), 500);
}

// ── Rendering ──────────────────────────────────────────────────────────────

function render() {
  if (!state.metrics) return;
  renderKpis();
  renderDaily();
  renderAgents();
  renderOpen();
  renderRecent();
  renderResponders();
  if (state.status) renderConnectors();
}

function renderKpis() {
  const { totals, open, response_times: times } = state.metrics;
  const human = Object.values(totals).reduce((a, b) => a + b, 0) - (totals.notification || 0);
  const decided = (totals.approved || 0) + (totals.rejected || 0);
  const missed = sum(totals, ["auto_approved", "auto_rejected", "timed_out"]);

  $("kpi-waiting").textContent = compact(open.length);
  $("kpi-waiting-note").textContent = open.length ? `next deadline ${until(open[0].timeout_at)}` : "Nothing waiting";
  $("kpi-requests").textContent = compact(human);
  $("kpi-requests-note").textContent = `${compact(totals.open || 0)} still open`;
  $("kpi-approval").textContent = decided ? `${Math.round((100 * (totals.approved || 0)) / decided)}%` : "–";
  $("kpi-approval-note").textContent = decided ? `${compact(totals.approved || 0)} of ${compact(decided)} decided by a person` : "No decisions yet";
  $("kpi-median").textContent = duration(times.median_seconds);
  $("kpi-median-note").textContent = times.count ? `p90 ${duration(times.p90_seconds)} · ${compact(times.count)} answers` : "No answers yet";
  $("kpi-missed").textContent = compact(missed);
  $("kpi-missed-note").textContent = human ? `${Math.round((100 * missed) / human)}% of requests` : "–";
  $("kpi-notes").textContent = compact(totals.notification || 0);
  const label = state.days === 1 ? "today" : `last ${state.days} days`;
  $("range-note").textContent = `Showing ${label} · ${tz}`;
}

function dayLabels(date) {
  const d = new Date(`${date}T12:00:00`);
  return {
    label: d.toLocaleDateString(undefined, { day: "numeric", month: "short" }),
    long: d.toLocaleDateString(undefined, { weekday: "short", day: "numeric", month: "short" }),
  };
}

function renderDaily() {
  const days = state.metrics.daily.map((day) => ({ ...dayLabels(day.date), values: folded(day.counts) }));
  $("daily-legend").replaceChildren(legend(SERIES, folded(state.metrics.totals)));
  stackedColumns($("daily"), { series: SERIES, days });
  $("daily-table").replaceChildren(
    table(
      [{ label: "Day" }, ...SERIES.map((s) => ({ label: s.name, numeric: true }))],
      days.map((d) => [d.long, ...SERIES.map((s) => compact(d.values[s.key]))]),
    ),
  );
  toggleTable("daily", false);
}

function renderAgents() {
  const rows = state.metrics.by_agent
    .map((group) => ({ name: group.key, value: group.total - (group.counts.notification || 0), open: group.counts.open || 0 }))
    .filter((row) => row.value > 0)
    .sort((a, b) => b.value - a.value || a.name.localeCompare(b.name));
  rankedBars($("agents"), { rows, color: css("--series-1"), unit: "requests for a person" });
  $("agents-table").replaceChildren(
    table(
      [{ label: "Agent" }, { label: "Requests", numeric: true }, { label: "Open", numeric: true }],
      rows.map((r) => [r.name, compact(r.value), compact(r.open)]),
    ),
  );
  toggleTable("agents", false);
}

function toggleTable(name, flip = true) {
  if (flip) state.tables.has(name) ? state.tables.delete(name) : state.tables.add(name);
  const on = state.tables.has(name);
  $(name).hidden = on;
  $(`${name}-table`).hidden = !on;
  if (name === "daily") $("daily-legend").hidden = on;
  const button = document.querySelector(`[data-table="${name}"]`);
  button.textContent = on ? "Chart" : "Table";
  button.setAttribute("aria-pressed", String(on));
}

function renderOpen() {
  const open = state.metrics.open;
  if (!open.length) {
    $("open").replaceChildren(el("div", { class: "empty", text: "Nothing is waiting on a person." }));
    return;
  }
  const rows = open.map((r) => {
    const soon = Date.parse(r.timeout_at) - Date.now() < 5 * 60 * 1000;
    return el(
      "tr",
      { class: "clickable", tabindex: "0", onclick: () => openDrawer(r.request_id), onkeydown: (e) => e.key === "Enter" && openDrawer(r.request_id) },
      el("td", {}, el("span", { class: "chip", text: title(r.intent) })),
      el("td", { class: "summary-cell", title: r.summary, text: r.summary }),
      el("td", { class: "secondary tight", text: r.agent_id }),
      el("td", { class: "secondary tight", text: r.responder_id }),
      el("td", { class: `n tight countdown ${soon ? "due-soon" : ""}`, "data-due": r.timeout_at, text: until(r.timeout_at) }),
    );
  });
  $("open").replaceChildren(
    el(
      "table",
      {},
      el("thead", {}, el("tr", {}, ["Intent", "Summary", "Agent", "For", "Deadline"].map((h, i) => el("th", { class: i === 4 ? "n" : "", text: h })))),
      el("tbody", {}, rows),
    ),
  );
}

const EVENT_TEXT = {
  CR_SUBMITTED: "Submitted",
  CR_ROUTING: "Routing",
  CR_PENDING_RESPONSE: "Waiting for a person",
  CR_RESPONDED: "Answered",
  CR_DELIVERED: "Delivered to the agent",
  CR_ESCALATED: "Escalated",
  CR_TIMED_OUT: "Timed out",
  CR_CANCELLED: "Cancelled",
  SLACK_NOTIFIED: "Posted to Slack",
  SLACK_NOTIFY_FAILED: "Slack post failed",
  SLACK_INTERACTION: "Clicked in Slack",
};

function renderRecent() {
  const recent = state.metrics.recent.slice(0, 10);
  if (!recent.length) {
    $("recent").replaceChildren(el("li", { class: "empty", text: "No activity yet." }));
    return;
  }
  $("recent").replaceChildren(
    ...recent.map((e) =>
      el(
        "li",
        { tabindex: "0", onclick: () => openDrawer(e.request_id), onkeydown: (ev) => ev.key === "Enter" && openDrawer(e.request_id) },
        el("time", { datetime: e.created_at, text: clock(e.created_at) }),
        el(
          "div",
          {},
          el("div", { class: "what" }, `${EVENT_TEXT[e.event_type] || e.event_type} `, el("span", { class: "muted", text: `· ${e.actor}` })),
          el("div", { class: "ctx", text: `${title(e.intent)} from ${e.agent_id}: ${e.summary}` }),
        ),
      ),
    ),
  );
}

function renderResponders() {
  const responders = state.metrics.responders;
  $("responders").replaceChildren(
    responders.length
      ? table(
          [{ label: "Responder" }, { label: "Answers", numeric: true }, { label: "Median time", numeric: true }],
          responders.map((r) => [r.responded_by, compact(r.total), duration(r.median_seconds)]),
        )
      : el("div", { class: "empty", text: "Nobody has answered anything in this period." }),
  );
  const intents = state.metrics.by_intent;
  $("intents").replaceChildren(
    intents.length
      ? table(
          [{ label: "Intent" }, { label: "Requests", numeric: true }, { label: "Open", numeric: true }],
          intents.map((g) => [title(g.key), compact(g.total), compact(g.counts.open || 0)]),
        )
      : el("div", { class: "empty", text: "No requests in this period." }),
  );
}

// ── Connectors ─────────────────────────────────────────────────────────────

const STATUS = {
  good: { icon: "✓", label: "Healthy" },
  warning: { icon: "!", label: "Check" },
  critical: { icon: "✕", label: "Down" },
  off: { icon: "–", label: "Not configured" },
};

function statusPill(kind, label) {
  const s = STATUS[kind];
  return el("span", { class: `status ${kind}` }, el("span", { class: "icon", "aria-hidden": "true", text: s.icon }), label || s.label);
}

const later = (a, b) => (a && (!b || Date.parse(a) > Date.parse(b)));

/** pairs: [label, value, fullValue?]; a shortened value keeps the full one as its tooltip. */
function facts(pairs) {
  return el(
    "dl",
    {},
    pairs.filter(Boolean).flatMap(([k, v, full]) => [el("dt", { text: k }), el("dd", { text: v ?? "–", title: full })]),
  );
}

/** ~/… for the home folder, and the middle elided when a path is still long. */
function shortPath(path) {
  if (!path) return path;
  let short = path.replace(/^\/Users\/[^/]+/, "~");
  const parts = short.split("/");
  if (short.length > 38 && parts.length > 4) short = [parts[0], parts[1], "…", ...parts.slice(-2)].join("/");
  return short;
}

/** ep-name-123.c-2.eu-west-2.aws.neon.tech -> "ep-name-123 (eu-west-2)". */
function shortHost(host) {
  const match = /^([^.]+)\..*?([a-z]{2}-[a-z]+-\d)\.aws\.neon\.tech$/.exec(host || "");
  return match ? `${match[1]} (${match[2]})` : host;
}

function connector(name, { kind, label, details, error, test, open }) {
  const result = state.tests[test];
  return el(
    "article",
    { class: "connector", "aria-label": name },
    el("header", {}, el("h3", { text: name }), statusPill(kind, label)),
    details,
    error ? el("div", { class: "error", text: error }) : null,
    el(
      "footer",
      {},
      test ? el("button", { class: "ghost", type: "button", onclick: () => runTest(test), disabled: result?.running, text: result?.running ? "Testing…" : "Test" }) : null,
      ...(open || []).map(([text, target]) => el("button", { class: "ghost", type: "button", onclick: () => openTarget(target), text })),
      result && !result.running
        ? el("span", { class: result.ok ? "result" : "error", text: `${result.ok ? "✓" : "✕"} ${result.message} (${result.latency_ms} ms)` })
        : null,
    ),
  );
}

function renderConnectors() {
  const { status, service } = state;
  const db = status.database, slack = status.slack, vault = status.vault;

  const serverKind = service?.loaded === false ? "warning" : "good";
  const server = connector("HCP server", {
    kind: serverKind,
    label: serverKind === "good" ? "Running" : "Not a service",
    details: facts([
      ["Version", status.version],
      ["Up for", duration(status.uptime_seconds)],
      ["Service", service?.loaded ? `${service.state ?? "?"}${service.pid ? ` · pid ${service.pid}` : ""}` : "not loaded by launchd"],
      ["Live clients", String(status.sse_clients)],
    ]),
    open: [["Open logs", "logs"]],
  });

  let slackKind = "off", slackLabel, slackError;
  if (slack.configured) {
    const socket = slack.socket;
    if (socket && socket.state !== "connected") {
      slackKind = socket.last_ok_at ? "warning" : "critical";
      slackLabel = title(socket.state || "connecting");
      slackError = socket.last_error;
    } else if (later(slack.api.last_error_at, slack.api.last_ok_at)) {
      slackKind = "warning";
      slackLabel = "Last call failed";
      slackError = slack.api.last_error;
    } else {
      slackKind = "good";
      slackLabel = socket ? "Connected" : "Healthy";
    }
  }
  const slackCard = connector("Slack", {
    kind: slackKind,
    label: slackLabel,
    error: slackError,
    details: slack.configured
      ? facts([
          ["Workspace", slack.identity ? `${slack.identity.team} as @${slack.identity.bot}` : "not reached yet"],
          ["Mode", slack.mode === "socket" ? "Socket Mode" : "HTTP interactivity"],
          ["Approvals", slack.channel],
          ["Notifications", slack.notify_channel || "logged only"],
          ["Responders", Object.keys(slack.responders || {}).join(", ") || "anyone in the channel"],
          ["Last API call", slack.api.last_ok_at ? ago(slack.api.last_ok_at) : "none yet"],
        ])
      : el("p", { class: "secondary", text: "Set SLACK_BOT_TOKEN, SLACK_APP_TOKEN and HCP_SLACK_CHANNEL in ~/.config/hcp/hcp.env, then restart the service." }),
    test: "slack",
    open: slack.configured && slack.identity ? [["Open channel", "slack"]] : [],
  });

  let vaultKind = "off", vaultLabel, vaultError;
  if (vault.configured) {
    if (later(vault.health.last_error_at, vault.health.last_ok_at)) {
      vaultKind = "critical";
      vaultLabel = "Write failing";
      vaultError = vault.health.last_error;
    } else if ((db.vault_outbox || 0) > 50) {
      vaultKind = "warning";
      vaultLabel = "Behind";
    } else {
      vaultKind = "good";
    }
  }
  const lastWrite = vault.last_write_at;
  const vaultCard = connector("Obsidian", {
    kind: vaultKind,
    label: vaultLabel,
    error: vaultError,
    details: vault.configured
      ? facts([
          ["Vault", shortPath(vault.vault_path), vault.vault_path],
          ["Obsidian", vault.looks_like_obsidian ? "vault found" : "no .obsidian folder"],
          ["Last write", lastWrite ? ago(lastWrite) : "none since start"],
          ["Written", `${compact(vault.written_since_start)} since start`],
          ["Queued", compact(db.vault_outbox)],
        ])
      : el("p", { class: "secondary", text: "No vault is configured (HCP_OBSIDIAN_VAULT)." }),
    test: "vault",
    open: vault.configured ? [["Open vault", "vault"]] : [],
  });

  const dbKind = !db.ok ? "critical" : db.latency_ms > 500 ? "warning" : "good";
  const dbCard = connector("Neon Postgres", {
    kind: dbKind,
    label: !db.ok ? "Unreachable" : db.latency_ms > 500 ? "Slow" : "Healthy",
    error: db.error,
    details: db.ok
      ? facts([
          ["Host", shortHost(db.host), db.host],
          ["Database", `${db.database} · Postgres ${db.server_version}`],
          ["Round trip", `${db.latency_ms} ms`],
          ["Size", bytes(db.size_bytes)],
          ["Rows", `${compact(db.requests)} requests · ${compact(db.audit_events)} events`],
          ["Schema", `version ${db.schema_version} · ${compact(db.api_keys)} active keys`],
        ])
      : null,
    test: "database",
    open: [["Open Neon console", "neon"]],
  });

  $("connectors").replaceChildren(server, slackCard, vaultCard, dbCard);
  $("server-note").textContent = `checked ${ago(new Date().toISOString())}`;
}

async function runTest(name) {
  state.tests[name] = { running: true };
  renderConnectors();
  try {
    state.tests[name] = await invoke("test_connector", { name });
  } catch (error) {
    state.tests[name] = { ok: false, message: String(error), latency_ms: 0 };
  }
  try {
    state.status = await invoke("status");
  } catch {}
  renderConnectors();
}

async function openTarget(target, id) {
  try {
    await invoke("open_target", { target, id: id ?? null });
  } catch (error) {
    problem(String(error));
  }
}

// ── Request drawer ─────────────────────────────────────────────────────────

async function openDrawer(id) {
  const drawer = $("drawer");
  drawer.replaceChildren(el("p", { class: "muted", text: "Loading…" }));
  drawer.hidden = false;
  $("drawer-backdrop").hidden = false;
  try {
    const { request: r, events } = await invoke("request_detail", { id });
    const response = r.response_data ? JSON.stringify(r.response_data, null, 2) : null;
    drawer.replaceChildren(
      el(
        "div",
        { class: "card-head" },
        el("span", { class: "chip", text: `${title(r.intent)} · ${title(r.urgency)}` }),
        el("span", { class: "chip", text: title(r.state) }),
        el(
          "div",
          { class: "actions" },
          el("button", { class: "ghost", type: "button", onclick: () => openTarget("request_note", r.request_id), text: "Find in Obsidian" }),
          el("button", { class: "ghost", type: "button", onclick: closeDrawer, text: "Close" }),
        ),
      ),
      el("h2", { text: r.context_package.summary }),
      el("div", { class: "secondary", text: `${r.agent_id} → ${r.responder_id} · submitted ${clock(r.submitted_at)} · deadline ${clock(r.timeout_at)}` }),
      el("div", { class: "mono muted", text: r.request_id }),
      r.context_package.detail ? el("section", {}, el("h3", { text: "Detail" }), el("pre", { text: r.context_package.detail })) : null,
      r.context_package.metadata
        ? el("section", {}, el("h3", { text: "Metadata" }), el("pre", { text: JSON.stringify(r.context_package.metadata, null, 2) }))
        : null,
      el(
        "section",
        {},
        el("h3", { text: "Answer" }),
        response
          ? el("div", {}, el("p", { class: "secondary", text: `by ${r.responded_by ?? "?"} ${r.responded_at ? `at ${clock(r.responded_at)}` : ""}` }), el("pre", { text: response }))
          : el("p", { class: "muted", text: "Not answered." }),
      ),
      el(
        "section",
        {},
        el("h3", { text: `If nobody answers: ${title(r.timeout_policy.fallback)}` }),
      ),
      el(
        "section",
        {},
        el("h3", { text: "Audit trail" }),
        el(
          "ol",
          { class: "timeline" },
          (events || []).map((e) =>
            el(
              "li",
              {},
              el("time", { datetime: e.created_at, text: new Date(e.created_at).toLocaleString() }),
              `${EVENT_TEXT[e.event_type] || e.event_type} `,
              el("span", { class: "muted", text: `· ${e.actor}` }),
            ),
          ),
        ),
      ),
    );
  } catch (error) {
    drawer.replaceChildren(el("p", { class: "error", text: String(error) }), el("button", { class: "ghost", type: "button", onclick: closeDrawer, text: "Close" }));
  }
}

function closeDrawer() {
  $("drawer").hidden = true;
  $("drawer-backdrop").hidden = true;
}

// ── Setup ──────────────────────────────────────────────────────────────────

async function boot() {
  const setup = await invoke("setup_state");
  const ready = setup.server.reachable && setup.has_key;
  // The backend's first "live" event can arrive before this page listens; start from what
  // the setup check says, and let later events take over.
  if (!ready) {
    $("live").classList.remove("on");
    $("live-text").textContent = setup.server.reachable ? "Not set up" : "Server offline";
  }
  $("setup").hidden = ready;
  $("dashboard").hidden = !ready;
  if (ready) {
    await refresh();
    return;
  }
  $("setup-why").textContent = setup.server.reachable
    ? `The HCP server at ${setup.base_url} is running.`
    : `HCP Desktop reads from the HCP server at ${setup.base_url}.`;
  $("setup-server").hidden = setup.server.reachable;
  $("setup-server-error").textContent = setup.server.error || "";
  $("setup-key").hidden = !setup.server.reachable || setup.has_key;
  $("create-key").disabled = !setup.hcp_bin_found || !setup.env_file_found;
  if (!setup.hcp_bin_found || !setup.env_file_found) {
    $("setup-error").textContent = `Needs ${setup.hcp_bin} and ${setup.env_file}; paste a key instead, or run deploy/macos/install.sh.`;
  }
}

async function withButton(button, job) {
  button.disabled = true;
  $("setup-error").textContent = "";
  try {
    await job();
    await boot();
  } catch (error) {
    $("setup-error").textContent = String(error);
  } finally {
    button.disabled = false;
  }
}

// ── Wiring ─────────────────────────────────────────────────────────────────

function selectRange(days) {
  state.days = days;
  localStorage.setItem("hcp.days", String(days));
  for (const button of $("range").querySelectorAll("button")) {
    button.setAttribute("aria-pressed", String(Number(button.dataset.days) === days));
  }
}

window.addEventListener("DOMContentLoaded", async () => {
  selectRange(state.days);
  $("range").addEventListener("click", (e) => {
    const days = Number(e.target.dataset?.days);
    if (days) {
      selectRange(days);
      refresh({ withStatus: false });
    }
  });
  for (const button of document.querySelectorAll("[data-table]")) {
    button.addEventListener("click", () => toggleTable(button.dataset.table));
  }
  $("refresh").addEventListener("click", () => ($("dashboard").hidden ? boot() : refresh()));
  $("setup-retry").addEventListener("click", boot);
  $("create-key").addEventListener("click", (e) => withButton(e.target, () => invoke("create_key")));
  $("save-key").addEventListener("click", (e) => withButton(e.target, () => invoke("save_key", { key: $("paste-key").value })));
  $("forget-key").addEventListener("click", async () => {
    await invoke("forget_key");
    await boot();
  });
  $("drawer-backdrop").addEventListener("click", closeDrawer);
  document.addEventListener("keydown", (e) => e.key === "Escape" && closeDrawer());

  const showLive = (payload) => {
    $("live").classList.toggle("on", payload.live);
    $("live-text").textContent = payload.live
      ? "Live"
      : { "no key": "Not set up", starting: "Connecting" }[payload.reason] || "Reconnecting";
  };
  await listen("hcp-change", refreshSoon);
  await listen("hcp-live", ({ payload }) => {
    showLive(payload);
    if (payload.live && !$("dashboard").hidden) refreshSoon();
  });
  // The first event may have gone out before this page was listening.
  showLive(await invoke("live"));

  // Deadlines count down between refreshes; connector status refreshes every 30 seconds.
  setInterval(() => {
    for (const cell of document.querySelectorAll(".countdown")) {
      cell.textContent = until(cell.dataset.due);
      cell.classList.toggle("due-soon", Date.parse(cell.dataset.due) - Date.now() < 5 * 60 * 1000);
    }
  }, 1000);
  setInterval(() => !$("dashboard").hidden && refresh(), 30000);
  let resized = null;
  window.addEventListener("resize", () => {
    hideTooltip();
    clearTimeout(resized);
    resized = setTimeout(render, 150);
  });

  await boot();
});
