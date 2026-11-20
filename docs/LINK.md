# Connector v1 native link

The native client opens `/link?name=<id>` with an Authorization Bearer header
and WebSocket subprotocol `sayuri-connector-v1`. The Worker selects that exact
subprotocol and rejects missing/old protocols before charging an admission.
Names contain 1–64 ASCII letters, digits, underscores, or hyphens. The native
key authorizes `connector:client:<id>` by literal prefix. It never supplies the
name. The native CLI takes a complete `--url`, with the token separate.

## Messages

Each frame contains one complete UTF-8 JSON object. On connecting, the native
client sends a single ready message without waiting for a request:

```json
{"type":"ready","system":"linux","shell":"bash","desktop":{"available":true,"backend":"x11-xdotool"},"notes":"/home/alice/.connector/NOTES.md"}
```

`notes` can be null and contains no file content. `desktop.available` reports
input capability; a screenshot might still be possible when that flag is false.
The gateway waits up to 15 seconds for readiness before closing the link.

The gateway then sends requests and, on interruption, optional cancellation:

```json
{"type":"call","id":"gateway-generated-uuid","op":"exec","args":{"command":"pwd","cwd":null,"stdin":"","timeout":60}}
{"type":"cancel","id":"gateway-generated-uuid"}
```

The native client responds once with a matching ID and `result` or `error`:

```json
{"type":"result","id":"gateway-generated-uuid","result":{"output":"/home/alice\n","exit_code":0}}
```

- `exec` runs a fresh native shell. Nonzero exit codes are normal results.
- `desktop` takes `{actions: [...]}`. An empty list captures only. A batch returns
  `completed`, optional errors, and an `image` with `data` (base64 PNG/JPEG),
  `mime_type`, `width`, and `height`, or null. Errors can accompany a usable
  image. Actions match the desktop action definitions in `src/computer.rs`.

Gateway UUIDs are only for correlating in-flight work on that socket. A reply
with another socket's ID cannot complete it. There are no persistent request
records, controller sessions, retries, or automatic replay. The native shell
applies its own deadline. Cancelling or disconnecting does not undo executed
work; inspect state before repeating an interrupted operation.

The link uses native Ping/Pong (15-second ping and 45-second liveness deadline),
not application heartbeats. At most 16 simultaneous calls are accepted, with
24 MiB per JSON message, 8 MiB retained shell output, and 8 MiB per screenshot.
A result too large to serialize is replaced with a small error, not truncated.
