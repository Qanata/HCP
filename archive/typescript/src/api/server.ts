import Fastify from "fastify";
import cors from "@fastify/cors";
import rateLimit from "@fastify/rate-limit";
import fastifyStatic from "@fastify/static";
import formbody from "@fastify/formbody";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { authMiddleware } from "./middleware/auth.js";
import { registerHealthRoutes } from "./routes/health.js";
import { registerRequestRoutes } from "./routes/requests.js";
import { registerAuditRoutes } from "./routes/audit.js";
import { registerSSERoutes } from "./routes/sse.js";
import { registerSlackInteractivity } from "../adaptors/slack/interactivity.js";

const __dirname = dirname(fileURLToPath(import.meta.url));

export async function createServer() {
  const app = Fastify({
    logger: {
      level: "info",
    },
  });

  // Plugins
  await app.register(cors, { origin: true });
  await app.register(rateLimit, {
    max: 100,
    timeWindow: "1 minute",
  });
  await app.register(formbody);

  // Serve portal static files
  await app.register(fastifyStatic, {
    root: join(__dirname, "..", "portal", "static"),
    prefix: "/portal/",
    decorateReply: false,
  });

  // Public routes (no auth)
  registerHealthRoutes(app);
  registerSSERoutes(app);
  registerSlackInteractivity(app);

  // Auth-protected routes
  app.register(async (protectedApp) => {
    protectedApp.addHook("onRequest", authMiddleware);
    registerRequestRoutes(protectedApp);
    registerAuditRoutes(protectedApp);
  });

  return app;
}
