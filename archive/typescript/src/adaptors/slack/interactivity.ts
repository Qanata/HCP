import type { FastifyInstance } from "fastify";
import { query } from "../../db/connection.js";
import { transitionState } from "../../engine/state-machine.js";
import { appendAuditEvent } from "../../audit/store.js";
import type { CoordinationRequest } from "../../types/cr.js";

export function registerSlackInteractivity(app: FastifyInstance): void {
  app.post("/slack/interactions", async (request, reply) => {
    const body = request.body as Record<string, string>;
    let payload: any;

    try {
      payload = JSON.parse(body.payload ?? "{}");
    } catch {
      return reply.code(400).send({ error: "Invalid payload" });
    }

    if (payload.type !== "block_actions") return reply.code(200).send();

    const action = payload.actions?.[0];
    if (!action) return reply.code(200).send();

    const requestId = action.value;
    const actionId = action.action_id;
    const userId = payload.user?.id ?? "unknown";
    const userName = payload.user?.name ?? "unknown";

    if (!["hcp_approve", "hcp_reject"].includes(actionId)) {
      return reply.code(200).send();
    }

    const { rows } = await query<CoordinationRequest>(
      "SELECT * FROM coordination_requests WHERE request_id = $1",
      [requestId]
    );
    if (!rows[0]) return reply.code(200).send({ text: "Request not found" });

    const cr = rows[0];
    if (cr.state !== "PENDING_RESPONSE") {
      return reply.code(200).send({ text: `Request is already in state: ${cr.state}` });
    }

    const decision = actionId === "hcp_approve" ? "approved" : "rejected";
    const now = new Date().toISOString();

    await transitionState({
      request_id: cr.request_id,
      from: "PENDING_RESPONSE",
      to: "RESPONDED",
      actor: `slack:${userId}`,
      actor_type: "HUMAN",
      payload: { decision, slack_user: userName },
      additionalUpdates: {
        response_data: { decision },
        responded_by: `slack:${userName}`,
        responded_at: now,
      },
    });

    await appendAuditEvent({
      request_id: cr.request_id,
      event_type: "SLACK_INTERACTION",
      actor: `slack:${userId}`,
      actor_type: "HUMAN",
      payload: { action_id: actionId, decision },
    });

    return reply.code(200).send({ text: `Request ${decision}. (ID: ${cr.request_id})` });
  });
}
