export { HCPClient } from "./client.js";
export type { HCPClientOptions } from "./client.js";

// Re-export types for SDK consumers
export type {
  CreateCRInput,
  SubmitResponseInput,
  CoordinationRequest,
  ContextPackage,
  TimeoutPolicy,
  RoutingHints,
  ResponseSchemaDefinition,
  ResponseOption,
} from "../types/cr.js";
export type { AuditEvent } from "../types/audit.js";
export {
  Intent,
  Urgency,
  State,
  Fallback,
} from "../types/common.js";
export {
  CreateCRSchema,
  SubmitResponseSchema,
  ContextPackageSchema,
  TimeoutPolicySchema,
  RoutingHintsSchema,
  ResponseSchemaDefinition as ResponseSchemaDefinitionSchema,
} from "../types/cr.js";
