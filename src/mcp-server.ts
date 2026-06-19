#!/usr/bin/env node
/**
 * HCP MCP Server — stdio transport
 *
 * Exposes HCP tools so any agent harness (Hermes, OpenClaw, NanoClaw, etc.)
 * can route ALL human interaction moments through HCP — not just dangerous
 * commands. Any time the agent would ask a question, request permission,
 * surface uncertainty, or flag a decision point, it uses these tools instead
 * of replying in the chat.
 *
 * Usage in an MCP config:
 *   {
 *     "mcpServers": {
 *       "hcp": {
 *         "command": "node",
 *         "args": ["/path/to/hcp/dist/mcp-server.js"],
 *         "env": {
 *           "HCP_BASE_URL": "https://hcp.yourdomain.com",
 *           "HCP_API_KEY": "hcp_xxxx"
 *         }
 *       }
 *     }
 *   }
 *
 * Or for local dev: HCP_BASE_URL=http://localhost:3100 HCP_API_KEY=hcp_xxx npx tsx src/mcp-server.ts
 */

import { HCPClient } from "./sdk/index.js";
import { config } from "./config.js";

const HCP_BASE_URL = process.env.HCP_BASE_URL;
const HCP_API_KEY  = process.env.HCP_API_KEY;
const HCP_RESPONDER = process.env.HCP_RESPONDER ?? "default";
const HCP_SLACK_CHANNEL = process.env.HCP_SLACK_CHANNEL;

if (!HCP_BASE_URL || !HCP_API_KEY) {
  process.stderr.write("HCP MCP Server: HCP_BASE_URL and HCP_API_KEY must be set\n");
  process.exit(1);
}

const client = new HCPClient({ baseUrl: HCP_BASE_URL, apiKey: HCP_API_KEY });

// Default routing used when the caller doesn't override
function defaultRouting(channel?: string) {
  return {
    responder_id: HCP_RESPONDER,
    channel: (channel ?? (HCP_SLACK_CHANNEL ? "slack" : "portal")) as "portal" | "slack",
    ...(HCP_SLACK_CHANNEL ? { slack_channel_id: HCP_SLACK_CHANNEL } : {}),
  };
}

// ── MCP protocol helpers ─────────────────────────────────────────────────────

function send(msg: object) {
  process.stdout.write(JSON.stringify(msg) + "\n");
}

function sendResult(id: number | string, result: unknown) {
  send({ jsonrpc: "2.0", id, result });
}

function sendError(id: number | string | null, code: number, message: string) {
  send({ jsonrpc: "2.0", id, error: { code, message } });
}

// ── Tool definitions ─────────────────────────────────────────────────────────

