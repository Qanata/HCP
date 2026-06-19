# HCP — Human Coordination Plane

A standalone TypeScript/Node.js service that bridges autonomous AI agents and human decision-makers. Agents call HCP to surface decision points (approvals, clarifications, escalations), humans respond via a web portal or Slack, and the service maintains a full audit trail.

Built for [NanoClaw](https://github.com/your-org/nanoclaw) — a WhatsApp-based personal Claude assistant — but usable by any agent system that can make HTTP calls.

---

## Table of Contents

- [Overview](#overview)
- [Architecture](#architecture)
- [Quick Start](#quick-start)
- [Configuration](#configuration)
- [API Reference](#api-reference)
- [State Machine](#state-machine)
- [Timeout & Fallback Behaviors](#timeout--fallback-behaviors)
- [TypeScript SDK](#typescript-sdk)
- [Web Portal](#web-portal)
- [Slack Integration](#slack-integration)
- [NanoClaw Integration](#nanoclaw-integration)
- [Authentication](#authentication)
- [Database Schema](#database-schema)
- [Type Reference](#type-reference)
- [Testing](#testing)
- [Project Structure](#project-structure)
- [Development](#development)

---

## Overview

Agents currently have no way to request human approval, clarification, or escalation — they either proceed autonomously or fail silently. HCP fills this gap:

1. An agent **submits a Coordination Request (CR)** with context, urgency, and a timeout policy.
2. HCP **routes** the CR to the designated human responder via the web portal or Slack.
3. The human **reviews and responds** (approve, reject, provide input, etc.).
4. The agent **polls or receives** the structured response and continues.
5. Every state change is recorded in an **append-only audit log**.

### Tech Stack

| Concern | Choice |
|---------|--------|
| Runtime | Node.js (ES2022) |
| Language | TypeScript (strict mode) |
| HTTP framework | Fastify 5 |
| Database | PostgreSQL via Neon (serverless) |
| Validation | zod |
| IDs | ULID |
| Build | tsup |
| Test | vitest |
| Slack | @slack/web-api |
| Portal | Vanilla HTML + Alpine.js |

---

## Architecture

```
                                    +------------------+
                                    |   Web Portal     |
                                    |   (Alpine.js)    |
                                    +--------+---------+
                                             |
+----------------+    REST API     +---------+----------+     Slack API
|   AI Agent     | -------------> |       HCP Server     | ------------> Slack
| (NanoClaw etc) | <------------- | (Fastify + Postgres) | <----------- (interactions)
+----------------+    Poll/SSE    +---------+----------+
                                             |
                                    +--------+---------+
                                    |   Audit Store    |
                                    |   (append-only)  |
                                    +------------------+
```

**Key components:**

- **Gateway API** — REST endpoints for CR lifecycle management
- **State Machine** — Enforces valid state transitions with optimistic concurrency
- **Manual Router** — Routes CRs to the agent-specified responder
- **Timeout Scheduler** — Polls every 10s for expired CRs, executes fallback behaviors
- **Audit Store** — Append-only event log for every state change
- **SSE** — Real-time event stream for connected clients
- **Slack Adaptor** — Block Kit notifications with interactive buttons
- **Web Portal** — Lightweight SPA for human responders

---

## Quick Start

### Prerequisites

- Node.js 22+
- npm 9+
- A [Neon](https://neon.tech) project (free tier works) — copy the connection string from the Neon dashboard

### Install & Run

```bash
# Install dependencies
npm install

# Create an API key for your agent
npm run setup-key -- my-agent "My Agent Key"
# Output:
#   API key created for agent "my-agent":
#     Key ID: 01ABC...
#     API Key: hcp_a1b2c3...
#     Store this key securely — it cannot be retrieved again.

# Start the server (development mode with hot reload)
npm run dev

# Or build and run in production
npm run build
npm start
```

The server starts on `http://localhost:3100` by default.

### Verify It Works

```bash
# Health check
curl http://localhost:3100/health

# Submit a coordination request
curl -X POST http://localhost:3100/v1/requests \
  -H "Authorization: Bearer hcp_a1b2c3..." \
  -H "Content-Type: application/json" \
  -d '{
    "intent": "APPROVAL",
    "urgency": "HIGH",
    "context_package": {
      "summary": "Deploy v2.1.0 to production",
      "detail": "Includes new auth flow and 3 bug fixes."
    },
    "timeout_policy": {
      "timeout_seconds": 600,
      "fallback": "BLOCK"
    },
    "routing_hints": {
      "responder_id": "ops-lead",
      "channel": "portal"
    }
  }'

# Open the portal to respond
open "http://localhost:3100/portal/?responder_id=ops-lead"

# After responding, poll the result
curl http://localhost:3100/v1/requests/<request_id> \
  -H "Authorization: Bearer hcp_a1b2c3..."
```

---

## Configuration

Set via environment variables or a `.env` file. See `.env.example` for defaults.

| Variable | Default | Description |
|----------|---------|-------------|
| `HCP_PORT` | `3100` | HTTP server port |
| `DATABASE_URL` | _(required)_ | Neon PostgreSQL connection string (`postgresql://...?sslmode=require`) |
| `HCP_BASE_URL` | `http://localhost:3100` | Public base URL (used in Slack messages and portal links) |
| `SLACK_BOT_TOKEN` | _(empty)_ | Slack Bot User OAuth Token (`xoxb-...`). Leave empty to disable Slack. |

Get your `DATABASE_URL` from the Neon dashboard → your project → Connection Details. Use the **pooled** connection string for production.

---

## API Reference

All `/v1/*` endpoints require authentication via `Authorization: Bearer <api_key>` header. Public endpoints (`/health`, `/v1/events`, `/slack/interactions`, `/portal/*`) do not.

### `POST /v1/requests` — Create Coordination Request

Submit a new CR. If an `idempotency_key` is provided and a matching CR exists, the existing CR is returned (200) instead of creating a duplicate (201).

**Request body:** See [CreateCRSchema](#createcrschema).

**Response:** `201 Created` with the full `CoordinationRequest` object.

```bash
curl -X POST http://localhost:3100/v1/requests \
  -H "Authorization: Bearer $HCP_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "intent": "APPROVAL",
    "urgency": "MEDIUM",
    "context_package": { "summary": "Delete 500 inactive user accounts" },
    "timeout_policy": { "timeout_seconds": 3600, "fallback": "AUTO_REJECT" },
    "routing_hints": { "responder_id": "admin", "channel": "portal" }
  }'
```

### `GET /v1/requests/:id` — Get Coordination Request

Fetch a single CR by ID. If the CR is in `RESPONDED` state, it automatically transitions to `DELIVERED` (so agents don't need a separate acknowledge call).

**Response:** `200 OK` with the full `CoordinationRequest` object.

### `GET /v1/requests` — List Coordination Requests

List CRs with optional filters.

| Query Parameter | Type | Description |
|-----------------|------|-------------|
| `agent_id` | string | Filter by agent |
| `state` | string | Filter by state (e.g., `PENDING_RESPONSE`) |
| `intent` | string | Filter by intent type |
| `urgency` | string | Filter by urgency level |
| `responder_id` | string | Filter by assigned responder |
| `limit` | number | Max results (default: 50) |
| `offset` | number | Pagination offset (default: 0) |

**Response:** `200 OK` with `{ requests: CoordinationRequest[] }`.

### `DELETE /v1/requests/:id` — Cancel Coordination Request

Cancel a CR. Only works from cancellable states: `SUBMITTED`, `ROUTING`, `PENDING_RESPONSE`, `ESCALATED`.

**Response:** `200 OK` with `{ status: "cancelled", request_id: "..." }`.

Returns `409 Conflict` if the CR is in a non-cancellable state.

### `POST /v1/requests/:id/respond` — Submit Response

Submit a human response to a CR. Only works when the CR is in `PENDING_RESPONSE` state.

**Request body:**
```json
{
  "response_data": { "decision": "approved", "comment": "LGTM" },
  "responded_by": "ops-lead"
}
```

**Response:** `200 OK` with `{ status: "responded", request_id: "..." }`.

### `GET /v1/audit` — Query Audit Events

Query the append-only audit log.

| Query Parameter | Type | Description |
|-----------------|------|-------------|
| `request_id` | string | Filter by CR |
| `event_type` | string | Filter by event type |
| `limit` | number | Max results (default: 100) |
| `offset` | number | Pagination offset (default: 0) |

**Response:** `200 OK` with `{ events: AuditEvent[] }`.

### `GET /v1/events` — Server-Sent Events

Real-time event stream. Connect with `EventSource` or `curl`.

| Query Parameter | Type | Description |
|-----------------|------|-------------|
| `agent_id` | string | Filter events by agent |
| `responder_id` | string | Filter events by responder |

**Events emitted:**
- `connected` — sent on initial connection with `{ client_id }`.
- `state_change` — sent when any CR changes state, with `{ request_id, state, agent_id }`.

```bash
curl -N http://localhost:3100/v1/events?responder_id=ops-lead
```

### `GET /health` — Health Check

**Response:**
```json
{
  "status": "ok",
  "timestamp": "2025-01-15T12:00:00.000Z",
  "sse_clients": 2
}
```

---

## State Machine

Every CR progresses through a defined set of states. Invalid transitions are rejected.

```
                    +-----------+
                    | SUBMITTED |
                    +-----+-----+
                          |
                    +-----v-----+
               +--->|  ROUTING  |<---+
               |    +-----+-----+    |
               |          |          |
               |    +-----v--------+ |
               |    | PENDING_     | |
               |    | RESPONSE     | |
               |    +--+---+---+---+ |
               |       |   |   |     |
          +----+--+    |   |   +-----+----+
          |ESCALATED|<-+   |   |TIMED_OUT |
          +---------+      |   +----------+
                           |
                    +------v----+
                    | RESPONDED |
                    +------+----+
                           |
                    +------v----+
                    | DELIVERED |
                    +-----------+
```

**Transitions:**

| From | To | Trigger |
|------|----|---------|
| SUBMITTED | ROUTING | Manual router picks up CR |
| ROUTING | PENDING_RESPONSE | Router delivers to responder |
| PENDING_RESPONSE | RESPONDED | Human submits response |
| RESPONDED | DELIVERED | Agent polls the CR (auto-transition) |
| PENDING_RESPONSE | TIMED_OUT | Timeout scheduler fires |
| PENDING_RESPONSE | ESCALATED | Timeout with ESCALATE fallback |
| ESCALATED | ROUTING | Re-routed to escalation responder |
| Any active state | CANCELLED | Agent cancels the CR |

**Terminal states:** `DELIVERED`, `TIMED_OUT`, `CANCELLED`

**Cancellable states:** `SUBMITTED`, `ROUTING`, `PENDING_RESPONSE`, `ESCALATED`

---

## Timeout & Fallback Behaviors

Each CR includes a `timeout_policy` specifying what happens if no human responds before the deadline. The timeout scheduler polls every 10 seconds.

| Fallback | Behavior | Resulting State |
|----------|----------|-----------------|
| `AUTO_APPROVE` | Injects `{ decision: "approved", auto: true }` as the response | RESPONDED |
| `AUTO_REJECT` | Injects `{ decision: "rejected", auto: true }` as the response | RESPONDED |
| `ESCALATE` | Re-routes to `escalation_responder_id` with a fresh timeout | PENDING_RESPONSE (via ESCALATED -> ROUTING) |
| `BLOCK` | Marks the CR as timed out; agent must handle the failure | TIMED_OUT |
| `FAIL` | Same as BLOCK — CR times out | TIMED_OUT |
| `SKIP` | Same as BLOCK — CR times out | TIMED_OUT |

If `ESCALATE` is specified but no `escalation_responder_id` is provided, the CR falls through to `TIMED_OUT`.

---

## TypeScript SDK

The SDK provides a typed client for agent-side integration. Import from the `hcp/sdk` export.

### Installation

```typescript
import { HCPClient } from "hcp/sdk";

const hcp = new HCPClient({
  baseUrl: "http://localhost:3100",
  apiKey: "hcp_a1b2c3...",
});
```

### Methods

#### `submit(input)` — Fire and Forget

Creates a CR and returns immediately. The CR starts in `SUBMITTED` state.

```typescript
const cr = await hcp.submit({
  intent: "APPROVAL",
  urgency: "HIGH",
  context_package: {
    summary: "Deploy v2.1.0 to production",
    detail: "Includes new auth flow and 3 bug fixes.",
  },
  timeout_policy: {
    timeout_seconds: 600,
    fallback: "AUTO_REJECT",
  },
  routing_hints: {
    responder_id: "ops-lead",
    channel: "portal",
  },
});
console.log(cr.request_id); // "01ABC..."
```

#### `coordinate(input, options?)` — Submit and Wait

Submits a CR and polls until it reaches a terminal state (`DELIVERED`, `TIMED_OUT`, or `CANCELLED`). This is the primary method for agent use.

```typescript
const result = await hcp.coordinate(
  {
    intent: "DECISION",
    urgency: "MEDIUM",
    context_package: {
      summary: "Which database should we migrate to?",
      metadata: { options: ["PostgreSQL", "MySQL", "MongoDB"] },
    },
    timeout_policy: { timeout_seconds: 3600, fallback: "FAIL" },
    routing_hints: { responder_id: "tech-lead", channel: "slack", slack_channel_id: "C0123456789" },
  },
  { pollIntervalMs: 3000 }
);

if (result.state === "DELIVERED") {
  console.log("Decision:", result.response_data);
} else {
  console.log("Timed out or cancelled:", result.state);
}
```

| Option | Default | Description |
|--------|---------|-------------|
| `pollIntervalMs` | `2000` | Milliseconds between polls |
| `maxWaitMs` | `timeout_seconds * 1000 + 5000` | Max time to wait before throwing |

#### `getRequest(requestId)` — Poll Status

```typescript
const cr = await hcp.getRequest("01ABC...");
// If state is RESPONDED, auto-transitions to DELIVERED
```

#### `respond(requestId, input)` — Submit Response

```typescript
await hcp.respond("01ABC...", {
  response_data: { decision: "approved" },
  responded_by: "ops-lead",
});
```

#### `cancelRequest(requestId)` — Cancel

```typescript
await hcp.cancelRequest("01ABC...");
```

#### `listRequests(filters?)` — List CRs

```typescript
const { requests } = await hcp.listRequests({
  state: "PENDING_RESPONSE",
  urgency: "CRITICAL",
});
```

#### `queryAudit(filters?)` — Audit Trail

```typescript
const { events } = await hcp.queryAudit({
  request_id: "01ABC...",
});
```

### Re-exported Types

The SDK re-exports all types and zod schemas for convenience:

```typescript
import {
  Intent, Urgency, State, Fallback,
  CreateCRSchema, SubmitResponseSchema,
  type CoordinationRequest, type AuditEvent,
  type CreateCRInput, type SubmitResponseInput,
} from "hcp/sdk";
```

---

## Web Portal

A lightweight Alpine.js SPA served at `/portal/`. Human responders use it to view and respond to pending CRs.

### Access

```
http://localhost:3100/portal/?responder_id=<your-id>
```

Optionally focus on a single CR:

```
http://localhost:3100/portal/?responder_id=ops-lead&request_id=01ABC...
```

### Features

- **Real-time updates** via SSE — new CRs appear automatically
- **Urgency indicators** — color-coded left borders and badges (red/orange/blue/grey)
- **Approve/Reject buttons** for APPROVAL intent CRs
- **Free-text input** for all other intent types (Cmd+Enter to submit)
- **Context display** — summary, detail, metadata, and timestamps
- **Connection status** — shows Connected/Disconnected badge with auto-reconnect

The portal does not require authentication — it's designed for internal network use. The `responder_id` query parameter controls which CRs are displayed.

---

## Slack Integration

HCP can send Block Kit notifications to Slack and handle interactive button clicks.

### Setup

1. Create a Slack app at [api.slack.com/apps](https://api.slack.com/apps)
2. Add Bot Token Scopes: `chat:write`
3. Enable Interactivity and set the Request URL to `https://<your-hcp-url>/slack/interactions`
4. Install the app to your workspace
5. Set `SLACK_BOT_TOKEN=xoxb-...` in your environment

### Usage

Set `channel: "slack"` and provide `slack_channel_id` in routing hints:

```json
{
  "routing_hints": {
    "responder_id": "ops-lead",
    "channel": "slack",
    "slack_channel_id": "C0123456789"
  }
}
```

### Slack Message Format

Messages include:
- **Header** with intent type
- **Urgency indicator** with emoji (CRITICAL=rotating_light, HIGH=warning, MEDIUM=large_blue_circle, LOW=white_circle)
- **Agent ID** and **CR ID**
- **Summary** and **detail** from the context package
- **Approve/Reject buttons** (for APPROVAL intent only)
- **Portal link** for full context and non-approval responses

### Interactivity

When a user clicks Approve or Reject in Slack:
1. Slack sends a POST to `/slack/interactions`
2. HCP transitions the CR from `PENDING_RESPONSE` to `RESPONDED`
3. The response is attributed to `slack:<username>`
4. Both `SLACK_INTERACTION` and `CR_RESPONDED` audit events are recorded

---

## NanoClaw Integration

HCP integrates with NanoClaw via its existing file-based IPC system. See `src/nanoclaw/integration.ts` for copy-paste code.

### Overview

1. Add `HCP_BASE_URL` and `HCP_API_KEY` to NanoClaw's environment
2. Add an `hcp_coordinate` case to `processTaskIpc()`
3. Register the `hcp_coordinate` MCP tool in `ipc-mcp-stdio.ts`

### IPC Handler

```typescript
// In NanoClaw's processTaskIpc():
case 'hcp_coordinate': {
  const { HCPClient } = await import('hcp/sdk');
  const client = new HCPClient({
    baseUrl: process.env.HCP_BASE_URL!,
    apiKey: process.env.HCP_API_KEY!,
  });
  const result = await client.coordinate(task.params);
  return { success: true, data: result };
}
```

### Agent Usage

Once registered, agents can call the tool naturally:

> "I need approval to proceed with the database migration. Let me check with the ops team."
>
> _Agent calls `hcp_coordinate` with intent=APPROVAL, summary="Database migration to PostgreSQL", routing to ops-lead_

---

## Authentication

API keys are SHA256-hashed and stored in the database. The raw key is only shown once at creation time.

### Creating Keys

```bash
npm run setup-key -- <agent-id> "<label>"

# Examples:
npm run setup-key -- nanoclaw "NanoClaw Production"
npm run setup-key -- test-agent "Development Testing"
```

### Using Keys

Include the raw key in the `Authorization` header:

```
Authorization: Bearer hcp_a1b2c3d4e5f6...
```

The `agent_id` associated with the key is automatically attached to all CRs created with that key.

### Key Revocation

Keys can be revoked by setting `revoked_at` in the database:

```sql
UPDATE api_keys SET revoked_at = NOW() WHERE agent_id = 'compromised-agent';
```

---

## Database Schema

PostgreSQL hosted on [Neon](https://neon.tech) (serverless, scale-to-zero). Schema is applied automatically on startup via `ensureSchema()`. Use the pooled connection string in production.

### Tables

#### `coordination_requests`

| Column | Type | Description |
|--------|------|-------------|
| `request_id` | TEXT PK | ULID |
| `agent_id` | TEXT | Agent that submitted the CR |
| `intent` | TEXT | APPROVAL, CLARIFICATION, etc. |
| `urgency` | TEXT | CRITICAL, HIGH, MEDIUM, LOW |
| `state` | TEXT | Current state |
| `context_package` | TEXT (JSON) | Summary, detail, metadata, attachments |
| `response_schema` | TEXT (JSON) | Expected response format |
| `timeout_policy` | TEXT (JSON) | Timeout seconds + fallback behavior |
| `routing_hints` | TEXT (JSON) | Responder ID, channel, Slack channel |
| `trace_id` | TEXT | Optional trace correlation ID |
| `idempotency_key` | TEXT UNIQUE | Prevents duplicate submissions |
| `responder_id` | TEXT | Assigned human responder |
| `response_data` | TEXT (JSON) | Human's response |
| `responded_by` | TEXT | Who responded |
| `responded_at` | TEXT | When they responded (ISO 8601) |
| `submitted_at` | TEXT | Creation timestamp |
| `updated_at` | TEXT | Last state change |
| `timeout_at` | TEXT | Deadline for response |
| `delivered_at` | TEXT | When agent received the response |

**Indexes:** `state`, `agent_id`, `responder_id`, `timeout_at`

#### `audit_events`

| Column | Type | Description |
|--------|------|-------------|
| `event_id` | TEXT PK | ULID |
| `request_id` | TEXT FK | Associated CR |
| `event_type` | TEXT | CR_SUBMITTED, CR_RESPONDED, SLACK_NOTIFIED, etc. |
| `actor` | TEXT | Who triggered the event |
| `actor_type` | TEXT | AGENT, HUMAN, or SYSTEM |
| `payload` | TEXT (JSON) | Event-specific data |
| `created_at` | TEXT | ISO 8601 timestamp |

#### `api_keys`

| Column | Type | Description |
|--------|------|-------------|
| `key_id` | TEXT PK | ULID |
| `key_hash` | TEXT UNIQUE | SHA256 hash of the raw API key |
| `agent_id` | TEXT | Agent this key authenticates as |
| `label` | TEXT | Human-readable label |
| `scopes` | TEXT (JSON) | Reserved for future use |
| `created_at` | TEXT | Creation timestamp |
| `revoked_at` | TEXT | Revocation timestamp (null if active) |

---

## Type Reference

### Enums

```typescript
type Intent = "APPROVAL" | "CLARIFICATION" | "ESCALATION" | "NOTIFICATION" | "DECISION" | "REVIEW" | "INPUT";
type Urgency = "CRITICAL" | "HIGH" | "MEDIUM" | "LOW";
type State = "SUBMITTED" | "ROUTING" | "PENDING_RESPONSE" | "RESPONDED" | "DELIVERED" | "ESCALATED" | "TIMED_OUT" | "CANCELLED";
type Fallback = "AUTO_APPROVE" | "AUTO_REJECT" | "ESCALATE" | "BLOCK" | "FAIL" | "SKIP";
type ActorType = "AGENT" | "HUMAN" | "SYSTEM";
type AuditEventType = "CR_SUBMITTED" | "CR_ROUTING" | "CR_PENDING_RESPONSE" | "CR_RESPONDED" | "CR_DELIVERED" | "CR_ESCALATED" | "CR_TIMED_OUT" | "CR_CANCELLED" | "SLACK_NOTIFIED" | "SLACK_INTERACTION";
```

### CreateCRSchema

```typescript
{
  intent: Intent;                          // Required
  urgency: Urgency;                        // Required
  context_package: {                       // Required
    summary: string;                       //   Required (min 1 char)
    detail?: string;
    metadata?: Record<string, unknown>;
    attachments?: Array<{
      type: string;
      name: string;
      content: string;
    }>;
  };
  response_schema?: {                      // Optional
    type: "choice" | "text" | "structured";
    options?: Array<{ key: string; label: string; description?: string }>;
    json_schema?: Record<string, unknown>;
  };
  timeout_policy: {                        // Required
    timeout_seconds: number;               //   Positive integer
    fallback: Fallback;
    escalation_responder_id?: string;      //   Required if fallback is ESCALATE
  };
  routing_hints: {                         // Required
    responder_id: string;                  //   Required (min 1 char)
    channel?: "portal" | "slack";          //   Default: "portal"
    slack_channel_id?: string;             //   Required if channel is "slack"
  };
  trace_id?: string;
  idempotency_key?: string;
}
```

### SubmitResponseSchema

```typescript
{
  response_data: Record<string, unknown>;  // Required — the actual response
  responded_by: string;                    // Required — who responded (min 1 char)
}
```

---

## Testing

Tests use vitest with in-memory SQLite databases for isolation.

```bash
# Run all tests
npm test

# Run tests in watch mode
npm run test:watch

# Run a specific test file
npx vitest run test/unit/state-machine.test.ts
```

### Test Suite

| File | Tests | Coverage |
|------|-------|----------|
| `test/unit/state-machine.test.ts` | 8 | Valid/invalid transitions, concurrent state changes, additional updates |
| `test/unit/audit-store.test.ts` | 4 | Append events, filter by request_id/event_type, pagination |
| `test/integration/api.test.ts` | 11 | All endpoints, auth, validation, idempotency, 404s |
| `test/integration/e2e-flow.test.ts` | 2 | Full CR lifecycle (submit -> respond -> deliver), cancellation |
| **Total** | **25** | |

---

## Project Structure

```
HCP/
  package.json
  tsconfig.json
  tsup.config.ts
  vitest.config.ts
  .env.example
  src/
    index.ts                          # Entry point: server + timeout scheduler
    config.ts                         # Env var loading with defaults
    db/
      connection.ts                   # Neon/postgres connection pool (DATABASE_URL)
      schema.ts                       # Table definitions + migrations
    types/
      common.ts                       # Intent, Urgency, State, Fallback enums
      cr.ts                           # CR interfaces + zod schemas
      audit.ts                        # Audit event types
    engine/
      state-machine.ts                # State transitions with validation
      timeout-scheduler.ts            # Polls expired CRs, executes fallbacks
      manual-router.ts                # Routes to agent-specified responder
    api/
      server.ts                       # Fastify setup + route registration
      middleware/
        auth.ts                       # Bearer token -> SHA256 lookup
      routes/
        requests.ts                   # CR CRUD + respond endpoint
        audit.ts                      # Audit log queries
        sse.ts                        # Server-sent events stream
        health.ts                     # Health check
    audit/
      store.ts                        # Append-only audit writes + queries
    adaptors/
      slack/
        client.ts                     # Block Kit renderer + Web API
        interactivity.ts              # Slack button click handler
    portal/
      static/
        index.html                    # Responder SPA
        app.js                        # Alpine.js portal logic
        styles.css                    # Minimal styling
    sdk/
      client.ts                       # HCPClient class
      index.ts                        # SDK exports + re-exported types
    nanoclaw/
      integration.ts                  # IPC task type + MCP tool docs
    utils/
      ulid.ts                         # ULID generation
      sse.ts                          # SSE client registry + broadcast
    scripts/
      setup-key.ts                    # CLI to create API keys
  test/
    unit/
      state-machine.test.ts
      audit-store.test.ts
    integration/
      api.test.ts
      e2e-flow.test.ts
```

---

## Development

### Scripts

| Script | Command | Description |
|--------|---------|-------------|
| `dev` | `tsx watch src/index.ts` | Start with hot reload |
| `build` | `tsup` | Build to `dist/` |
| `start` | `node dist/index.js` | Run production build |
| `test` | `vitest run` | Run test suite |
| `test:watch` | `vitest` | Run tests in watch mode |
| `setup-key` | `tsx src/scripts/setup-key.ts` | Create an API key |

### Build Outputs

tsup produces two entry points:

- `dist/index.js` — Server entry point
- `dist/sdk/index.js` — SDK for agent-side consumption (importable as `hcp/sdk`)

Both include source maps and TypeScript declaration files.

---

## AWS Deployment

Production target is **ECS Fargate + Neon** (~$32–52/mo).

```
Route 53 → ACM → ALB → ECS Fargate (hcp service)
                              │
                    ┌─────────┴─────────┐
               Secrets Manager      Neon (Postgres)
          SLACK_BOT_TOKEN, etc.   DATABASE_URL (pooled)
```

### Environment (production)

| Variable | Source |
|----------|--------|
| `DATABASE_URL` | AWS Secrets Manager |
| `SLACK_BOT_TOKEN` | AWS Secrets Manager |
| `HCP_BASE_URL` | `https://hcp.yourdomain.com` |
| `HCP_PORT` | `3100` (ECS task definition) |

### Deployment pipeline

```
Push to main
  → npm run build + npm test
  → Docker build → push to ECR
  → ECS rolling deploy (health check: /health)
```

### Agent API keys

Each agent (nanoclaw, openclaw, hermes, Claude Code sessions) gets its own key:

```bash
# Run inside the ECS task or locally against the deployed DATABASE_URL
npm run setup-key -- nanoclaw "NanoClaw Production"
npm run setup-key -- openclaw "OpenClaw Production"
npm run setup-key -- claude-code "Claude Code Sessions"
```

Set `HCP_BASE_URL` and `HCP_API_KEY` in each agent's environment. For Claude Code terminal sessions, add to your shell profile:

```bash
export HCP_BASE_URL=https://hcp.yourdomain.com
export HCP_API_KEY=hcp_your_key_here
```

---

## License

MIT
