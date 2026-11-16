import { describe, it, expect, beforeEach } from "vitest";
import { IMemoryDb, newDb } from "pg-mem";
import { appendAuditEvent, getAuditTrail } from "../../src/audit/store.js";
import { setPool } from "../../src/db/connection.js";
import { ensureSchema } from "../../src/db/schema.js";
import { generateId } from "../../src/utils/ulid.js";
import type pg from "pg";

let pool: pg.Pool;

async function setupTestDb() {
  const memDb: IMemoryDb = newDb();
  const adapter = memDb.adapters.createPg();
  pool = new adapter.Pool() as unknown as pg.Pool;
  setPool(pool);
  await ensureSchema(pool);
}

async function insertTestCR(): Promise<string> {
  const id = generateId();
  const now = new Date().toISOString();
  await pool.query(
    `INSERT INTO coordination_requests
     (request_id, agent_id, intent, urgency, state, context_package, timeout_policy, routing_hints, responder_id, submitted_at, updated_at, timeout_at)
     VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)`,
    [
      id, "test-agent", "APPROVAL", "MEDIUM", "SUBMITTED",
      JSON.stringify({ summary: "Test" }),
      JSON.stringify({ timeout_seconds: 300, fallback: "FAIL" }),
      JSON.stringify({ responder_id: "test-responder", channel: "portal" }),
      "test-responder", now, now,
      new Date(Date.now() + 300_000).toISOString(),
    ]
  );
  return id;
}

describe("Audit Store", () => {
  beforeEach(async () => { await setupTestDb(); });

  it("appends an audit event", async () => {
    const requestId = await insertTestCR();

    const event = await appendAuditEvent({
      request_id: requestId,
      event_type: "CR_SUBMITTED",
      actor: "test-agent",
      actor_type: "AGENT",
      payload: { intent: "APPROVAL" },
    });

    expect(event.event_id).toBeTruthy();
    expect(event.request_id).toBe(requestId);
    expect(event.event_type).toBe("CR_SUBMITTED");
    expect(event.payload).toEqual({ intent: "APPROVAL" });
  });

  it("retrieves audit trail filtered by request_id", async () => {
    const id1 = await insertTestCR();
    const id2 = await insertTestCR();

    await appendAuditEvent({ request_id: id1, event_type: "CR_SUBMITTED", actor: "agent-1", actor_type: "AGENT" });
    await appendAuditEvent({ request_id: id2, event_type: "CR_SUBMITTED", actor: "agent-2", actor_type: "AGENT" });
    await appendAuditEvent({ request_id: id1, event_type: "CR_ROUTING",   actor: "system",  actor_type: "SYSTEM" });

    const trail = await getAuditTrail({ request_id: id1 });
    expect(trail).toHaveLength(2);
    expect(trail[0].event_type).toBe("CR_SUBMITTED");
    expect(trail[1].event_type).toBe("CR_ROUTING");
  });

  it("filters by event_type", async () => {
    const id = await insertTestCR();

    await appendAuditEvent({ request_id: id, event_type: "CR_SUBMITTED", actor: "agent",  actor_type: "AGENT" });
    await appendAuditEvent({ request_id: id, event_type: "CR_ROUTING",   actor: "system", actor_type: "SYSTEM" });

    const events = await getAuditTrail({ event_type: "CR_ROUTING" });
    expect(events).toHaveLength(1);
    expect(events[0].event_type).toBe("CR_ROUTING");
  });

  it("respects limit and offset", async () => {
    const id = await insertTestCR();

    for (let i = 0; i < 5; i++) {
      await appendAuditEvent({ request_id: id, event_type: "CR_SUBMITTED", actor: `agent-${i}`, actor_type: "AGENT" });
    }

    const page1 = await getAuditTrail({ request_id: id, limit: 2, offset: 0 });
    expect(page1).toHaveLength(2);

    const page2 = await getAuditTrail({ request_id: id, limit: 2, offset: 2 });
    expect(page2).toHaveLength(2);

    const page3 = await getAuditTrail({ request_id: id, limit: 2, offset: 4 });
    expect(page3).toHaveLength(1);
  });
});
