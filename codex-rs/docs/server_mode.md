# Server Mode (Localhost API)

This document proposes and describes a new “server mode” for Codex CLI that exposes a localhost API so other apps can connect and use the models configured in Codex.

Status: planned (implementation to follow in subsequent PRs)

## Goals

- Provide a simple, local API so tools can program against Codex without shelling out to the CLI.
- Reuse Codex configuration (models, providers, auth) and behavior.
- Default to safe, localhost‑only, bearer‑token‑protected access.
- Support streaming responses for interactive UIs.

## Non‑Goals (initial version)

- Exposing Codex beyond localhost by default.
- Full parity with every OpenAI endpoint. We will start small and iterate.

## Transports

- HTTP/JSON with optional Server‑Sent Events (SSE) for streaming.
- Optional MCP over WebSocket endpoint for MCP‑aware clients (planned).

## CLI

A new subcommand runs the server:

```
codex serve [--host 127.0.0.1] [--port 8765] [--token <SECRET>] [--cors-origin <ORIGIN>...]
```

Flags (subject to refinement during implementation):

- `--host`: Interface to bind; defaults to `127.0.0.1`.
- `--port`/`-p`: Port; defaults to `8765`.
- `--token`: Static bearer token for simple auth. If omitted, the server refuses external requests unless `--no-auth` is provided for local dev.
- `--no-auth`: Disable auth for quick local experiments (localhost only).
- `--cors-origin`: Allowlist of origins for CORS (repeatable).
- `--api`: Which APIs to enable: `openai`, `mcp`, or `both` (default: `openai`).
- Inherits all config flags (`-c/--config`, `--profile`, etc.) so server side uses the intended model/provider configuration.

## Endpoints (HTTP)

Base path: `/v1`.

- `GET /healthz`: Liveness probe; returns `200 OK` with `{ "status": "ok" }`.
- `GET /v1/models`: List models configured for the active provider/profile. Shape mirrors OpenAI’s `List models` minimally: `{ "data": [{ "id": "<model>" }] }`.
- `POST /v1/chat/completions`: Minimal OpenAI‑compatible chat completions surface.
  - Request: `{ model?: string, messages: [...], temperature?, max_tokens?, stream? }`
  - Response (non‑stream): `{ id, created, model, object: "chat.completion", choices: [...] }`
  - Response (stream): `text/event-stream` with `data:` lines. Ends with `data: [DONE]`.

- `POST /v1/responses`: OpenAI Responses API proxy (for ChatGPT login or OpenAI Responses).
  - Forwards your JSON payload and headers needed by Responses.
  - Add `"stream": true` for streaming SSE.

Notes:
- The server adapts requests to Codex’s configured provider (OpenAI, Azure‑OpenAI, OSS/Ollama, etc.). Unsupported parameters are ignored.
- Use `/v1/responses` when your provider is configured for the Responses API (e.g., ChatGPT login). Use `/v1/chat/completions` when using a Chat‑Completions provider.
- The server does not run Codex’s “agent with tools” loop; it focuses on raw model completions. A future `/v1/codex/complete` endpoint will expose “agentic” behavior.

## MCP over WebSocket (planned)

For MCP clients, the server exposes a WebSocket endpoint at `/mcp` that upgrades to a JSON‑RPC message stream compatible with `codex mcp`. This provides a network transport alternative to stdio for MCP development tools.

## Authentication

- Default: Bearer token via `Authorization: Bearer <TOKEN>`.
  - Set via `--token` or `server.auth_token` in `config.toml`.
- `--no-auth` is allowed only when binding to `127.0.0.1`; binding to any non‑loopback address without a token will be rejected.
- No multi‑user session management in the initial version.

## CORS

- Disabled by default.
- Opt‑in allowlist via `--cors-origin https://example.app` (repeatable) or `server.cors_origins` array in `config.toml`.

## Configuration

Proposed `config.toml` section:

```toml
[server]
# Defaults shown
bind_address = "127.0.0.1"
port = 8765
# One of: "openai", "mcp", "both"
api = "openai"
# Optional: if unset, auth is required unless `no_auth = true` and bind_address is loopback
auth_token = ""
no_auth = false
cors_origins = []
```

All existing configuration (e.g., `model`, `model_provider`, provider overrides) applies to the server as it does to CLI modes.

## Quick Start (once implemented)

- Start the server:

  ```sh
  codex serve --port 8765 --token "$CODEX_SERVER_TOKEN"
  ```

- Call the API with curl:

  ```sh
  curl -N \
    -H "Authorization: Bearer $CODEX_SERVER_TOKEN" \
    -H "Content-Type: application/json" \
    -d '{"model":"gpt-4o-mini","messages":[{"role":"user","content":"Hello!"}],"stream":true}' \
    http://127.0.0.1:8765/v1/chat/completions
  ```

- Use with an OpenAI SDK by overriding the base URL (example in JavaScript):

  ```js
  import OpenAI from "openai";
  const client = new OpenAI({ apiKey: process.env.CODEX_SERVER_TOKEN, baseURL: "http://127.0.0.1:8765/v1" });
  const res = await client.chat.completions.create({
    model: "gpt-4o-mini",
    messages: [{ role: "user", content: "Hello!" }],
    stream: false,
  });
  console.log(res.choices[0].message.content);
  ```

## Security Considerations

- The server binds to loopback by default and should not be exposed publicly.
- Always set a strong token if other local processes you don’t control run on the same machine.
- No persistent per‑request logging of payloads beyond standard diagnostics unless configured by the user.

## Implementation Notes (high‑level)

- Crate: `codex-server` using `axum`/`hyper` and `tokio`.
- Subcommand: `codex serve` in the `cli` crate, reusing `CliConfigOverrides`.
- Request handling:
  - Translate OpenAI‑style requests into Codex `Prompt` and pipe to the existing streaming pipeline.
  - Implement SSE using `axum::response::Sse` with backpressure.
- MCP WS: Reuse the existing MCP message processor; map frames to/from WebSocket messages.
- Testing:
  - Unit tests for request parsing/validation and SSE chunking.
  - Integration tests using a mock provider (similar to existing core tests).

We’ll iterate from this minimal viable surface as use cases evolve.
