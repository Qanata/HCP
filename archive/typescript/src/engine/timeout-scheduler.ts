import { query } from "../db/connection.js";
import { transitionState } from "./state-machine.js";
import type { CoordinationRequest } from "../types/cr.js";

async function processExpiredCRs(): Promise<void> {
  const now = new Date().toISOString();

  const { rows } = await query<CoordinationRequest>(
    `SELECT * FROM coordination_requests
     WHERE state IN ('PENDING_RESPONSE', 'ESCALATED')
     AND timeout_at <= $1`,
    [now]
  );

  for (const cr of rows) {
    const fallback = cr.timeout_policy.fallback;

    try {
      switch (fallback) {
        case "AUTO_APPROVE":
          await transitionState({
            request_id: cr.request_id,
            from: cr.state,
            to: "RESPONDED",
            actor: "system",
            actor_type: "SYSTEM",
            payload: { fallback, auto: true },
            additionalUpdates: {
              response_data: { decision: "approved", auto: true },
              responded_by: "system:auto_approve",
              responded_at: now,
            },
          });
          break;

        case "AUTO_REJECT":
          await transitionState({
            request_id: cr.request_id,
            from: cr.state,
            to: "RESPONDED",
            actor: "system",
            actor_type: "SYSTEM",
            payload: { fallback, auto: true },
            additionalUpdates: {
              response_data: { decision: "rejected", auto: true },
              responded_by: "system:auto_reject",
              responded_at: now,
            },
          });
          break;

        case "ESCALATE": {
          const escalationResponderId = cr.timeout_policy.escalation_responder_id;
          if (!escalationResponderId) {
            await transitionState({
              request_id: cr.request_id,
              from: cr.state,
              to: "TIMED_OUT",
              actor: "system",
              actor_type: "SYSTEM",
              payload: { fallback, reason: "no_escalation_target" },
            });
            break;
          }

          await transitionState({
            request_id: cr.request_id,
            from: cr.state,
            to: "ESCALATED",
            actor: "system",
            actor_type: "SYSTEM",
            payload: { fallback, escalation_responder_id: escalationResponderId },
            additionalUpdates: {
              responder_id: escalationResponderId,
              timeout_at: new Date(
                Date.now() + cr.timeout_policy.timeout_seconds * 1000
              ).toISOString(),
            },
          });

          await transitionState({
            request_id: cr.request_id,
            from: "ESCALATED",
            to: "ROUTING",
            actor: "system",
            actor_type: "SYSTEM",
            payload: { responder_id: escalationResponderId },
          });
          await transitionState({
            request_id: cr.request_id,
            from: "ROUTING",
            to: "PENDING_RESPONSE",
            actor: "system",
            actor_type: "SYSTEM",
            payload: { responder_id: escalationResponderId },
          });
          break;
        }

        default:
          await transitionState({
            request_id: cr.request_id,
            from: cr.state,
            to: "TIMED_OUT",
            actor: "system",
            actor_type: "SYSTEM",
            payload: { fallback },
          });
          break;
      }
    } catch (err) {
      console.error(`Failed to process timeout for CR ${cr.request_id}:`, err);
    }
  }
}

let intervalHandle: ReturnType<typeof setInterval> | null = null;

export function startTimeoutScheduler(intervalMs: number): void {
  if (intervalHandle) return;
  intervalHandle = setInterval(() => {
    processExpiredCRs().catch((err) =>
      console.error("Timeout scheduler error:", err)
    );
  }, intervalMs);
}

export function stopTimeoutScheduler(): void {
  if (intervalHandle) {
    clearInterval(intervalHandle);
    intervalHandle = null;
  }
}
