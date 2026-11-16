import { z } from "zod";

export const AuditEventType = z.enum([
  "CR_SUBMITTED",
  "CR_ROUTING",
  "CR_PENDING_RESPONSE",
  "CR_RESPONDED",
  "CR_DELIVERED",
  "CR_ESCALATED",
  "CR_TIMED_OUT",
  "CR_CANCELLED",
  "SLACK_NOTIFIED",
  "SLACK_INTERACTION",
]);
export type AuditEventType = z.infer<typeof AuditEventType>;

export const ActorType = z.enum(["AGENT", "HUMAN", "SYSTEM"]);
export type ActorType = z.infer<typeof ActorType>;

export interface AuditEvent {
  event_id: string;
  request_id: string;
  event_type: string;
  actor: string;
  actor_type: string;
  payload: Record<string, unknown>;
  created_at: string;
}
