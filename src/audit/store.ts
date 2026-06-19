import { query } from "../db/connection.js";
import { generateId } from "../utils/ulid.js";
import type { AuditEvent } from "../types/audit.js";

export async function appendAuditEvent(params: {
  request_id: string;
  event_type: string;
  actor: string;
  actor_type: string;
  payload?: Record<string, unknown>;
}): Promise<AuditEvent> {
  const event: AuditEvent = {
    event_id: generateId(),
    request_id: params.request_id,
    event_type: params.event_type,
    actor: params.actor,
    actor_type: params.actor_type,
    payload: params.payload ?? {},
    created_at: new Date().toISOString(),
  };

  await query(
    `INSERT INTO audit_events (event_id, request_id, event_type, actor, actor_type, payload, created_at)
     VALUES ($1, $2, $3, $4, $5, $6, $7)`,
    [
      event.event_id,
      event.request_id,
      event.event_type,
      event.actor,
      event.actor_type,
      JSON.stringify(event.payload),
      event.created_at,
    ]
  );

  return event;
}

export async function getAuditTrail(filters: {
  request_id?: string;
  event_type?: string;
  limit?: number;
  offset?: number;
}): Promise<AuditEvent[]> {
  const conditions: string[] = [];
  const params: unknown[] = [];
  let idx = 1;

  if (filters.request_id) {
    conditions.push(`request_id = $${idx++}`);
    params.push(filters.request_id);
  }
  if (filters.event_type) {
    conditions.push(`event_type = $${idx++}`);
    params.push(filters.event_type);
  }

  const where = conditions.length > 0 ? `WHERE ${conditions.join(" AND ")}` : "";
  const limit = filters.limit ?? 100;
  const offset = filters.offset ?? 0;
  params.push(limit, offset);

  const { rows } = await query<AuditEvent>(
    `SELECT * FROM audit_events ${where} ORDER BY created_at ASC LIMIT $${idx++} OFFSET $${idx}`,
    params
  );

  return rows;
}
