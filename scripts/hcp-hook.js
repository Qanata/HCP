#!/usr/bin/env node
/**
 * HCP Claude Code Hook
 *
 * Handles PreToolUse, PostToolUse, Notification, and Stop hook events.
 * Routes every meaningful agent moment through HCP — not just risky commands.
 *
 * Three tiers per event:
 *   SAFE   — read-only operations, pass through silently
 *   NOTIFY — regular operations, fire-and-forget notification to HCP (non-blocking)
 *   GATE   — destructive/irreversible operations, block until approved or rejected
 *
 * Install all four hooks in .claude/settings.json:
 *
 *   {
 *     "hooks": {
 *       "PreToolUse": [{ "matcher": "", "hooks": [{ "type": "command", "command": "node /path/to/hcp/scripts/hcp-hook.js" }] }],
 *       "PostToolUse": [{ "matcher": "", "hooks": [{ "type": "command", "command": "node /path/to/hcp/scripts/hcp-hook.js" }] }],
 *       "Notification": [{ "matcher": "", "hooks": [{ "type": "command", "command": "node /path/to/hcp/scripts/hcp-hook.js" }] }],
 *       "Stop": [{ "matcher": "", "hooks": [{ "type": "command", "command": "node /path/to/hcp/scripts/hcp-hook.js" }] }]
 *     }
 *   }
 *
 * Required env vars:
 *   HCP_BASE_URL      — e.g. https://hcp.yourdomain.com
 *   HCP_API_KEY       — hcp_xxxx
 *   HCP_RESPONDER     — your responder ID (e.g. "isaac")
 *
 * Optional env vars:
 *   HCP_SLACK_CHANNEL — Slack channel ID for gate requests (e.g. "C0123456789")
 *   HCP_HOOK_TIMEOUT  — seconds to wait for approval (default: 300)
 *   HCP_HOOK_MODE     — gate_only | notify_and_gate (default) | gate_all
 *                       gate_only:      only block on GATE patterns, ignore everything else
 *                       notify_and_gate: notify on NOTIFY patterns, block on GATE patterns
 *                       gate_all:       block on everything except SAFE patterns
 */

// ── Config ───────────────────────────────────────────────────────────────────

const HCP_BASE_URL    = process.env.HCP_BASE_URL;
const HCP_API_KEY     = process.env.HCP_API_KEY;
const HCP_RESPONDER   = process.env.HCP_RESPONDER ?? "default";
const HCP_SLACK_CHANNEL = process.env.HCP_SLACK_CHANNEL;
const TIMEOUT_SECONDS = parseInt(process.env.HCP_HOOK_TIMEOUT ?? "300", 10);
const HOOK_MODE       = process.env.HCP_HOOK_MODE ?? "notify_and_gate";

// ── Tier definitions ─────────────────────────────────────────────────────────

// SAFE: read-only, purely informational — always pass through silently
const SAFE_BASH = [
  /^(ls|ll|la|l)\b/,
  /^(cat|head|tail|less|more|wc)\b/,
  /^(grep|rg|ag|ack|find)\b/,
  /^(echo|printf|pwd|which|whereis|type)\b/,
  /^(env|printenv|export -p)\b/,
  /^(ps|top|htop|pstree)\b/,
  /^(git\s+(status|log|diff|show|branch|remote|tag|stash\s+list|ls-files))\b/,
  /^(npm\s+(list|ls|outdated|audit))\b/,
  /^(npx\s+tsc\s+--noEmit)\b/,
  /^(curl\s+.*--head|-I)\b/,
  /^(date|cal|uptime|uname)\b/,
  /^(df|du|free|vmstat)\b/,
];

// GATE: destructive / irreversible / high-impact — block and require approval
const GATE_BASH = [
  /\brm\s+-[rf]/,
  /\bgit\s+(push(\s+--force)?|reset\s+--hard|branch\s+-[Dd]|tag\s+-d)\b/,
  /\bnpm\s+publish\b/,
  /\bdrop\s+(table|database|schema)\b/i,
  /\btruncate\s+table\b/i,
  /\bkubectl\s+(delete|apply|scale|rollout\s+restart)\b/,
  /\bterraform\s+(apply|destroy|force-unlock)\b/,
  /\baws\s+.*\s+(delete|remove|terminate|destroy|deregister)\b/,
  /\bheroku\b.*\b(destroy|delete)\b/,
  /\bchmod\s+(-R\s+)?[0-7]*7[0-7][0-7]\b/,
  /\bchown\s+-R\b/,
  /\bmkfs\b/,
  /\bdd\s+if=/,
  /\bsystemctl\s+(stop|disable|mask)\b/,
  /\bkill\s+-9\b/,
  /\bcrontab\s+-r\b/,
  />\s*\/dev\/sd[a-z]/,
];

