import type { FastifyInstance } from "fastify";
import { getAuditTrail } from "../../audit/store.js";

export function registerAuditRoutes(app: FastifyInstance): void {
  app.get("/v1/audit", async (request) => {
    const q = request.query as Record<string, string | undefined>;

    const events = await getAuditTrail({
      request_id: q.request_id,
      event_type: q.event_type,
      limit: q.limit ? parseInt(q.limit, 10) : undefined,
      offset: q.offset ? parseInt(q.offset, 10) : undefined,
    });

    return { events };
  });
}
