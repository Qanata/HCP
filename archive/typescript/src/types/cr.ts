import { z } from "zod";
import { Intent, Urgency, State, Fallback } from "./common.js";

export const ContextPackageSchema = z.object({
  summary: z.string().min(1),
  detail: z.string().optional(),
  metadata: z.record(z.unknown()).optional(),
  attachments: z
    .array(
      z.object({
        type: z.string(),
        name: z.string(),
        content: z.string(),
      })
    )
    .optional(),
});
export type ContextPackage = z.infer<typeof ContextPackageSchema>;

export const TimeoutPolicySchema = z.object({
  timeout_seconds: z.number().int().positive(),
  fallback: Fallback,
  escalation_responder_id: z.string().optional(),
});
export type TimeoutPolicy = z.infer<typeof TimeoutPolicySchema>;

export const RoutingHintsSchema = z.object({
  responder_id: z.string().min(1),
  channel: z.enum(["portal", "slack"]).default("portal"),
  slack_channel_id: z.string().optional(),
});
export type RoutingHints = z.infer<typeof RoutingHintsSchema>;

export const ResponseOptionSchema = z.object({
  key: z.string(),
  label: z.string(),
  description: z.string().optional(),
});
export type ResponseOption = z.infer<typeof ResponseOptionSchema>;

export const ResponseSchemaDefinition = z.object({
  type: z.enum(["choice", "text", "structured"]),
  options: z.array(ResponseOptionSchema).optional(),
  json_schema: z.record(z.unknown()).optional(),
});
export type ResponseSchemaDefinition = z.infer<typeof ResponseSchemaDefinition>;

export const CreateCRSchema = z.object({
  intent: Intent,
  urgency: Urgency,
  context_package: ContextPackageSchema,
  response_schema: ResponseSchemaDefinition.optional(),
  timeout_policy: TimeoutPolicySchema,
  routing_hints: RoutingHintsSchema,
  trace_id: z.string().optional(),
  idempotency_key: z.string().optional(),
});
export type CreateCRInput = z.infer<typeof CreateCRSchema>;

export const SubmitResponseSchema = z.object({
  response_data: z.record(z.unknown()),
  responded_by: z.string().min(1),
});
export type SubmitResponseInput = z.infer<typeof SubmitResponseSchema>;

export interface CoordinationRequest {
  request_id: string;
  agent_id: string;
  intent: string;
  urgency: string;
  state: string;
  context_package: ContextPackage;
  response_schema: ResponseSchemaDefinition | null;
  timeout_policy: TimeoutPolicy;
  routing_hints: RoutingHints;
  trace_id: string | null;
  idempotency_key: string | null;
  responder_id: string;
  response_data: Record<string, unknown> | null;
  responded_by: string | null;
  responded_at: string | null;
  submitted_at: string;
  updated_at: string;
  timeout_at: string;
  delivered_at: string | null;
}