// Everything else is NOTIFY tier — e.g. writes, installs, API calls, scripts

// Non-bash tools that are always NOTIFY (never GATE, never SAFE)
const NOTIFY_TOOLS = new Set(["Write", "Edit", "MultiEdit", "WebFetch", "WebSearch"]);

// ── Routing helpers ──────────────────────────────────────────────────────────

function routingHints() {
  return {
    responder_id: HCP_RESPONDER,
    channel: HCP_SLACK_CHANNEL ? "slack" : "portal",
    ...(HCP_SLACK_CHANNEL ? { slack_channel_id: HCP_SLACK_CHANNEL } : {}),
  };
}

function bashTier(command) {
  if (SAFE_BASH.some((p) => p.test(command.trim()))) return "safe";
  if (GATE_BASH.some((p) => p.test(command)))         return "gate";
  return "notify";
}

// ── HCP API client (zero dependencies) ───────────────────────────────────────

async function hcpPost(path, body) {
  const res = await fetch(`${HCP_BASE_URL}${path}`, {
    method: "POST",
    headers: { "Content-Type": "application/json", Authorization: `Bearer ${HCP_API_KEY}` },
    body: JSON.stringify(body),
  });
  if (!res.ok) throw new Error(`HCP ${res.status}: ${await res.text()}`);
  return res.json();
}

async function hcpGet(path) {
  const res = await fetch(`${HCP_BASE_URL}${path}`, {
    headers: { Authorization: `Bearer ${HCP_API_KEY}` },
  });
  if (!res.ok) throw new Error(`HCP ${res.status}`);
  return res.json();
}

async function notify(summary, metadata = {}) {
  return hcpPost("/v1/requests", {
    intent: "NOTIFICATION",
    urgency: "LOW",
    context_package: { summary, metadata },
    timeout_policy: { timeout_seconds: 3600, fallback: "AUTO_APPROVE" },
    routing_hints: routingHints(),
  });
}

async function gate(summary, detail, urgency = "HIGH") {
  return hcpPost("/v1/requests", {
    intent: "APPROVAL",
    urgency,
    context_package: {
      summary,
      detail: detail || undefined,
      metadata: { source: "claude-code-hook" },
    },
    timeout_policy: { timeout_seconds: TIMEOUT_SECONDS, fallback: "AUTO_REJECT" },
    routing_hints: routingHints(),
  });
}

const TERMINAL = new Set(["DELIVERED", "TIMED_OUT", "CANCELLED"]);

async function waitForResponse(requestId) {
  const deadline = Date.now() + TIMEOUT_SECONDS * 1000 + 5000;
  while (Date.now() < deadline) {
    await new Promise((r) => setTimeout(r, 3000));
    const cr = await hcpGet(`/v1/requests/${requestId}`);
    if (TERMINAL.has(cr.state)) return cr;
  }
  throw new Error("timed out");
}

// ── stdin ────────────────────────────────────────────────────────────────────

async function readStdin() {
  return new Promise((resolve) => {
    let data = "";
    process.stdin.setEncoding("utf8");
    process.stdin.on("data", (c) => (data += c));
    process.stdin.on("end", () => { try { resolve(JSON.parse(data)); } catch { resolve({}); } });
  });
}

// ── Hook handlers ─────────────────────────────────────────────────────────────

