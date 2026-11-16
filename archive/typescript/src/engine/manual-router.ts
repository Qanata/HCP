import { transitionState } from "./state-machine.js";
import { notifySlack } from "../adaptors/slack/client.js";
import type { CoordinationRequest } from "../types/cr.js";
import { config } from "../config.js";

export async function routeCR(cr: CoordinationRequest): Promise<void> {
  // Transition SUBMITTED -> ROUTING
  transitionState({
    request_id: cr.request_id,
    from: "SUBMITTED",
    to: "ROUTING",
    actor: "system",
    actor_type: "SYSTEM",
    payload: { responder_id: cr.routing_hints.responder_id },
  });

  // Transition ROUTING -> PENDING_RESPONSE
  transitionState({
    request_id: cr.request_id,
    from: "ROUTING",
    to: "PENDING_RESPONSE",
    actor: "system",
    actor_type: "SYSTEM",
    payload: {
      channel: cr.routing_hints.channel,
      responder_id: cr.routing_hints.responder_id,
    },
  });

  // Notify via Slack if channel is slack and token is configured
  if (cr.routing_hints.channel === "slack" && config.slackBotToken) {
    try {
      await notifySlack(cr);
    } catch (err) {
      console.error(`Failed to send Slack notification for CR ${cr.request_id}:`, err);
    }
  }
}
