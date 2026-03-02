# AskHuman

A multi-platform desktop application that exposes an [MCP (Model Context Protocol)](https://modelcontextprotocol.io) tool called **`ask_human`**. AI agents can call this tool to display a question in the desktop window and wait for the human operator to type a reply.

## Features

- **MCP Streamable HTTP transport** (spec version `2025-03-26`) — multiple agent sessions can connect simultaneously
- **Desktop UI** built with [iced](https://iced.rs/) (a non-HTML Rust GUI library)
- Each question appears as an independent **card** — multiple questions from different agents are shown at the same time
- **Close button** (✕) on every card to reject a question
- **Error tip** shown in red above the answer box if submission fails; allows the user to retry
- **Confirmation animation** — on successful submission the card briefly shows a green ✓ message, then auto-removes after 1.5 seconds
- Status bar always shows the MCP endpoint URL

## Building

```bash
cargo build --release
```

Requires a C linker and the usual X11/Wayland development headers on Linux.

## Running

```bash
./target/release/ask-human
# Optional: override the port (default 3000)
ASKHUMAN_PORT=8080 ./target/release/ask-human
```

The app prints the MCP endpoint URL to stdout:

```
AskHuman MCP server  →  http://127.0.0.1:3000/mcp
```

## Configuring an MCP Client

Point your MCP client at the Streamable HTTP endpoint:

| Setting | Value |
|---------|-------|
| Transport | Streamable HTTP (`2025-03-26`) |
| URL | `http://127.0.0.1:3000/mcp` |

### Tool: `ask_human`

| Parameter | Type | Required | Description |
|-----------|------|----------|-------------|
| `question` | string | ✓ | The question to display to the human |
| `context`  | string |   | Optional extra context shown below the question |

**Returns:** The human's answer as a text string, or an error if they declined.

### Example call (JSON-RPC over HTTP)

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "tools/call",
  "params": {
    "name": "ask_human",
    "arguments": {
      "question": "Should I proceed with the deployment?",
      "context": "The staging tests all passed. Production traffic is ~10k req/s."
    }
  }
}
```

## Architecture

```
┌─────────────────────────────────────┐
│  ask-human process                  │
│                                     │
│  ┌──────────────┐  tokio mpsc chan  │
│  │  MCP HTTP    │ ──────────────►  │
│  │  server      │                  │
│  │  (axum)      │ ◄────────────── │
│  │  POST /mcp   │  oneshot reply   │
│  └──────────────┘                  │
│         ▲  background thread        │
│         │                           │
│  ┌──────────────┐                  │
│  │  iced UI     │  main thread     │
│  │  (desktop)   │                  │
│  └──────────────┘                  │
└─────────────────────────────────────┘
```

- The **MCP server** runs on a dedicated Tokio runtime in a background thread.
- The **iced UI** runs on the main thread (required on macOS/Windows).
- Questions flow through a `tokio::sync::mpsc` channel from server → UI.
- Answers flow back through per-question `tokio::sync::oneshot` channels.