const TOOLS = [
  {
    name: "hcp_coordinate",
    description: [
      "Route a human interaction moment through HCP and WAIT for the response before continuing.",
      "",
      "Use this tool — instead of asking in the chat — whenever you would:",
      "  • Ask the user a clarifying question before proceeding",
      "  • Request permission to take an action",
      "  • Surface ambiguity you cannot resolve autonomously",
      "  • Ask the user to choose between options",
      "  • Flag a risk or concern and ask how to proceed",
      "  • Need a piece of information only the human can supply",
      "",
      "This is the PRIMARY tool for all human interaction. When in doubt, use this.",
      "Blocks until the human responds via Slack or the portal, then returns their answer.",
    ].join("\n"),
    inputSchema: {
      type: "object",
      required: ["intent", "urgency", "context_package", "timeout_policy", "routing_hints"],
      properties: {
        intent: {
          type: "string",
          enum: ["APPROVAL", "CLARIFICATION", "ESCALATION", "NOTIFICATION", "DECISION", "REVIEW", "INPUT"],
          description: [
            "APPROVAL — you need explicit go/no-go before acting",
            "CLARIFICATION — you have ambiguity you cannot resolve alone",
            "DECISION — you need the human to choose between options",
            "INPUT — you need information only the human can provide",
            "REVIEW — you want human eyes on something before continuing",
            "ESCALATION — situation exceeds your confidence threshold",
            "NOTIFICATION — informational, no response required (use hcp_notify instead for this)",
          ].join("\n"),
        },
        urgency: {
          type: "string",
          enum: ["CRITICAL", "HIGH", "MEDIUM", "LOW"],
          description: "CRITICAL: blocks all progress. HIGH: needed soon. MEDIUM: needed before next major step. LOW: convenient but not urgent.",
        },
        context_package: {
          type: "object",
          required: ["summary"],
          properties: {
            summary: {
              type: "string",
              description: "One clear sentence or paragraph: what the situation is and exactly what you need from the human. Write as if they have no other context.",
            },
            detail: { type: "string", description: "Full context, relevant excerpts, options considered, and why you can't proceed without input." },
            metadata: { type: "object", description: "Structured data: options list, file paths, command being considered, etc." },
          },
        },
        timeout_policy: {
          type: "object",
          required: ["timeout_seconds", "fallback"],
          properties: {
            timeout_seconds: { type: "number", description: "How long to wait. 300 (5m) for CRITICAL, 1800 (30m) for HIGH, 3600 (1h) for MEDIUM/LOW." },
            fallback: {
              type: "string",
              enum: ["AUTO_APPROVE", "AUTO_REJECT", "ESCALATE", "BLOCK", "FAIL", "SKIP"],
              description: "What to do if no response. Default to AUTO_REJECT for approvals (safe), AUTO_APPROVE for low-risk clarifications.",
            },
            escalation_responder_id: { type: "string", description: "Who to escalate to if fallback=ESCALATE." },
          },
        },
        routing_hints: {
          type: "object",
          required: ["responder_id"],
          properties: {
            responder_id: { type: "string", description: "Who should respond. Use the configured default unless you know a better person." },
            channel: { type: "string", enum: ["portal", "slack"], description: "portal: web UI. slack: sends a Slack message." },
            slack_channel_id: { type: "string", description: "Required when channel=slack." },
          },
        },
        trace_id: { type: "string", description: "ID linking this CR to the current agent run/trace." },
        idempotency_key: { type: "string", description: "Prevents duplicate CRs on retry." },
      },
    },
  },

  {
    name: "hcp_notify",
    description: [
      "Send a one-way notification to the human and return immediately. Do NOT wait for a response.",
      "",
      "Use this for:",
      "  • Progress updates during long-running tasks ('Starting database migration...')",
      "  • Completion announcements ('Finished. 42 files processed.')",
      "  • Informational flags that don't require action ('Found 3 deprecated dependencies, continuing.')",
      "  • Session start/end announcements",
      "",
      "If you need the human to DO something or ANSWER something, use hcp_coordinate instead.",
    ].join("\n"),
    inputSchema: {
      type: "object",
      required: ["summary"],
      properties: {
        summary: { type: "string", description: "What happened or what you're about to do. One or two sentences." },
        detail: { type: "string", description: "Optional extended context." },
        metadata: { type: "object", description: "Optional structured data (file counts, durations, etc.)." },
        urgency: {
          type: "string",
          enum: ["HIGH", "MEDIUM", "LOW"],
          description: "LOW for routine updates. MEDIUM for notable events. HIGH for unexpected situations that are FYI but not blocking.",
          default: "LOW",
        },
      },
    },
  },

  {
    name: "hcp_submit",
    description: "Submit a coordination request and return the request_id immediately without waiting. Use when you want to surface something to the human but continue working in parallel. Check the response later with hcp_poll.",
    inputSchema: {
      type: "object",
      required: ["intent", "urgency", "context_package", "timeout_policy", "routing_hints"],
      properties: {
        intent: { type: "string", enum: ["APPROVAL", "CLARIFICATION", "ESCALATION", "NOTIFICATION", "DECISION", "REVIEW", "INPUT"] },
        urgency: { type: "string", enum: ["CRITICAL", "HIGH", "MEDIUM", "LOW"] },
        context_package: {
          type: "object",
          required: ["summary"],
          properties: {
            summary: { type: "string" },
            detail: { type: "string" },
            metadata: { type: "object" },
          },
        },
        timeout_policy: {
          type: "object",
          required: ["timeout_seconds", "fallback"],
          properties: {
            timeout_seconds: { type: "number" },
            fallback: { type: "string", enum: ["AUTO_APPROVE", "AUTO_REJECT", "ESCALATE", "BLOCK", "FAIL", "SKIP"] },
          },
        },
        routing_hints: {
          type: "object",
          required: ["responder_id"],
          properties: {
            responder_id: { type: "string" },
            channel: { type: "string", enum: ["portal", "slack"] },
            slack_channel_id: { type: "string" },
          },
        },
        trace_id: { type: "string" },
        idempotency_key: { type: "string" },
      },
    },
  },

  {
    name: "hcp_poll",
    description: "Check the current state and response of a previously submitted coordination request (from hcp_submit). Returns the response_data once the human has responded.",
    inputSchema: {
      type: "object",
      required: ["request_id"],
      properties: {
        request_id: { type: "string", description: "The request_id returned by hcp_submit." },
      },
    },
  },

  {
    name: "hcp_cancel",
    description: "Cancel a pending coordination request. Use if the question is no longer relevant (e.g. the agent found another way to proceed).",
    inputSchema: {
      type: "object",
      required: ["request_id"],
      properties: {
        request_id: { type: "string" },
      },
    },
  },
];

