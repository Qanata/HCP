# HCP — Human Coordination Plane

HCP sits between autonomous agents and the people who supervise them. An agent submits a
coordination request — an approval, a clarification, a decision — and HCP posts it to
Slack, waits for a person to answer, and hands the answer back. Every step is recorded in
an audit log and written to an Obsidian vault.

It is a single native binary, `hcp`, that runs as a background service on a Mac. The same
binary is also an MCP server (`hcp mcp`) and a Claude Code hook (`hcp hook`) that holds
destructive commands until someone approves them. `HCP.app` is a desktop dashboard for it:
metrics, what is waiting, recent activity, and the health of Slack, Obsidian and Neon.

**Documentation:** [`crates/hcp/README.md`](crates/hcp/README.md) — setup, configuration,
Slack, the vault, connecting agents, API keys, and development.

## Repository layout

| Path | What it is |
|------|------------|
| `crates/hcp/` | The server, MCP server and hook (Rust) |
| `crates/hcp-desktop/` | `HCP.app`, the desktop dashboard (Tauri) |
| `deploy/macos/` | launchd service, installer and example configuration |
| `docs/agent-system-prompt.md` | Guidance for agents on when and how to use HCP |
| `HCP_PRD.docx.md` | Product requirements |
| `index.html`, `docs/` | The GitHub Pages site |
| `archive/typescript/` | The original TypeScript service. Archived: kept for reference, not maintained |

## Quick start

```bash
deploy/macos/install.sh            # the server
deploy/macos/install-desktop.sh    # the dashboard app
```

Then follow [Running it on the Mac](crates/hcp/README.md#running-it-on-the-mac). To put
Claude Code under HCP, register the MCP server and add the hook to your user settings; see
[Connecting agents](crates/hcp/README.md#connecting-agents):

```bash
claude mcp add --scope user hcp -- ~/.local/bin/hcp --env-file ~/.config/hcp/claude-code.env mcp
```

If something does not connect, see
[Troubleshooting](crates/hcp/README.md#troubleshooting).