async function handlePreToolUse(input) {
  const toolName  = input.tool_name ?? "";
  const toolInput = input.tool_input ?? {};

  if (toolName === "Bash") {
    const command = toolInput.command ?? "";
    const tier    = bashTier(command);

    if (tier === "safe") return allow();

    if (tier === "gate" || HOOK_MODE === "gate_all") {
      let cr;
      try {
        cr = await gate(
          `Claude Code wants to run: \`${command.slice(0, 200)}\``,
          command.length > 200 ? command : undefined,
          tier === "gate" ? "HIGH" : "MEDIUM"
        );
      } catch (err) {
        process.stderr.write(`[hcp-hook] HCP unreachable, allowing: ${err.message}\n`);
        return allow();
      }

      process.stderr.write(`[hcp-hook] Awaiting approval for bash command (${cr.request_id})…\n`);
      if (!HCP_SLACK_CHANNEL) {
        process.stderr.write(`[hcp-hook] ${HCP_BASE_URL}/portal/?responder_id=${HCP_RESPONDER}&request_id=${cr.request_id}\n`);
      }

      let result;
      try { result = await waitForResponse(cr.request_id); }
      catch {
        process.stderr.write("[hcp-hook] Timed out — blocking.\n");
        return block("HCP approval timed out");
      }

      if (result.state === "DELIVERED" && result.response_data?.decision === "approved") {
        process.stderr.write(`[hcp-hook] Approved by ${result.responded_by}.\n`);
        return allow();
      }

      const reason = result.response_data?.comment ?? result.response_data?.reason ?? result.state;
      process.stderr.write(`[hcp-hook] Rejected (${reason}).\n`);
      return block(reason);
    }

    // NOTIFY tier (notify_and_gate mode)
    if (HOOK_MODE !== "gate_only") {
      try {
        await notify(
          `Running: \`${command.slice(0, 200)}\``,
          { tool: "Bash", command }
        );
      } catch { /* non-blocking, swallow */ }
    }
    return allow();
  }

  // Non-bash tools
  if (NOTIFY_TOOLS.has(toolName) && HOOK_MODE !== "gate_only") {
    const summary = buildToolSummary(toolName, toolInput);
    try { await notify(summary, { tool: toolName, input: toolInput }); } catch { /* swallow */ }
  }

  return allow();
}

async function handlePostToolUse(input) {
  if (HOOK_MODE === "gate_only") return allow();

  const toolName   = input.tool_name ?? "";
  const toolOutput = input.tool_output ?? "";

  // Only emit post-tool notifications for writes and edits (not reads)
  const significant = ["Write", "Edit", "MultiEdit", "Bash"];
  if (!significant.includes(toolName)) return allow();

  const toolInput = input.tool_input ?? {};
  const summary   = `Completed ${toolName}: ${buildToolSummary(toolName, toolInput)}`;

  try {
    await notify(summary, { tool: toolName, output_preview: String(toolOutput).slice(0, 300) });
  } catch { /* swallow */ }

  return allow();
}

async function handleNotification(input) {
  if (HOOK_MODE === "gate_only") return allow();

  const message = input.message ?? input.notification ?? JSON.stringify(input);
  try {
    await notify(`Agent notification: ${message}`, { source: "claude-code-notification" });
  } catch { /* swallow */ }

  return allow();
}

async function handleStop(input) {
  const reason  = input.stop_reason ?? input.reason ?? "session_end";
  const summary = input.session_summary ?? input.message ?? "";

  try {
    await notify(
      `Claude Code session ended (${reason})${summary ? `: ${summary.slice(0, 300)}` : ""}`,
      { source: "claude-code-stop", reason }
    );
  } catch { /* swallow */ }

  return allow();
}

// ── Output helpers ────────────────────────────────────────────────────────────

function allow() { process.exit(0); }

function block(reason) {
  process.stdout.write(JSON.stringify({ decision: "block", reason }));
  process.exit(1);
}

function buildToolSummary(toolName, toolInput) {
  if (toolName === "Bash")      return `\`${(toolInput.command ?? "").slice(0, 120)}\``;
  if (toolName === "Write")     return `write ${toolInput.file_path ?? ""}`;
  if (toolName === "Edit")      return `edit ${toolInput.file_path ?? ""}`;
  if (toolName === "MultiEdit") return `edit ${toolInput.file_path ?? ""}`;
  if (toolName === "WebFetch")  return `fetch ${toolInput.url ?? ""}`;
  if (toolName === "WebSearch") return `search "${toolInput.query ?? ""}"`;
  return toolName;
}

// ── Main ─────────────────────────────────────────────────────────────────────

async function main() {
  if (!HCP_BASE_URL || !HCP_API_KEY) return allow();

  const input     = await readStdin();
  const hookEvent = input.hook_event_name ?? "";

  try {
    switch (hookEvent) {
      case "PreToolUse":    return await handlePreToolUse(input);
      case "PostToolUse":   return await handlePostToolUse(input);
      case "Notification":  return await handleNotification(input);
      case "Stop":          return await handleStop(input);
      default:
        // Unknown event — pass through
        return allow();
    }
  } catch (err) {
    process.stderr.write(`[hcp-hook] Error in ${hookEvent}: ${err.message}\n`);
    return allow(); // always fail open
  }
}

main();