// ── Tool execution ────────────────────────────────────────────────────────────

async function callTool(name: string, args: Record<string, unknown>): Promise<unknown> {
  switch (name) {

    case "hcp_coordinate": {
      const cr = await client.coordinate(args as any);
      return {
        request_id: cr.request_id,
        state: cr.state,
        response_data: cr.response_data,
        responded_by: cr.responded_by,
        responded_at: cr.responded_at,
      };
    }

    case "hcp_notify": {
      const cr = await client.submit({
        intent: "NOTIFICATION",
        urgency: (args.urgency as any) ?? "LOW",
        context_package: {
          summary: args.summary as string,
          detail: args.detail as string | undefined,
          metadata: args.metadata as Record<string, unknown> | undefined,
        },
        timeout_policy: { timeout_seconds: 3600, fallback: "AUTO_APPROVE" },
        routing_hints: defaultRouting(),
      });
      return { request_id: cr.request_id, state: cr.state };
    }

    case "hcp_submit": {
      const input = args as any;
      if (!input.routing_hints?.responder_id) {
        input.routing_hints = defaultRouting(input.routing_hints?.channel);
      }
      const cr = await client.submit(input);
      return { request_id: cr.request_id, state: cr.state };
    }

    case "hcp_poll": {
      const cr = await client.getRequest(args.request_id as string);
      return {
        request_id: cr.request_id,
        state: cr.state,
        response_data: cr.response_data,
        responded_by: cr.responded_by,
      };
    }

    case "hcp_cancel": {
      return client.cancelRequest(args.request_id as string);
    }

    default:
      throw new Error(`Unknown tool: ${name}`);
  }
}

// ── Message dispatch ──────────────────────────────────────────────────────────

async function handleMessage(msg: any) {
  const { id, method, params } = msg;

  try {
    switch (method) {
      case "initialize":
        sendResult(id, {
          protocolVersion: "2024-11-05",
          capabilities: { tools: {} },
          serverInfo: { name: "hcp", version: "0.1.0" },
        });
        break;

      case "notifications/initialized":
        break;

      case "tools/list":
        sendResult(id, { tools: TOOLS });
        break;

      case "tools/call": {
        const { name, arguments: args } = params;
        try {
          const result = await callTool(name, args ?? {});
          sendResult(id, {
            content: [{ type: "text", text: JSON.stringify(result, null, 2) }],
          });
        } catch (err: any) {
          sendResult(id, {
            content: [{ type: "text", text: `Error: ${err.message}` }],
            isError: true,
          });
        }
        break;
      }

      default:
        sendError(id ?? null, -32601, `Method not found: ${method}`);
    }
  } catch (err: any) {
    sendError(id ?? null, -32603, err.message);
  }
}

// ── stdin reader ──────────────────────────────────────────────────────────────

let buffer = "";

process.stdin.setEncoding("utf8");
process.stdin.on("data", async (chunk: string) => {
  buffer += chunk;
  const lines = buffer.split("\n");
  buffer = lines.pop() ?? "";

  for (const line of lines) {
    const trimmed = line.trim();
    if (!trimmed) continue;
    try {
      const msg = JSON.parse(trimmed);
      await handleMessage(msg);
    } catch {
      sendError(null, -32700, "Parse error");
    }
  }
});

process.stdin.on("end", () => process.exit(0));

process.stderr.write(`HCP MCP Server ready (${HCP_BASE_URL})\n`);
