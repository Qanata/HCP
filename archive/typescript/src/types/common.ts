import { z } from "zod";

export const Intent = z.enum([
  "APPROVAL",
  "CLARIFICATION",
  "ESCALATION",
  "NOTIFICATION",
  "DECISION",
  "REVIEW",
  "INPUT",
]);
export type Intent = z.infer<typeof Intent>;

export const Urgency = z.enum(["CRITICAL", "HIGH", "MEDIUM", "LOW"]);
export type Urgency = z.infer<typeof Urgency>;

export const State = z.enum([
  "SUBMITTED",
  "ROUTING",
  "PENDING_RESPONSE",
  "RESPONDED",
  "DELIVERED",
  "ESCALATED",
  "TIMED_OUT",
  "CANCELLED",
]);
export type State = z.infer<typeof State>;

export const Fallback = z.enum([
  "AUTO_APPROVE",
  "AUTO_REJECT",
  "ESCALATE",
  "BLOCK",
  "FAIL",
  "SKIP",
]);
export type Fallback = z.infer<typeof Fallback>;
