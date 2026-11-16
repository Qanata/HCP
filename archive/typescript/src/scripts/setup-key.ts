import { randomBytes } from "node:crypto";
import { getPool, closePool } from "../db/connection.js";
import { ensureSchema } from "../db/schema.js";
import { generateId } from "../utils/ulid.js";
import { hashApiKey } from "../api/middleware/auth.js";

const agentId = process.argv[2] || "default-agent";
const label = process.argv[3] || "Default API Key";

const pool = getPool();
await ensureSchema(pool);

const rawKey = `hcp_${randomBytes(32).toString("hex")}`;
const keyHash = hashApiKey(rawKey);
const keyId = generateId();
const now = new Date().toISOString();

await pool.query(
  `INSERT INTO api_keys (key_id, key_hash, agent_id, label, scopes, created_at)
   VALUES ($1, $2, $3, $4, $5, $6)`,
  [keyId, keyHash, agentId, label, "[]", now]
);

console.log(`API key created for agent "${agentId}":`);
console.log(`  Key ID: ${keyId}`);
console.log(`  API Key: ${rawKey}`);
console.log(`  Store this key securely — it cannot be retrieved again.`);

await closePool();
