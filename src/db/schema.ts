import type pg from "pg";

const SCHEMA_VERSION = 1;

const CREATE_TABLES = `
  CREATE TABLE IF NOT EXISTS schema_version (
    version INTEGER NOT NULL
  );

  CREATE TABLE IF NOT EXISTS coordination_requests (
    request_id      TEXT PRIMARY KEY,
    agent_id        TEXT NOT NULL,
    intent          TEXT NOT NULL,
    urgency         TEXT NOT NULL,
    state           TEXT NOT NULL DEFAULT 'SUBMITTED',
    context_package JSONB NOT NULL,
    response_schema JSONB,
    timeout_policy  JSONB NOT NULL,
    routing_hints   JSONB NOT NULL,
    trace_id        TEXT,
    idempotency_key TEXT UNIQUE,
    responder_id    TEXT NOT NULL,
    response_data   JSONB,
    responded_by    TEXT,
    responded_at    TIMESTAMPTZ,
    submitted_at    TIMESTAMPTZ NOT NULL,
    updated_at      TIMESTAMPTZ NOT NULL,
    timeout_at      TIMESTAMPTZ NOT NULL,
    delivered_at    TIMESTAMPTZ
  );

  CREATE INDEX IF NOT EXISTS idx_cr_state        ON coordination_requests(state);
  CREATE INDEX IF NOT EXISTS idx_cr_agent_id     ON coordination_requests(agent_id);
  CREATE INDEX IF NOT EXISTS idx_cr_responder_id ON coordination_requests(responder_id);
  CREATE INDEX IF NOT EXISTS idx_cr_timeout_at   ON coordination_requests(timeout_at);

  CREATE TABLE IF NOT EXISTS audit_events (
    event_id    TEXT PRIMARY KEY,
    request_id  TEXT NOT NULL REFERENCES coordination_requests(request_id),
    event_type  TEXT NOT NULL,
    actor       TEXT NOT NULL,
    actor_type  TEXT NOT NULL,
    payload     JSONB NOT NULL DEFAULT '{}',
    created_at  TIMESTAMPTZ NOT NULL
  );

  CREATE INDEX IF NOT EXISTS idx_audit_request_id ON audit_events(request_id);

  CREATE TABLE IF NOT EXISTS api_keys (
    key_id     TEXT PRIMARY KEY,
    key_hash   TEXT NOT NULL UNIQUE,
    agent_id   TEXT NOT NULL,
    label      TEXT NOT NULL,
    scopes     JSONB NOT NULL DEFAULT '[]',
    created_at TIMESTAMPTZ NOT NULL,
    revoked_at TIMESTAMPTZ
  );
`;

export async function ensureSchema(pool: pg.Pool): Promise<void> {
  const client = await pool.connect();
  try {
    await client.query("BEGIN");
    await client.query(CREATE_TABLES);

    const { rows } = await client.query<{ version: number }>(
      "SELECT version FROM schema_version LIMIT 1"
    );

    if (rows.length === 0) {
      await client.query("INSERT INTO schema_version (version) VALUES ($1)", [SCHEMA_VERSION]);
    } else if (rows[0].version < SCHEMA_VERSION) {
      // Future migrations go here
      await client.query("UPDATE schema_version SET version = $1", [SCHEMA_VERSION]);
    }

    await client.query("COMMIT");
  } catch (err) {
    await client.query("ROLLBACK");
    throw err;
  } finally {
    client.release();
  }
}
