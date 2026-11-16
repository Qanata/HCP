import { describe, it, expect, beforeEach, afterEach } from "vitest";
import { IMemoryDb, newDb } from "pg-mem";
import { canTransition, transitionState } from "../../src/engine/state-machine.js";
import { setPool } from "../../src/db/connection.js";
import { ensureSchema } from "../../src/db/schema.js";
import { generateId } from "../../src/utils/ulid.js";
import type pg from "pg";

let memDb: IMemoryDb;
let pool: pg.Pool;

async function setupTestDb() {
  memDb = newDb();
  const adapter = memDb.adapters.createPg();
  pool = new adapter.Pool() as unknown as pg.Pool;
  setPool(pool);
  await ensureSchema(pool);
}

async function insertTestCR(state: string = "SUBMITTED"): Promise<string> {
  const id = generateId();
  const now = new Date().toISOString();
  await pool.query(
    `INSERT INTO coordination_requests
     (request_id, agent_id, intent, urgency, state, context_package, timeout_policy, routing_hints, responder_id, submitted_at, updated_at, timeout_at)
     VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)`,
    [
      id, "test-agent", "APPROVAL", "MEDIUM", state,
      JSON.stringify({ summary: "Test" }),
      JSON.stringify({ timeout_seconds: 300, fallback: "FAIL" }),
      JSON.stringify({ responder_id: "test-responder", channel: "portal" }),
      "test-responder", now, now,
      new Date(Date.now() + 300_000).toISOString(),
    ]
  );
  return id;
}

describe("canTransition", () => {
  it("allows valid transitions", () => {
    expect(canTransition("SUBMITTED", "ROUTING")).toBe(true);
    expect(canTransition("ROUTING", "PENDING_RESPONSE")).toBe(true);
    expect(canTransition("PENDING_RESPONSE", "RESPONDED")).toBe(true);
    expect(canTransition("RESPONDED", "DELIVERED")).toBe(true);
  });

  it("rejects invalid transitions", () => {
    expect(canTransition("SUBMITTED", "DELIVERED")).toBe(false);
    expect(canTransition("DELIVERED", "SUBMITTED")).toBe(false);
    expect(canTransition("TIMED_OUT", "RESPONDED")).toBe(false);
    expect(canTransition("CANCELLED", "ROUTING")).toBe(false);
  });

  it("allows cancellation from active states", () => {
    expect(canTransition("SUBMITTED", "CANCELLED")).toBe(true);
    expect(canTransition("ROUTING", "CANCELLED")).toBe(true);
    expect(canTransition("PENDING_RESPONSE", "CANCELLED")).toBe(true);
  });

  it("allows timeout from PENDING_RESPONSE", () => {
    expect(canTransition("PENDING_RESPONSE", "TIMED_OUT")).toBe(true);
  });
});

describe("transitionState", () => {
  beforeEach(async () => { await setupTestDb(); });

  it("transitions state and writes audit event", async () => {
    const id = await insertTestCR("SUBMITTED");

    await transitionState({
      request_id: id,
      from: "SUBMITTED",
      to: "ROUTING",
      actor: "system",
      actor_type: "SYSTEM",
    });

    const { rows } = await pool.query<{ state: string }>(
      "SELECT state FROM coordination_requests WHERE request_id = $1",
      [id]
    );
    expect(rows[0].state).toBe("ROUTING");

    const audit = await pool.query<{ event_type: string }>(
      "SELECT * FROM audit_events WHERE request_id = $1",
      [id]
    );
    expect(audit.rows).toHaveLength(1);
    expect(audit.rows[0].event_type).toBe("CR_ROUTING");
  });

  it("throws on invalid transition", async () => {
    const id = await insertTestCR("SUBMITTED");

    await expect(
      transitionState({ request_id: id, from: "SUBMITTED", to: "DELIVERED", actor: "system", actor_type: "SYSTEM" })
    ).rejects.toThrow("Invalid state transition");
  });

  it("throws on concurrent state change", async () => {
    const id = await insertTestCR("ROUTING");

    await expect(
      transitionState({ request_id: id, from: "SUBMITTED", to: "ROUTING", actor: "system", actor_type: "SYSTEM" })
    ).rejects.toThrow("Failed to transition");
  });

  it("applies additional updates", async () => {
    const id = await insertTestCR("PENDING_RESPONSE");
    const now = new Date().toISOString();

    await transitionState({
      request_id: id,
      from: "PENDING_RESPONSE",
      to: "RESPONDED",
      actor: "test-user",
      actor_type: "HUMAN",
      additionalUpdates: {
        response_data: { decision: "approved" },
        responded_by: "test-user",
        responded_at: now,
      },
    });

    const { rows } = await pool.query<{ state: string; response_data: any; responded_by: string }>(
      "SELECT state, response_data, responded_by FROM coordination_requests WHERE request_id = $1",
      [id]
    );
    expect(rows[0].state).toBe("RESPONDED");
    expect(rows[0].response_data).toEqual({ decision: "approved" });
    expect(rows[0].responded_by).toBe("test-user");
  });
});
