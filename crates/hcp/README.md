# HCP server (Rust)

The Rust implementation of the Human Coordination Plane. Agents submit coordination
requests over HTTP, humans answer them in Slack, and every step is logged to an Obsidian
vault. It is a single native binary (`hcp`) intended to run as a background service on a
Mac.

Everything the TypeScript service did is ported except the web portal, which Slack
replaces. The TypeScript code in the repository root is kept for reference.

| Piece | Status |
|-------|--------|
| REST API (`/v1/requests`, `/v1/audit`, `/v1/events`, `/health`) | Ported, wire-compatible for agents |
| State machine, timeouts, fallbacks, escalation | Ported, with fixes (see [Differences](#differences-from-the-typescript-service)) |
| Slack approvals | Ported and extended: Socket Mode, signed HTTP, modals for free-text answers |
| Logs in an Obsidian vault | New |
| API key management (`hcp keys …`) | Ported (replaces `setup-key` and `provision-agents`) |
| MCP server (`hcp mcp`) | Ported, on the official `rmcp` SDK |
| Claude Code hook (`hcp hook`) | Ported, with fixes (see [The Claude Code hook](#the-claude-code-hook)) |
| macOS service (launchd) | New, in `deploy/macos/` |
| Web portal | Dropped. Humans respond in Slack |
| Rate limiting, CORS | Not ported. The server binds to loopback by default and has no browser client |

## Running it on the Mac

Requirements: a Rust toolchain (1.94 or newer), a Postgres database (the existing Neon
database works as-is), an Obsidian vault, and a Slack app.

```bash
deploy/macos/install.sh
```

The first run builds the binary, installs it to `~/.local/bin/hcp`, and creates
`~/.config/hcp/hcp.env` from the example. Fill that file in, create a key for each agent,
then run the script again to start the service:

```bash
~/.local/bin/hcp --env-file ~/.config/hcp/hcp.env keys create claude-code --label "Claude Code"
```

```bash
deploy/macos/install.sh
```

```bash
curl http://127.0.0.1:3100/health
```

The service starts at login and is restarted by launchd if it exits. Output from before
the vault is open (a bad config, a missing vault) goes to `~/Library/Logs/hcp/stderr.log`.

Two things to know about macOS:

- A LaunchAgent only runs while you are logged in. For a headless machine, enable
  automatic login.
- If the vault lives in iCloud Drive, `~/Documents` or `~/Desktop`, macOS privacy
  controls can deny a background process access to it. If `stderr.log` shows
  `Operation not permitted`, grant `~/.local/bin/hcp` Full Disk Access in System Settings
  → Privacy & Security, or keep the vault somewhere else.

To run it in the foreground instead: `hcp --env-file <file> serve` (or put the settings
in `./.env`).

## Configuration

Read from the environment, or from an env file (`--env-file`, default `./.env`).
Variables already set in the environment win over the file.

| Variable | Default | Meaning |
|----------|---------|---------|
| `DATABASE_URL` | required | Postgres connection string. TLS follows `sslmode`; use `verify-full` for Neon |
| `HCP_OBSIDIAN_VAULT` | required | The vault folder. Must exist; `~/` is expanded |
| `HCP_OBSIDIAN_FOLDER` | `HCP` | Folder inside the vault that HCP writes to |
| `SLACK_BOT_TOKEN` | unset | Bot token (`xoxb-…`, scope `chat:write`). Unset: no Slack; requests can only be answered through the API |
| `SLACK_APP_TOKEN` | unset | App-level token (`xapp-…`, scope `connections:write`). Enables Socket Mode |
| `SLACK_SIGNING_SECRET` | unset | Enables `POST /slack/interactions` with signature verification |
| `HCP_SLACK_CHANNEL` | required with Slack | Channel ID that receives every request needing a human |
| `HCP_SLACK_NOTIFY_CHANNEL` | unset | Channel for `NOTIFICATION` requests. Unset: they are only logged |
| `HCP_SLACK_RESPONDERS` | unset | `responder_id=SlackUserID,…`. A mapped responder is @-mentioned and is the only one who can answer |
| `HCP_BIND` | `127.0.0.1` | Listen address |
| `HCP_PORT` | `3100` | Listen port |
| `HCP_TIMEOUT_POLL_SECONDS` | `10` | How often timeouts and undelivered requests are checked |
| `HCP_DB_MAX_CONNECTIONS` | `10` | Connection pool size |
| `HCP_LOG` | `info,sqlx=warn` | Console verbosity ([filter syntax](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html)) |

With Slack enabled, at least one of `SLACK_APP_TOKEN` and `SLACK_SIGNING_SECRET` is
required; without either, buttons would have nowhere to report to.

## Slack setup

Socket Mode is the right choice for a machine without a public URL: the server opens an
outbound WebSocket to Slack and interactions arrive over it.

1. Create an app at <https://api.slack.com/apps>.
2. **Socket Mode**: enable it and create an app-level token with the `connections:write`
   scope. That is `SLACK_APP_TOKEN`.
3. **OAuth & Permissions**: add the bot scope `chat:write`, install the app, and copy the
   bot token. That is `SLACK_BOT_TOKEN`.
4. **Interactivity & Shortcuts**: turn interactivity on. No request URL is needed in
   Socket Mode.
5. Invite the bot to the approvals channel and put the channel ID in `HCP_SLACK_CHANNEL`.

What people see:

- **Approvals** get Approve and Reject buttons.
- **Clarification, decision, input, review and escalation** requests get a Respond button
  that opens a text box; the answer reaches the agent as `{"text": "…"}`.
- **Notifications** have no controls, and are only posted if a channel is named for them.
- Once a request is answered, cancelled, escalated or timed out, its message is rewritten
  to say so and the buttons are removed.
- The message says what happens at the deadline. An `AUTO_APPROVE` fallback is called out
  in bold, because silence then means yes.

Routing: anything that needs a human goes to `routing_hints.slack_channel_id` if the
request names one, otherwise to `HCP_SLACK_CHANNEL`. A request asking for the old
`portal` channel is routed to Slack as well.

## What lands in the vault

Everything is under one folder (`HCP/` by default):

```
HCP/
  Requests/2026-11/01M7ADF336CWQ1VMM5SQMFZJ3Y.md   one note per request
  Log/HCP Log 2026-11-13.md                        audit log, one line per event
  Service/HCP Service 2026-11-13.md                the server's own log
```

A **request note** is regenerated from the database whenever the request changes. It has
properties you can query (`state`, `intent`, `urgency`, `agent`, `responder`, `decision`,
`deadline`, …), tags (`#hcp/approval`, `#hcp/state/delivered`), the context the agent
sent, the response, and the full audit trail. Do not edit these notes; edits are
overwritten.

The **audit log** is append-only. Each line links to its request note and ends in a block
ID, so a specific event can be linked from other notes:

```markdown
- `12:17:38.406` `CR_SUBMITTED` · [[01M7ADF3…|APPROVAL: Deploy v2.1.0 to production]] · `claude-code` (AGENT) · `{"intent":"APPROVAL","urgency":"HIGH"}` ^01M7ADF3394JDHAB8DYKKH6YAR
- `12:17:38.433` `CR_RESPONDED` · [[01M7ADF3…|APPROVAL: Deploy v2.1.0 to production]] · `slack:U07ISAAC` (HUMAN) · `{"response_data":{"decision":"approved"},…}` ^01M7ADF341Q7S80EVNJVH9227R
```

The **service log** holds the server's own INFO, WARN and ERROR lines (start-up, Slack
connection, delivery failures).

Reliability: an audit event is queued for the vault in the same database transaction that
records it, and removed from the queue only after it has been written. If the vault is
unavailable (unmounted, permissions, the server crashed mid-write) the log falls behind
and catches up; entries are not lost and not duplicated. Postgres remains the system of
record and is what `/v1/audit` reads.

Safety: text supplied by agents is written as inert text (fenced `text` blocks, code
spans, or escaped). A request cannot plant links, embeds, tags, properties or plugin code
blocks in your vault.

Not done yet: requests that existed before this server first ran are not back-filled into
the vault.

## Connecting agents

Agents talk to the server with an API key (`hcp keys create <agent>`). The same `hcp`
binary provides the two agent-side integrations. Both read these settings from the
environment, or from a file named with `--env-file`:

| Variable | Default | Meaning |
|----------|---------|---------|
| `HCP_API_KEY` | required | The agent's key |
| `HCP_BASE_URL` | `http://127.0.0.1:3100` | Where the server is |
| `HCP_RESPONDER` | `default` | Who requests are addressed to when the agent does not say |
| `HCP_SLACK_CHANNEL` | unset | Post this agent's requests, notifications included, in this Slack channel instead of the server's default |
| `HCP_POLL_INTERVAL_MS` | `2000` | How often a waiting call checks for the answer |

Neither command reads `./.env`. They run inside whatever project the agent is working on,
and a file there could be written by the agent being supervised. Keep the key in a file
outside the project, readable only by you, and name it explicitly:

```bash
printf 'HCP_API_KEY=hcp_...\nHCP_RESPONDER=isaac\n' > ~/.config/hcp/claude-code.env && chmod 600 ~/.config/hcp/claude-code.env
```

### The MCP server

`hcp mcp` is an MCP server over stdio for any harness that supports MCP:

```json
{
  "mcpServers": {
    "hcp": {
      "command": "/Users/you/.local/bin/hcp",
      "args": ["--env-file", "/Users/you/.config/hcp/hermes.env", "mcp"]
    }
  }
}
```

| Tool | What it does |
|------|--------------|
| `hcp_coordinate` | Asks a human and blocks until they answer in Slack. The main tool |
| `hcp_notify` | One-way notification; returns at once |
| `hcp_submit` / `hcp_poll` / `hcp_cancel` | Ask without blocking, check later, withdraw |

`routing_hints` is optional: without it a request goes to `HCP_RESPONDER` in the default
channel.

It speaks every MCP revision from 2024-11-05 through 2026-07-28, including the stateless
protocol that revision introduced. Compared with the Node server it replaces:

- A blocked `hcp_coordinate` no longer stalls other calls on the same connection.
- While waiting it sends progress notifications, when the client supplies a progress
  token, so clients with an idle timeout do not give up on a long wait (Claude Code
  aborts a silent stdio tool call after 30 minutes).
- If the client cancels the call or disconnects, the question is withdrawn and its Slack
  message retired, rather than left for someone to answer into the void.
- It waits until the server's deadline, so an escalation that extends the deadline
  extends the wait.

### The Claude Code hook

`hcp hook` sorts every Bash command into one of three tiers:

| Tier | Examples | What happens |
|------|----------|--------------|
| Safe | `ls`, `git status`, `cat x \| grep y` | Passes silently, without contacting the server |
| Notify | `cargo build`, `npm install`, file edits | A notification is logged; nothing waits |
| Gate | `rm -rf`, `git push`, `terraform apply`, `DROP TABLE` | Blocked until a human approves it in Slack |

Register it in `~/.claude/settings.json` (your user settings, not the project's):

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "",
        "hooks": [
          {
            "type": "command",
            "command": "/Users/you/.local/bin/hcp --env-file /Users/you/.config/hcp/claude-code.env hook",
            "timeout": 400,
            "onFailure": "block"
          }
        ]
      }
    ],
    "PostToolUse": [
      { "matcher": "", "hooks": [{ "type": "command", "command": "/Users/you/.local/bin/hcp --env-file /Users/you/.config/hcp/claude-code.env hook" }] }
    ],
    "Notification": [
      { "matcher": "", "hooks": [{ "type": "command", "command": "/Users/you/.local/bin/hcp --env-file /Users/you/.config/hcp/claude-code.env hook" }] }
    ],
    "Stop": [
      { "hooks": [{ "type": "command", "command": "/Users/you/.local/bin/hcp --env-file /Users/you/.config/hcp/claude-code.env hook" }] }
    ]
  }
}
```

The two extra fields on `PreToolUse` matter. Claude Code lets a tool call proceed when a
hook crashes, cannot start, or times out; `"onFailure": "block"` (Claude Code 2.1.295 or
later) closes that gap. `timeout` is in seconds and must be longer than
`HCP_HOOK_TIMEOUT`, or Claude Code gives up on the hook before the human has had their
time. Only the `PreToolUse` entry is needed if you just want the gate.

| Variable | Default | Meaning |
|----------|---------|---------|
| `HCP_HOOK_TIMEOUT` | `300` | Seconds a human has to answer; after that the command is rejected |
| `HCP_HOOK_MODE` | `notify_and_gate` | `gate_only`: no notifications. `gate_all`: gate every Bash command that is not read-only |
| `HCP_HOOK_ON_ERROR` | `block` | What happens to a gated command when approval cannot be requested at all. `allow` fails open |

A gated command runs only if a person approved it. A rejection, a timeout, or losing
contact with the server while waiting all block it, and Claude is told why. What changed
from the Node hook:

| | Node hook | `hcp hook` |
|-|-----------|------------|
| How it blocks | Exit code 1 with a JSON form Claude Code now documents as deprecated | Exit code 2 with the reason on stderr, the documented way |
| Server unreachable or key missing | Command allowed | Command blocked, unless `HCP_HOOK_ON_ERROR=allow` |
| `echo ok && rm -rf build` | Passed silently: a read-only prefix hid the rest | Gated: destructive patterns are checked first, and a compound command is read-only only if every part is |
| `echo x > file`, `find . -delete`, `env X=1 ./deploy.sh` | Treated as read-only | Not read-only; `find -delete` is gated |
| `rm -Rf`, `git -C dir push`, `kubectl -n prod delete`, `aws s3 rm` | Not gated | Gated |
| macOS equivalents (`diskutil erase…`, `launchctl bootout`, `> /dev/disk2`) | Not covered | Gated |
| Result and stop details | Read from fields that do not exist | Read from `tool_response` and `last_assistant_message` |
| Context for the reviewer | Command only | Also the session ID and working directory |

The pattern lists are in `src/hook.rs`. They are a tripwire for common destructive
commands, not a sandbox: a determined agent can find a command they do not match.

## API keys and who can do what

```bash
hcp keys create <agent_id> [--label <text>] [--scope respond] [--scope admin]
hcp keys list
hcp keys revoke <key_id>
```

Keys created by the TypeScript service keep working (same hashing, same table).

| Key | Can |
|-----|-----|
| No scopes (an agent) | Submit requests; read, list, cancel and collect answers for its own requests; read its own audit events and event stream |
| `respond` | Also answer requests through `POST /v1/requests/{id}/respond` |
| `admin` | Also read every agent's requests, audit events and event stream |

## Differences from the TypeScript service

Agents do not need to change. These behaviours are deliberately different:

| Area | TypeScript | Rust | Why |
|------|-----------|------|-----|
| Answering through the API | Any key, including the requesting agent's | Needs the `respond` scope | An agent could approve its own request |
| Visibility | Any key could read, cancel or collect any request | Agents see only their own (`admin` sees all) | Agents could read and cancel each other's requests |
| Slack interactions | Unauthenticated HTTP endpoint | Socket Mode, or HTTP with signature and timestamp checks | Anyone who could reach the endpoint could approve anything |
| Slack delivery failure | Request became pending anyway | Stays in `ROUTING`, retried every 30s, times out at the deadline | An `AUTO_APPROVE` fallback could approve a request nobody was shown |
| `ESCALATE` fallback | Re-escalated to the same person forever | Escalates once, then times out | The request never ended |
| Escalation notice | None | New Slack message; the old one is retired | The escalation responder was never told |
| Routing | Two transitions fired without waiting | Sequential, and resumed after a crash | Requests could stick in `SUBMITTED` |
| State change + audit event | Two separate writes | One transaction | A crash between them lost the audit record |
| Idempotency key reuse by another agent | Returned the first agent's request | `409 Conflict` | Leaked requests across agents |
| `/v1/events` | Open; filters ignored | Requires a key; filtered to the caller | Anyone could watch all activity |
| Default `routing_hints.channel` | `portal` | `slack` | There is no portal |
| Listen address | `0.0.0.0` | `127.0.0.1` | Not exposed to the network unless asked |
| Database TLS | Certificate never verified | Follows `sslmode` in the URL | |
| Agent text in Slack | Posted as-is | Escaped | A request could `@channel` or disguise links |

One state-machine edge was added: `ROUTING → TIMED_OUT`, for a request that could never be
delivered. And a request past its deadline is settled when its agent reads it, rather
than only on the next scheduler pass, so a waiting agent gets the outcome on its first
poll after the deadline.

Both servers can read the same database, but only this one feeds the vault, so do not run
them side by side if the vault log is meant to be complete.

## Development

```bash
cargo test
```

The tests need no setup. End-to-end tests start the real server against a throwaway
Postgres (downloaded once into `target/tmp/`, about 270 MB), a mock Slack API and Socket
Mode server, and a temporary vault. `tests/cli.rs` and `tests/agents.rs` run the actual
binary, the latter as an MCP server and as a hook. `tests/node_clients.rs` drives the
original Node hook and MCP server against this server and is skipped when Node is not
installed.

Slack is mocked in every test: the Slack integration has not yet been exercised against
the real Slack API. Likewise the hook is tested by feeding it the events Claude Code
documents, not from inside a live Claude Code session.

```bash
cargo clippy --all-targets -- -D warnings
```

```bash
cargo fmt --all
```

Layout:

```
src/
  domain.rs              request types and the state machine
  store.rs               all SQL (Postgres via sqlx)
  engine.rs              submit, route, answer, cancel, time out, escalate
  slack/mod.rs           messages, Web API client, request signatures
  slack/interactions.rs  button clicks and modal submissions
  slack/socket.rs        Socket Mode connection
  vault.rs               Obsidian notes, audit log, service log
  api.rs                 HTTP routes and authentication
  client.rs              HTTP client used by the agent-side commands
  mcp.rs                 `hcp mcp`: the MCP tools
  hook.rs                `hcp hook`: command tiers and the approval gate
  app.rs                 wiring and lifecycle
  logging.rs             console and vault log setup
  config.rs              environment configuration
  main.rs                CLI
```
