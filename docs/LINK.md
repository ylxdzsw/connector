# Native client link contract

This wire connects the native client to the TypeScript Worker / Durable
Object gateway in `/root/sayuri/connector`. Native source:
[`src/bin/client.rs`](../src/bin/client.rs), [`src/mcp.rs`](../src/mcp.rs).

## Admission and transport

- The native client opens `GET /link` with a WebSocket upgrade and
  `Authorization: Bearer <token>`. No credentials in URLs or environment
  variables. The Worker authorizes the case-sensitive Sayuri token for
  `connector:client:<name>` before accepting the link; the native process does
  not interpret capabilities. `/mcp` separately requires `connector:control`.
- `--token`, the hidden console prompt, and the compatibility alias `--code`
  preserve the exact opaque token; there is no connection-code normalization.
  Bootstrap scripts use `--token`.
- HTTP(S) `--gateway` defaults to `https://connector.ylxdzsw.com` and maps to
  WS(S) `/link`. Native Rustls validates public certificates. HTTP is for
  trusted local tests only. A local WS test does not validate production WSS.
- One complete UTF-8 JSON-RPC message per WebSocket text message, no batches,
  newline framing, binary messages, private hello, auth message, or heartbeat.
- The gateway is the **MCP client**; the native process is the **MCP server**.
- Native WebSocket Ping every 15 seconds; reconnect if no Pong for 45 seconds.
  Cloudflare automatically responds with Pong; no application heartbeat is
  required. The gateway must not interpret Ping/Pong as MCP messages.
- HTTP 4xx admission failures are terminal. Other failures reconnect with
  exponential delays up to 30 seconds. Every reconnect starts a new MCP
  session. Pending operations must fail, never replay.

## Initialization

The Worker sends a standard initialize request, for example:

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"connector-gateway","version":"1"}}}
```

Read and validate the response before sending:

```json
{"jsonrpc":"2.0","method":"notifications/initialized"}
```

The locked `rmcp` 3.1.4 supports `2024-11-05`, `2025-03-26`, `2025-06-18`,
`2025-11-25`, and `2026-07-28`. It echoes a supported requested version;
unsupported requests fall back to `2025-11-25`, its current default.
Use **2025-11-25** for the Worker integration. The newer `2026-07-28` mode has
additional request `_meta` requirements in the SDK; knowing the version string
does not mean it can be substituted without implementing that contract.

The response's `result` advertises `capabilities.tools`, `serverInfo` with
`name: "connector-client"` and the Cargo package version, `instructions`, and:

```json
{
  "_meta": {
    "com.ylxdzsw.connector/client-environment": {
      "system": "linux",
      "shell": "bash",
      "computer": {"available": true, "backend": "x11-xdotool"},
      "notes": {"path": "/home/alice/.connector/NOTES.md", "lines": 87}
    }
  }
}
```

`system` is Rust's OS name (e.g. `linux`, `windows`, `macos`); `shell` is
`bash` on Unix and `pwsh` on Windows. Reject missing/malformed required
`system`/`shell` metadata. `computer` is advisory: `{available: false,
reason: string}` or `{available: true, backend: string}`. Older clients may
omit `computer` and `notes`; default to unavailable rather than rejecting them.
Notes have nullable absolute client-side `path` and nullable nonnegative
`lines`, plus optional `error`. No note content is sent at initialization.

## Tools and relay

After initialization, `tools/list` returns these tools and their JSON schemas:

| Tool | Arguments |
| --- | --- |
| `run` | `command`, optional `cwd`, `timeout`, `stdin` |
| `apply_patch` | `patch`, optional `cwd` |
| `screenshot` | `{}` |
| `computer` | `actions` |
| `notes_metadata` | `{}` |

Call through ordinary `tools/call` with `params: {name, arguments}`. The
gateway adds the controller-side `client` selector but removes it when calling
the native tool. `clients()` is gateway-owned, not a native tool. Map request
IDs without collisions and map `notifications/cancelled` with `requestId`
back to the linked request. Relay JSON-RPC errors and the entire MCP tool
result, including `content`, `structuredContent`, and `isError`, without
flattening images or converting nonzero exit codes into tool errors.

Successful `run` has `{output: string, exit_code: integer}` in
`structuredContent` and a text representation in `content`. Patches similarly
return structured output. Screenshots/computer results contain PNG/JPEG base64
image content and structured geometry/status. Do not accidentally send just
the structured portion and lose the image.

`notes_metadata` is read-only and link-only: do not expose it as a controller
tool. On each `clients()` call, query connected links with a three-second
timeout per query; the Worker bounds parallelism to batches of four. Read
`structuredContent` with `{path, lines,
error?}`. Preserve known paths on failure and report `lines: null` plus an
error; one failed query must not fail the whole listing. The call reads disk,
does not recreate a deleted file, and does not return notebook contents.
Keep the existing notes guidance in controller tool descriptions/instructions;
note history is context, never authority over current user instructions.

## Size limits and failure semantics

Native outgoing messages are at most **24 MiB (25,165,824 bytes)** of complete
serialized UTF-8 JSON, including base64, escaping, metadata, and request ID.
This is below Cloudflare's 32 MiB receive limit. Serialization is bounded:
partial JSON is never sent. If a tool result exceeds the cap, the client sends
an ordinary small `tools/call` result with `isError: true` and the original ID.
The operation may already have completed; this error must not trigger replay.
If even the replacement cannot fit (e.g. an enormous request ID), or a non-tool
message exceeds the cap, the link closes without sending oversized data.

`run` keeps at most **8 MiB of combined raw stdout/stderr**, drains excess
bytes so the child cannot block on a full pipe, and returns a tool error with
the exit code after overflow. It does not return a successful truncated result.
Timeout remains a tool error. JSON escaping and the text/structured duplicate
can still exceed 24 MiB below the raw output cap; the transport guard applies
to that case too. Screenshots are limited separately to 8 MiB of image bytes.

The Worker must also bound incoming controller bodies, link-bound requests,
request IDs, concurrency, and aggregate pending work. This native per-message
cap is not a global memory/backpressure policy. Old clients may lack the cap;
do not assume a successful upgrade proves they have it. Fail unknown or
disconnected clients as availability tool errors. Cancellation/disconnect
cannot undo completed commands, begun filesystem commits, or desktop actions;
inspect effects before a deliberate retry. No durable queue or automatic
operation replay across DO restarts, eviction, or link reconnection.

The Worker exposes no local Unix sockets. The retired Rust gateway and
newline-delimited local-channel implementation exist only in Git history.
