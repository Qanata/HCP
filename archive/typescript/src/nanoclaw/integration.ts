/**
 * NanoClaw Integration Artifacts
 *
 * This file documents the IPC task type and MCP tool schema
 * that NanoClaw would register to integrate with HCP.
 *
 * Integration steps:
 * 1. Add HCP_BASE_URL and HCP_API_KEY to NanoClaw's environment
 * 2. Add the IPC task handler to processTaskIpc()
 * 3. Register the MCP tool in ipc-mcp-stdio.ts
 */

/**
 * IPC Task Type for NanoClaw's processTaskIpc() handler.
 *
 * Add this to the switch statement in processTaskIpc():
 *
 * ```typescript
 * case 'hcp_coordinate': {
 *   const { HCPClient } = await import('hcp/sdk');
 *   const client = new HCPClient({
 *     baseUrl: process.env.HCP_BASE_URL!,
 *     apiKey: process.env.HCP_API_KEY!,
 *   });
 *   const result = await client.coordinate(task.params);
 *   return { success: true, data: result };
 * }
 * ```
 */
export const IPC_TASK_TYPE = "hcp_coordinate";

/**
 * MCP Tool Schema for NanoClaw's ipc-mcp-stdio.ts tool registration.
 *
 * Add this to the tools array:
 *
 * ```typescript
 * {
 *   name: 'hcp_coordinate',
 *   description: 'Request human coordination (approval, clarification, escalation, etc.) via the Human Coordination Plane. Blocks until a human responds or the timeout expires.',
 *   inputSchema: {
 *     type: 'object',
 *     properties: {
 *       intent: {
 *         type: 'string',
 *         enum: ['APPROVAL', 'CLARIFICATION', 'ESCALATION', 'NOTIFICATION', 'DECISION', 'REVIEW', 'INPUT'],
 *         description: 'The type of human coordination needed',
 *       },
 *       urgency: {
 *         type: 'string',
 *         enum: ['CRITICAL', 'HIGH', 'MEDIUM', 'LOW'],
 *         description: 'How urgent this request is',
 *       },
 *       context_package: {
 *         type: 'object',
 *         properties: {
 *           summary: { type: 'string', description: 'Brief summary for the human reviewer' },
 *           detail: { type: 'string', description: 'Detailed context (optional)' },
 *           metadata: { type: 'object', description: 'Arbitrary metadata (optional)' },
 *         },
 *         required: ['summary'],
 *       },
 *       timeout_policy: {
 *         type: 'object',
 *         properties: {
 *           timeout_seconds: { type: 'number', description: 'How long to wait for a response' },
 *           fallback: {
 *             type: 'string',
 *             enum: ['AUTO_APPROVE', 'AUTO_REJECT', 'ESCALATE', 'BLOCK', 'FAIL', 'SKIP'],
 *             description: 'What to do if no response before timeout',
 *           },
 *         },
 *         required: ['timeout_seconds', 'fallback'],
 *       },
 *       routing_hints: {
 *         type: 'object',
 *         properties: {
 *           responder_id: { type: 'string', description: 'Who should respond' },
 *           channel: { type: 'string', enum: ['portal', 'slack'], description: 'Notification channel' },
 *           slack_channel_id: { type: 'string', description: 'Slack channel ID (if channel=slack)' },
 *         },
 *         required: ['responder_id'],
 *       },
 *     },
 *     required: ['intent', 'urgency', 'context_package', 'timeout_policy', 'routing_hints'],
 *   },
 * }
 * ```
 */
export const MCP_TOOL_SCHEMA = {
  name: "hcp_coordinate",
  description:
    "Request human coordination via the Human Coordination Plane. Blocks until a human responds or the timeout expires.",
} as const;
