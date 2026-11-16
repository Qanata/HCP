import type { FastifyInstance } from "fastify";
import { getClientCount } from "../../utils/sse.js";

export function registerHealthRoutes(app: FastifyInstance): void {
  app.get("/health", async () => {
    return {
      status: "ok",
      timestamp: new Date().toISOString(),
      sse_clients: getClientCount(),
    };
  });
}
