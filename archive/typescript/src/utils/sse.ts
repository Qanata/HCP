import type { FastifyReply } from "fastify";

interface SSEClient {
  id: string;
  reply: FastifyReply;
  agentId?: string;
  responderId?: string;
}

const clients: Map<string, SSEClient> = new Map();

export function addSSEClient(client: SSEClient): void {
  clients.set(client.id, client);
}

export function removeSSEClient(id: string): void {
  clients.delete(id);
}

export function emitSSE(event: {
  event: string;
  data: Record<string, unknown>;
}): void {
  const payload = `event: ${event.event}\ndata: ${JSON.stringify(event.data)}\n\n`;

  for (const [id, client] of clients) {
    try {
      client.reply.raw.write(payload);
    } catch {
      clients.delete(id);
    }
  }
}

export function getClientCount(): number {
  return clients.size;
}
