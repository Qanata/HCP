import type { FastifyInstance } from "fastify";
import { generateId } from "../../utils/ulid.js";
import { addSSEClient, removeSSEClient } from "../../utils/sse.js";

export function registerSSERoutes(app: FastifyInstance): void {
  app.get("/v1/events", async (request, reply) => {
    const clientId = generateId();
    const agentId = (request.query as Record<string, string>).agent_id;
    const responderId = (request.query as Record<string, string>).responder_id;

    reply.raw.writeHead(200, {
      "Content-Type": "text/event-stream",
      "Cache-Control": "no-cache",
      Connection: "keep-alive",
    });

    reply.raw.write(`event: connected\ndata: ${JSON.stringify({ client_id: clientId })}\n\n`);

    addSSEClient({ id: clientId, reply, agentId, responderId });

    request.raw.on("close", () => {
      removeSSEClient(clientId);
    });

    // Keep the connection open — Fastify won't auto-close since we wrote to raw
    await reply;
  });
}
