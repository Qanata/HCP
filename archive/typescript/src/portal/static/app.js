function hcpPortal() {
  const params = new URLSearchParams(window.location.search);
  const responderId = params.get("responder_id") || "default";
  const focusRequestId = params.get("request_id");

  return {
    responderId,
    requests: [],
    loading: true,
    connected: false,
    eventSource: null,

    async init() {
      await this.fetchRequests();
      this.connectSSE();
    },

    async fetchRequests() {
      try {
        const url = `/v1/requests?responder_id=${encodeURIComponent(this.responderId)}&state=PENDING_RESPONSE`;
        const res = await fetch(url);
        if (res.ok) {
          const data = await res.json();
          this.requests = data.requests || [];
          // If a specific request_id was given, also fetch completed ones
          if (focusRequestId) {
            const singleRes = await fetch(`/v1/requests/${focusRequestId}`);
            if (singleRes.ok) {
              const cr = await singleRes.json();
              if (!this.requests.find((r) => r.request_id === cr.request_id)) {
                this.requests.unshift(cr);
              }
            }
          }
        }
      } catch (err) {
        console.error("Failed to fetch requests:", err);
      } finally {
        this.loading = false;
      }
    },

    connectSSE() {
      const url = `/v1/events?responder_id=${encodeURIComponent(this.responderId)}`;
      this.eventSource = new EventSource(url);

      this.eventSource.addEventListener("connected", () => {
        this.connected = true;
      });

      this.eventSource.addEventListener("state_change", (event) => {
        const data = JSON.parse(event.data);
        this.fetchRequests();
      });

      this.eventSource.onerror = () => {
        this.connected = false;
        // Reconnect after delay
        setTimeout(() => this.connectSSE(), 5000);
      };
    },

    async respond(requestId, responseData) {
      try {
        const res = await fetch(`/v1/requests/${requestId}/respond`, {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({
            response_data: responseData,
            responded_by: this.responderId,
          }),
        });

        if (res.ok) {
          await this.fetchRequests();
        } else {
          const err = await res.json();
          alert(`Error: ${err.error || "Unknown error"}`);
        }
      } catch (err) {
        alert(`Network error: ${err.message}`);
      }
    },

    async respondText(requestId, text) {
      if (!text.trim()) return;
      await this.respond(requestId, { text: text.trim() });
    },

    formatTime(iso) {
      if (!iso) return "N/A";
      return new Date(iso).toLocaleString();
    },
  };
}
