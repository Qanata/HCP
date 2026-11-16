import { WebClient } from "@slack/web-api";
import { config } from "../../config.js";
import type { CoordinationRequest } from "../../types/cr.js";
import { appendAuditEvent } from "../../audit/store.js";

function getSlackClient(): WebClient | null {
  if (!config.slackBotToken) return null;
  return new WebClient(config.slackBotToken);
}

const URGENCY_EMOJI: Record<string, string> = {
  CRITICAL: ":rotating_light:",
  HIGH: ":warning:",
  MEDIUM: ":large_blue_circle:",
  LOW: ":white_circle:",
};

function buildBlocks(cr: CoordinationRequest): object[] {
  const portalUrl = `${config.baseUrl}/portal/?responder_id=${encodeURIComponent(cr.responder_id)}&request_id=${cr.request_id}`;
  const emoji = URGENCY_EMOJI[cr.urgency] ?? ":grey_question:";

  const blocks: object[] = [
    {
      type: "header",
      text: {
        type: "plain_text",
        text: `${cr.intent} Request`,
      },
    },
    {
      type: "section",
      text: {
        type: "mrkdwn",
        text: `${emoji} *Urgency:* ${cr.urgency}\n*Agent:* ${cr.agent_id}\n*ID:* \`${cr.request_id}\``,
      },
    },
    {
      type: "section",
      text: {
        type: "mrkdwn",
        text: `*Summary:*\n${cr.context_package.summary}`,
      },
    },
  ];

  if (cr.context_package.detail) {
    blocks.push({
      type: "section",
      text: {
        type: "mrkdwn",
        text: `*Detail:*\n${cr.context_package.detail.slice(0, 2000)}`,
      },
    });
  }

  // Decision buttons for APPROVAL intent
  if (cr.intent === "APPROVAL") {
    blocks.push({
      type: "actions",
      block_id: `hcp_actions_${cr.request_id}`,
      elements: [
        {
          type: "button",
          text: { type: "plain_text", text: "Approve" },
          style: "primary",
          action_id: "hcp_approve",
          value: cr.request_id,
        },
        {
          type: "button",
          text: { type: "plain_text", text: "Reject" },
          style: "danger",
          action_id: "hcp_reject",
          value: cr.request_id,
        },
      ],
    });
  }

  // Portal link
  blocks.push({
    type: "section",
    text: {
      type: "mrkdwn",
      text: `<${portalUrl}|View in Portal>`,
    },
  });

  return blocks;
}

export async function notifySlack(cr: CoordinationRequest): Promise<void> {
  const client = getSlackClient();
  if (!client) return;

  const channelId = cr.routing_hints.slack_channel_id;
  if (!channelId) {
    console.warn(`No slack_channel_id in routing_hints for CR ${cr.request_id}`);
    return;
  }

  const blocks = buildBlocks(cr);

  await client.chat.postMessage({
    channel: channelId,
    text: `[HCP] ${cr.intent} request from ${cr.agent_id}: ${cr.context_package.summary}`,
    blocks: blocks as any,
  });

  appendAuditEvent({
    request_id: cr.request_id,
    event_type: "SLACK_NOTIFIED",
    actor: "system",
    actor_type: "SYSTEM",
    payload: { channel_id: channelId },
  });
}
