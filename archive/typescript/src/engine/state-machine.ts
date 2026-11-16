import { query } from "../db/connection.js";
import { appendAuditEvent } from "../audit/store.js";
import { emitSSE } from "../utils/sse.js";

const VALID_TRANSITIONS: Record<string, string[]> = {
  SUBMITTED: ["ROUTING", "CANCELLED"],
  ROUTING: ["PENDING_RESPONSE", "ESCALATED", "CANCELLED"],
  PENDING_RESPONSE: ["RESPONDED", "ESCALATED", "TIMED_OUT", "CANCELLED"],
  RESPONDED: ["DELIVERED"],
  DELIVERED: [],
  ESCALATED: ["ROUTING", "TIMED_OUT", "CANCELLED"],
  TIMED_OUT: [],
  CANCELLED: [],
};

const CANCELLABLE_STATES = new Set([
  "SUBMITTED",
  "ROUTING",
  "PENDING_RESPONSE",
  "ESCALATED",
]);

export function canTransition(from: string, to: string): boolean {
  return VALID_TRANSITIONS[from]?.includes(to) ?? false;
}

export function isCancellable(state: string): boolean {
  return CANCELLABLE_STATES.has(state);
}

export async function transitionState(params: {
  request_id: string;
  from: string;
  to: string;
  actor: string;
  actor_type: string;
  payload?: Record<string, unknown>;
  additionalUpdates?: Record<string, unknown>;
}): Promise<void> {
  if (!canTransition(params.from, params.to)) {
    throw new Error(`Invalid state transition: ${params.from} -> ${params.to}`);
  }

  const now = new Date().toISOString();
  const setClauses: string[] = ["state = $1", "updated_at = $2"];
  const updateParams: unknown[] = [params.to, now];
  let idx = 3;

  if (params.additionalUpdates) {
    for (const [key, value] of Object.entries(params.additionalUpdates)) {
      setClauses.push(`${key} = $${idx++}`);
      updateParams.push(
        typeof value === "object" && value !== null ? JSON.stringify(value) : value
      );
    }
  }

  updateParams.push(params.request_id, params.from);

  const result = await query(
    `UPDATE coordination_requests SET ${setClauses.join(", ")} WHERE request_id = $${idx++} AND state = $${idx}`,
    updateParams
  );

  if (result.rowCount === 0) {
    throw new Error(
      `Failed to transition CR ${params.request_id}: state may have changed concurrently`
    );
  }

  await appendAuditEvent({
    request_id: params.request_id,
    event_type: `CR_${params.to}`,
    actor: params.actor,
    actor_type: params.actor_type,
    payload: params.payload,
  });

  const { rows } = await query<{ agent_id: string }>(
    "SELECT agent_id FROM coordination_requests WHERE request_id = $1",
    [params.request_id]
  );

  if (rows[0]) {
    emitSSE({
      event: "state_change",
      data: { request_id: params.request_id, state: params.to, agent_id: rows[0].agent_id },
    });
  }
}
