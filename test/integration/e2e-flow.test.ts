import { describe, it, expect, beforeAll, afterAll } from "vitest";
import { IMemoryDb, newDb } from "pg-mem";
import { createServer } from "../../src/api/server.js";
import { setPool } from "../../src/db/connection.js";
import { ensureSchema } from "../../src/db/schema.js";
import { generateId } from "../../src/utils/ulid.js";
import { hashApiKey } from "../../src/api/middleware/auth.js";
import type { FastifyInstance } from "fastify";
import type pg from "pg";

let app: FastifyInstance;
let pool: pg.Pool;
const TEST_API_KEY = "hcp_e2e_test_key";

async function setup() {
  const memDb: IMemoryDb = newDb();
  const adapter = memDb.adapters.createPg();
  pool = new adapter.Pool() as unknown as pg.Pool;
  setPool(pool);
  await ensureSchema(pool);

  const keyHash = hashApiKey(TEST_API_KEY);
  await pool.query(
    `INSERT INTO api_keys (key_id, key_hash, agent_id, label, scopes, created_at)
     VALUES ($1,$2,$3,$4,$5,$6)`,
    [generateId(), keyHash, "e2e-agent", "E2E Key", "[]", new Date().toISOString()]
  );

  app = await createServer();
}

function inject(method: string, url: string, body?: object) {
  const headers: Record<string, string> = { authorization: `Bearer ${TEST_API_KEY}` };
  if (body) headers["content-type"] = "application/json";
  return app.inject({ method: method as any, url, headers, payload: body });
}

describe("E2E Flow: Submit -> Route -> Respond -> Deliver", () => {
  beforeAll(async () => { await setup(); });
  afterAll(async () => { await app?.close(); });

  it("completes a full coordination request lifecycle", async () => {
    const submitRes = await inject("POST", "/v1/requests", {
      intent: "APPROVAL",
      urgency: "HIGH",
      context_package: {
        summary: "Deploy critical hotfix to production",
        detail: "Fixes a data corruption bug affecting 5% of users.",
        metadata: { version: "2.1.1", tickets: ["BUG-123"] },
      },
      timeout_policy: { timeout_seconds: 600, fallback: "BLOCK" },
      routing_hints: { responder_id: "ops-lead", channel: "portal" },
    });

    expect(submitRes.statusCode).toBe(201);
    const cr = JSON.parse(submitRes.body);
    expect(cr.request_id).toBeTruthy();
    expect(cr.state).toBe("SUBMITTED");

    await new Promise((r) => setTimeout(r, 100));

    const checkRes = await inject("GET", `/v1/requests/${cr.request_id}`);
    expect(JSON.parse(checkRes.body).state).toBe("PENDING_RESPONSE");

    const respondRes = await inject("POST", `/v1/requests/${cr.request_id}/respond`, {
      response_data: { decision: "approved", comment: "LGTM, proceed with deploy" },
      responded_by: "ops-lead",
    });
    expect(respondRes.statusCode).toBe(200);

    const deliverRes = await inject("GET", `/v1/requests/${cr.request_id}`);
    const delivered = JSON.parse(deliverRes.body);
    expect(delivered.state).toBe("DELIVERED");
    expect(delivered.response_data).toEqual({ decision: "approved", comment: "LGTM, proceed with deploy" });
    expect(delivered.responded_by).toBe("ops-lead");
    expect(delivered.delivered_at).toBeTruthy();

    const auditRes = await inject("GET", `/v1/audit?request_id=${cr.request_id}`);
    const audit = JSON.parse(auditRes.body);
    expect(audit.events.length).toBeGreaterThanOrEqual(4);

    const eventTypes = audit.events.map((e: any) => e.event_type);
    expect(eventTypes).toContain("CR_SUBMITTED");
    expect(eventTypes).toContain("CR_ROUTING");
    expect(eventTypes).toContain("CR_PENDING_RESPONSE");
    expect(eventTypes).toContain("CR_RESPONDED");
    expect(eventTypes).toContain("CR_DELIVERED");
  });

  it("handles cancellation correctly", async () => {
    const submitRes = await inject("POST", "/v1/requests", {
      intent: "CLARIFICATION",
      urgency: "LOW",
      context_package: { summary: "Need clarification on feature scope" },
      timeout_policy: { timeout_seconds: 300, fallback: "SKIP" },
      routing_hints: { responder_id: "product-manager", channel: "portal" },
    });

    const cr = JSON.parse(submitRes.body);
    await new Promise((r) => setTimeout(r, 100));

    const cancelRes = await inject("DELETE", `/v1/requests/${cr.request_id}`);
    expect(cancelRes.statusCode).toBe(200);

    const checkRes = await inject("GET", `/v1/requests/${cr.request_id}`);
    expect(JSON.parse(checkRes.body).state).toBe("CANCELLED");
  });
});
