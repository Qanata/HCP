#!/usr/bin/env node
/**
 * HCP Agent Key Provisioner
 *
 * Creates API keys for all your agents in one shot and prints the env vars
 * ready to paste into each agent's environment.
 *
 * Usage:
 *   DATABASE_URL=postgresql://... node scripts/provision-agents.js
 *
 * Or with a .env file:
 *   node --env-file=.env scripts/provision-agents.js
 *
 * Agents defined in AGENTS below. Add/remove as needed.
 * Skips agents that already have an active (non-revoked) key.
 */

import { createHash, randomBytes } from "node:crypto";
import pg from "pg";

// ── Define your agents here ──────────────────────────────────────────────────

const AGENTS = [
  { agentId: "claude-code",  label: "Claude Code Sessions" },
  { agentId: "hermes",       label: "Hermes Production" },
  { agentId: "nanoclaw",     label: "NanoClaw Production" },
  { agentId: "openclaw",     label: "OpenClaw Production" },
];

// ── Helpers ──────────────────────────────────────────────────────────────────

function generateUlid() {
  // Minimal ULID: timestamp (10 chars) + random (16 chars), base32 Crockford
  const CHARS = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";
  const now = Date.now();
  let t = now;
  let ts = "";
  for (let i = 9; i >= 0; i--) {
    ts = CHARS[t % 32] + ts;
    t = Math.floor(t / 32);
  }
  let rand = "";
  const bytes = randomBytes(10);
  for (let i = 0; i < 10; i++) {
    rand += CHARS[bytes[i] % 32];
  }
  return (ts + rand).slice(0, 26);
}

function hashKey(raw) {
  return createHash("sha256").update(raw).digest("hex");
}

function generateRawKey() {
  return `hcp_${randomBytes(32).toString("hex")}`;
}

// ── Main ─────────────────────────────────────────────────────────────────────

const DATABASE_URL = process.env.DATABASE_URL;
if (!DATABASE_URL) {
  console.error("Error: DATABASE_URL is not set.");
  process.exit(1);
}

const pool = new pg.Pool({ connectionString: DATABASE_URL, ssl: { rejectUnauthorized: false } });

const results = [];

for (const agent of AGENTS) {
  // Check for existing active key
  const { rows: existing } = await pool.query(
    "SELECT key_id FROM api_keys WHERE agent_id = $1 AND revoked_at IS NULL LIMIT 1",
    [agent.agentId]
  );

  if (existing.length > 0) {
    results.push({ ...agent, status: "skipped", reason: "active key exists" });
    continue;
  }

  const rawKey = generateRawKey();
  const keyId = generateUlid();
  const now = new Date().toISOString();

  await pool.query(
    `INSERT INTO api_keys (key_id, key_hash, agent_id, label, scopes, created_at)
     VALUES ($1, $2, $3, $4, $5, $6)`,
    [keyId, hashKey(rawKey), agent.agentId, agent.label, "[]", now]
  );

  results.push({ ...agent, status: "created", rawKey });
}

await pool.end();

// ── Output ───────────────────────────────────────────────────────────────────

const BASE_URL = process.env.HCP_BASE_URL ?? "https://hcp.yourdomain.com";

console.log("\n╔══════════════════════════════════════════════════════════╗");
console.log("║           HCP Agent Key Provisioning Complete            ║");
console.log("╚══════════════════════════════════════════════════════════╝\n");

for (const r of results) {
  if (r.status === "skipped") {
    console.log(`⏭  ${r.agentId} (${r.label}) — skipped, active key already exists\n`);
    continue;
  }

  console.log(`✅  ${r.agentId} (${r.label})`);
  console.log(`    Key created.\n`);
}

// Print per-agent env blocks for new keys only
const created = results.filter((r) => r.status === "created");
if (created.length === 0) {
  console.log("No new keys were created (all agents already provisioned).");
  process.exit(0);
}

console.log("══════════════════════════════════════════════════════════");
console.log("  ENV VARS — paste into each agent's environment / .env");
console.log("══════════════════════════════════════════════════════════\n");

for (const r of created) {
  console.log(`# ${r.label}`);
  console.log(`export HCP_BASE_URL="${BASE_URL}"`);
  console.log(`export HCP_API_KEY="${r.rawKey}"`);
  console.log();
}

console.log("──────────────────────────────────────────────────────────");
console.log("MCP config snippet (Hermes / OpenClaw / Claude Desktop):");
console.log("──────────────────────────────────────────────────────────\n");

const mcpServers = {};
for (const r of created) {
  if (r.agentId === "claude-code") continue; // claude-code uses hook, not MCP server
  mcpServers[`hcp-${r.agentId}`] = {
    command: "node",
    args: [`${process.cwd()}/dist/mcp-server.js`],
    env: {
      HCP_BASE_URL: BASE_URL,
      HCP_API_KEY: r.rawKey,
    },
  };
}

if (Object.keys(mcpServers).length > 0) {
  console.log(JSON.stringify({ mcpServers }, null, 2));
}

console.log("\n──────────────────────────────────────────────────────────");
console.log("Claude Code hook config (.claude/settings.json):");
console.log("──────────────────────────────────────────────────────────\n");

const claudeCodeKey = created.find((r) => r.agentId === "claude-code");
if (claudeCodeKey) {
  const hookConfig = {
    hooks: {
      PreToolUse: [
        {
          matcher: "Bash",
          hooks: [
            {
              type: "command",
              command: `HCP_BASE_URL=${BASE_URL} HCP_API_KEY=${claudeCodeKey.rawKey} HCP_RESPONDER=isaac node ${process.cwd()}/scripts/hcp-hook.js`,
            },
          ],
        },
      ],
    },
  };
  console.log(JSON.stringify(hookConfig, null, 2));
}

console.log("\n⚠️  Store these keys securely — they cannot be retrieved again.");
