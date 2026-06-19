import type { FastifyInstance } from "fastify";
import { query } from "../../db/connection.js";
import { generateId } from "../../utils/ulid.js";
import { CreateCRSchema, SubmitResponseSchema } from "../../types/cr.js";
import type { CoordinationRequest } from "../../types/cr.js";
import { appendAuditEvent } from "../../audit/store.js";
import { transitionState, isCancellable } from "../../engine/state-machine.js";
import { routeCR } from "../../engine/manual-router.js";

export function registerRequestRoutes(app: FastifyInstance): void {
  app.post("/v1/requests", async (request, reply) => {
    const parsed = CreateCRSchema.safeParse(request.body);
    if (!parsed.success) {
      return reply.code(400).send({ error: "Validation error", details: parsed.error.flatten() });
    }

    const input = parsed.data;

    if (input.idempotency_key) {
      const { rows } = await query<CoordinationRequest>(
        "SELECT * FROM coordination_requests WHERE idempotency_key = $1",
        [input.idempotency_key]
      );
      if (rows[0]) return reply.code(200).send(rows[0]);
    }

    const now = new Date().toISOString();
    const requestId = generateId();
    const timeoutAt = new Date(
      Date.now() + input.timeout_policy.timeout_seconds * 1000
    ).toISOString();

    const cr: CoordinationRequest = {
      request_id: requestId,
      agent_id: request.agentId!,
      intent: input.intent,
      urgency: input.urgency,
      state: "SUBMITTED",
      context_package: input.context_package,
      response_schema: input.response_schema ?? null,
      timeout_policy: input.timeout_policy,
      routing_hints: input.routing_hints,
      trace_id: input.trace_id ?? null,
      idempotency_key: input.idempotency_key ?? null,
      responder_id: input.routing_hints.responder_id,
      response_data: null,
      responded_by: null,
      responded_at: null,
      submitted_at: now,
      updated_at: now,
      timeout_at: timeoutAt,
      delivered_at: null,
    };

    await query(
      `INSERT INTO coordination_requests
       (request_id, agent_id, intent, urgency, state, context_package, response_schema,
        timeout_policy, routing_hints, trace_id, idempotency_key, responder_id,
        response_data, responded_by, responded_at, submitted_at, updated_at, timeout_at, delivered_at)
       VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19)`,
      [
        cr.request_id, cr.agent_id, cr.intent, cr.urgency, cr.state,
        JSON.stringify(cr.context_package),
        cr.response_schema ? JSON.stringify(cr.response_schema) : null,
        JSON.stringify(cr.timeout_policy),
        JSON.stringify(cr.routing_hints),
        cr.trace_id, cr.idempotency_key, cr.responder_id,
        null, null, null,
        cr.submitted_at, cr.updated_at, cr.timeout_at, null,
      ]
    );

    await appendAuditEvent({
      request_id: cr.request_id,
      event_type: "CR_SUBMITTED",
      actor: cr.agent_id,
      actor_type: "AGENT",
      payload: { intent: cr.intent, urgency: cr.urgency },
    });

    routeCR(cr).catch((err) =>
      console.error(`Routing failed for CR ${cr.request_id}:`, err)
    );

    return reply.code(201).send(cr);
  });

  app.get("/v1/requests/:id", async (request, reply) => {
    const { id } = request.params as { id: string };
    const { rows } = await query<CoordinationRequest>(
      "SELECT * FROM coordination_requests WHERE request_id = $1",
      [id]
    );
    if (!rows[0]) return reply.code(404).send({ error: "Not found" });

    const cr = rows[0];

    if (cr.state === "RESPONDED") {
      try {
        await transitionState({
          request_id: cr.request_id,
          from: "RESPONDED",
          to: "DELIVERED",
          actor: request.agentId ?? "system",
          actor_type: request.agentId ? "AGENT" : "SYSTEM",
          additionalUpdates: { delivered_at: new Date().toISOString() },
        });
        cr.state = "DELIVERED";
        cr.delivered_at = new Date().toISOString();
      } catch {
        // Concurrent transition — return current state
      }
    }

    return cr;
  });

  app.get("/v1/requests", async (request) => {
    const q = request.query as Record<string, string | undefined>;
    const conditions: string[] = [];
    const params: unknown[] = [];
    let idx = 1;

    if (q.agent_id)     { conditions.push(`agent_id = $${idx++}`);     params.push(q.agent_id); }
    if (q.state)        { conditions.push(`state = $${idx++}`);         params.push(q.state); }
    if (q.intent)       { conditions.push(`intent = $${idx++}`);        params.push(q.intent); }
    if (q.urgency)      { conditions.push(`urgency = $${idx++}`);       params.push(q.urgency); }
    if (q.responder_id) { conditions.push(`responder_id = $${idx++}`);  params.push(q.responder_id); }

    const where = conditions.length > 0 ? `WHERE ${conditions.join(" AND ")}` : "";
    const limit = q.limit ? parseInt(q.limit, 10) : 50;
    const offset = q.offset ? parseInt(q.offset, 10) : 0;
    params.push(limit, offset);

    const { rows } = await query<CoordinationRequest>(
      `SELECT * FROM coordination_requests ${where} ORDER BY submitted_at DESC LIMIT $${idx++} OFFSET $${idx}`,
      params
    );

    return { requests: rows };
  });

  app.delete("/v1/requests/:id", async (request, reply) => {
    const { id } = request.params as { id: string };
    const { rows } = await query<CoordinationRequest>(
      "SELECT * FROM coordination_requests WHERE request_id = $1",
      [id]
    );
    if (!rows[0]) return reply.code(404).send({ error: "Not found" });

    const cr = rows[0];
    if (!isCancellable(cr.state)) {
      return reply.code(409).send({ error: `Cannot cancel CR in state ${cr.state}` });
    }

    await transitionState({
      request_id: cr.request_id,
      from: cr.state,
      to: "CANCELLED",
      actor: request.agentId ?? "system",
      actor_type: request.agentId ? "AGENT" : "SYSTEM",
    });

    return { status: "cancelled", request_id: cr.request_id };
  });

  app.post("/v1/requests/:id/respond", async (request, reply) => {
    const { id } = request.params as { id: string };
    const parsed = SubmitResponseSchema.safeParse(request.body);
    if (!parsed.success) {
      return reply.code(400).send({ error: "Validation error", details: parsed.error.flatten() });
    }

    const { rows } = await query<CoordinationRequest>(
      "SELECT * FROM coordination_requests WHERE request_id = $1",
      [id]
    );
    if (!rows[0]) return reply.code(404).send({ error: "Not found" });

    const cr = rows[0];
    if (cr.state !== "PENDING_RESPONSE") {
      return reply.code(409).send({ error: `Cannot respond to CR in state ${cr.state}` });
    }

    const now = new Date().toISOString();
    await transitionState({
      request_id: cr.request_id,
      from: "PENDING_RESPONSE",
      to: "RESPONDED",
      actor: parsed.data.responded_by,
      actor_type: "HUMAN",
      payload: { response_data: parsed.data.response_data },
      additionalUpdates: {
        response_data: parsed.data.response_data,
        responded_by: parsed.data.responded_by,
        responded_at: now,
      },
    });

    return { status: "responded", request_id: cr.request_id };
  });
}
