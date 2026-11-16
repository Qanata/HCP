import type { CreateCRInput, CoordinationRequest, SubmitResponseInput } from "../types/cr.js";
import type { AuditEvent } from "../types/audit.js";

export interface HCPClientOptions {
  baseUrl: string;
  apiKey: string;
}

export class HCPClient {
  private baseUrl: string;
  private apiKey: string;

  constructor(options: HCPClientOptions) {
    this.baseUrl = options.baseUrl.replace(/\/$/, "");
    this.apiKey = options.apiKey;
  }

  private async fetch<T>(
    path: string,
    options: RequestInit = {}
  ): Promise<T> {
    const url = `${this.baseUrl}${path}`;
    const response = await globalThis.fetch(url, {
      ...options,
      headers: {
        "Content-Type": "application/json",
        Authorization: `Bearer ${this.apiKey}`,
        ...options.headers,
      },
    });

    if (!response.ok) {
      const body = await response.text();
      throw new Error(`HCP API error ${response.status}: ${body}`);
    }

    return response.json() as Promise<T>;
  }

  async submit(input: CreateCRInput): Promise<CoordinationRequest> {
    return this.fetch<CoordinationRequest>("/v1/requests", {
      method: "POST",
      body: JSON.stringify(input),
    });
  }

  async getRequest(requestId: string): Promise<CoordinationRequest> {
    return this.fetch<CoordinationRequest>(`/v1/requests/${requestId}`);
  }

  async cancelRequest(requestId: string): Promise<{ status: string; request_id: string }> {
    return this.fetch(`/v1/requests/${requestId}`, { method: "DELETE" });
  }

  async listRequests(filters?: {
    agent_id?: string;
    state?: string;
    intent?: string;
    urgency?: string;
    responder_id?: string;
    limit?: number;
    offset?: number;
  }): Promise<{ requests: CoordinationRequest[] }> {
    const params = new URLSearchParams();
    if (filters) {
      for (const [key, value] of Object.entries(filters)) {
        if (value !== undefined) params.set(key, String(value));
      }
    }
    const qs = params.toString();
    return this.fetch(`/v1/requests${qs ? `?${qs}` : ""}`);
  }

  async respond(
    requestId: string,
    input: SubmitResponseInput
  ): Promise<{ status: string; request_id: string }> {
    return this.fetch(`/v1/requests/${requestId}/respond`, {
      method: "POST",
      body: JSON.stringify(input),
    });
  }

  async queryAudit(filters?: {
    request_id?: string;
    event_type?: string;
    limit?: number;
    offset?: number;
  }): Promise<{ events: AuditEvent[] }> {
    const params = new URLSearchParams();
    if (filters) {
      for (const [key, value] of Object.entries(filters)) {
        if (value !== undefined) params.set(key, String(value));
      }
    }
    const qs = params.toString();
    return this.fetch(`/v1/audit${qs ? `?${qs}` : ""}`);
  }

  /**
   * Submit a CR and poll until it reaches a terminal state.
   * Returns the final CR with response_data populated.
   */
  async coordinate(
    input: CreateCRInput,
    options?: { pollIntervalMs?: number; maxWaitMs?: number }
  ): Promise<CoordinationRequest> {
    const cr = await this.submit(input);
    const pollInterval = options?.pollIntervalMs ?? 2000;
    const maxWait = options?.maxWaitMs ?? input.timeout_policy.timeout_seconds * 1000 + 5000;
    const deadline = Date.now() + maxWait;

    const TERMINAL_STATES = new Set([
      "DELIVERED",
      "TIMED_OUT",
      "CANCELLED",
    ]);

    while (Date.now() < deadline) {
      await new Promise((resolve) => setTimeout(resolve, pollInterval));
      const current = await this.getRequest(cr.request_id);
      if (TERMINAL_STATES.has(current.state)) {
        return current;
      }
    }

    throw new Error(
      `CR ${cr.request_id} did not reach terminal state within ${maxWait}ms`
    );
  }
}
