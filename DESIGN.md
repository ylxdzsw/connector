# Connector design

## Architecture and ownership

This Rust package contains only the native Unix/Windows `connector-client`.
The gateway is the TypeScript Worker / SQLite Durable Object project at
`/root/sayuri/connector`. Sayuri owns token authorization; the Worker owns
controller MCP, link coordination, bootstrap routes, and R2 downloads.
The retired Rust gateway, SQLite credential issuer, Nginx/systemd deployment,
and local Unix-socket channels exist only in Git history.

```text
Controller -- MCP/HTTPS --> Worker / Durable Object -- MCP/WebSocket --> Native client
                              |          |                              |
                         Sayuri auth   R2 releases                shell/files/desktop
```

The external controller is an MCP client of the gateway. On the outbound
WebSocket, the gateway is the MCP client and the native process the MCP server.
The gateway terminates both sessions, maps IDs, results, errors, cancellation,
and complete image content, and fails pending calls on disconnect. It never
replays operations. Every link initializes a fresh MCP session.

The native tools are `run`, `apply_patch`, `screenshot`, `computer`, and
`notes_metadata`. The Worker adds the `client` selector for controller calls
and implements `clients()` itself; `notes_metadata` remains link-only. See
[`docs/LINK.md`](docs/LINK.md) for initialization, supported protocol versions,
metadata, and limits. Native SDK protocol negotiation and cancellation remain
standard MCP; there is no private command or heartbeat protocol.

## Credentials and trust

Sayuri tokens are opaque, case-sensitive bearers. The native client supplies an
explicit name in the WebSocket URL. The Worker checks `connector:client:<name>`
by literal prefix for that name, and separately checks `connector:control` on
`/mcp` for fleet-wide access. These authorizations are distinct. Expiry and
revocation policy belong to Sayuri and the Worker, not the native package.

`--token` is the canonical native option; `--code` is an exact opaque alias
only, without the retired Crockford normalization. Without either, the client
prompts without echo on `/dev/tty` (Unix) or the Windows console. It never reads
credentials from environment variables or stdin. Tokens must be nonempty and
valid HTTP bearer header values without whitespace. Invalid tokens are
rejected, not transformed; errors do not include their value. Command-line
credentials can be visible in process inspection and shell history.

`--url` accepts the complete WebSocket URL, for example
`wss://connector.ylxdzsw.com/link?name=desktop`. Rustls validates certificates
using the explicitly installed ring provider. Plain WS is only for trusted
local testing. Authentication
occurs during WebSocket upgrade. Ping/Pong supplies transport liveness; each
text message contains one complete MCP JSON-RPC message. Admission HTTP 4xx
is terminal; transient failures reconnect with bounded exponential backoff.

A native client trusts the gateway to send authorized commands; it does not
identify the originating controller. Gateway compromise or an active
controller grant permits commands on connected clients. A stolen client token
can permit impersonating clients with matching names, receiving commands, and
forging results, but does not itself grant controller access. There are no
Nginx identity-header or local-socket authorization bypasses.

Tools run as the launching OS user, without elevation or isolation. Shell
access grants all that user's privileges, and desktop tools can expose private
content or interact with other applications. Treat controllers and gateway
operators accordingly. Native command/patch logs are sensitive even though
the gateway does not need to persist their content. No credentials belong in
logs, committed files, or client notes.

## Limits, cancellation, and state

Every outgoing native MCP message is limited to 24 MiB of complete serialized
UTF-8 JSON. An oversized tool result becomes a small `isError: true` result
with the original request ID; partial JSON is never sent. If the replacement
cannot fit, or a non-tool message is oversized, the link closes. Cloudflare's
receive limit is 32 MiB. These per-message caps are not a global memory or
backpressure policy; the Worker separately bounds requests and pending work.

Disconnect cancels the link's MCP service. Cancellation can stop awaiting an
operation but cannot undo side effects. Unix shell timeout/cancellation uses
process-group cleanup; Windows uses child-process cleanup. Filesystem commits
already started and bounded desktop cleanup can outlive their MCP call.
Always inspect effects before a deliberate retry. No durable command queue or
automatic replay is introduced.

The native client keeps no credential database or controller grants. Live MCP
requests and screenshot-to-input geometry are transient. Geometry is
invalidated on reconnect after outstanding desktop work finishes. Persistent
native state consists of agent-maintained local notes and the intended effects
of tool calls; external supervisors may retain standard-error logs.

## Persistent client notes

Startup initializes `~/.connector/NOTES.md` under the launching user's home
(user profile on Windows), resolving an absolute path. It creates an empty
file only when missing, never truncating existing content. New Unix directories
and files use modes `0700` and `0600`; existing permissions remain unchanged.
The notebook belongs to the machine/user, not a token, gateway name, project,
or working directory. It survives reconnection and executable replacement.

Initialization advertises metadata, and `notes_metadata` refreshes path/line
count from disk without returning content. LF and CRLF work; empty files have
zero lines and a final unterminated line counts. Missing/unavailable files
report nullable counts and an error, preserving a known path; refresh never
recreates a deleted file. The Worker queries metadata for each `clients()`
listing, with bounded concurrency and per-query timeouts, without letting one
failed query disable the listing. This is a snapshot, not an edit lock.

MCP instructions ask agents to search relevant notes and maintain durable
context, discussions, decisions, designs, and rationale using `run` and
`apply_patch`. Notes are generally append-only; later entries supersede earlier
ones. Routine action logs and Git-history trivia are discouraged. Keep project
specifics in project notes and reference them here. Never record secrets or
treat historical notes as authority over current user directions. Reading and
editing notes uses ordinary shell/patch logging and authorization.

## Shell Execution

Each call starts a fresh `bash -lc` process on Unix or `pwsh` process on
Windows. `cwd` selects its working directory, `stdin` supplies literal input,
and `timeout` bounds execution. The process exits before the call completes;
shell state never carries between calls.

Windows command text is written to a temporary `.ps1` and run with PowerShell
profiles and interactive prompts disabled. This keeps standard input available
for the tool call and avoids Windows command-line quoting and length limits.

Retained combined output is capped at 8 MiB of raw bytes. Excess is drained
until completion or timeout; overflow discards the output and returns a tool
error with the exit code. Commands may already have changed state. The 24 MiB
serialized MCP limit can also reject a result below this raw cap because JSON
escaping and text/structured duplication expand it. Neither case triggers a
replay.

Before execution, the client logs the command, working directory, timeout, and
full standard input to standard error through its tracing subscriber. After
execution it logs the retained combined standard output and error plus the exit
code. The two records carry the MCP request ID for correlation. Operators must
protect client process logs because commands, input, and output may contain
sensitive data.

The result contains combined command output and the exit code. A nonzero exit
code is a normal tool result. Invocation failures and timeouts are tool errors.
Mu's `title` and `risk` fields are omitted because they are controller UI and
policy metadata rather than execution inputs.

## Structured Patches

Each `apply_patch` call accepts one `*** Begin Patch` / `*** End Patch`
envelope using Mu/Codex add, update, move, and delete operations. The client
parses and preflights the complete envelope before changing the filesystem. It
rejects conflicting targets and existing add or move destinations, and combines
repeated non-moving updates to one path into one final write.

Relative paths resolve from `cwd`, or from the client process's current
directory when `cwd` is omitted. Absolute paths are used as written. Updating
through a symlink edits its regular-file target; moving or deleting a symlink
acts on the link. Existing-file updates preserve the inode, permissions, hard
links, and symlink target relationship, use an advisory lock, and keep a
recoverable sibling backup while writing.

Publication is recoverable, not a cross-file transaction. A later commit
failure reports earlier completed changes. Patch work runs through
`spawn_blocking`; cancellation or disconnect cannot stop filesystem work that
has already begun. Inspect affected paths after an interrupted call.

The client logs the full patch, working directory, result summary, and failures
to standard error. Patch content and paths may therefore be persisted by the
service supervisor and must be treated as sensitive.

## Screenshot Capture

On Unix, each `screenshot` call detects the graphical session and invokes
common capture software found in the client's `PATH`. Wayland uses
desktop-native tools and `grim`; X11 uses desktop-native tools, `maim`, `scrot`,
or ImageMagick. Windows invokes a bounded `pwsh` script using .NET drawing APIs
to capture the interactive virtual desktop as JPEG.

Capture is best-effort. A process-wide desktop lock serializes capture and
computer batches, including across reconnects. Capture programs have bounded
execution time and write into a private temporary directory. Only valid PNG
and JPEG output is accepted. Images are
limited to 8 MiB; when available on Unix, ImageMagick may reduce an oversized
valid capture to a bounded JPEG.

Results include image dimensions. The client retains only the latest
screenshot-to-input geometry, not the image bytes. A small PNG/JPEG header
reader extracts dimensions without image-codec dependencies; Unix reduction
preserves the original dimensions for coordinate mapping. Windows capture
and input both use per-monitor-aware physical pixels.

Connector does not persist screenshots or log their bytes. It logs only the
selected backend and image size. Screenshots can expose notifications,
credentials, private application content, and everything else visible to the
user's graphical session, so controller grants and client logs must be
protected accordingly.

## Desktop Input

`computer(actions)` exposes `move`, `click`, `scroll`, `drag`, `key`,
`type`, and `wait`. Batches
contain at most 32 actions; click count is 1–3, scroll amount is 1–100 wheel
steps, key chords contain 1–8 named keys, literal text is at most 8192 UTF-8
bytes with no NUL, and each wait is at most 5000 ms. There is a 30-second batch
execution budget checked between actions. Each X11 subprocess has a separate
5-second timeout; capture has its own timeout. This is not a hard 30-second
wall-clock limit including lock acquisition, cleanup, and capture.

The controller uses integer pixels from the most recent full screenshot,
with `(0, 0)` at its top-left. The client maps scaling and native desktop
offsets. A coordinate action requires a prior mappable screenshot; reconnects
invalidate the previous mapping after pending desktop work finishes. Capture
geometry is checked before and after capture, and again before actions;
changed geometry fails input and the final capture supplies a new observation.
This does not detect ordinary UI changes or make stale screenshots safe.
Windows retains virtual-desktop coverage, including negative native origins;
X11 retains backend-dependent full-X-screen capture. There is no per-monitor
selection, frame IDs, separate-X-screen stitching, or Wayland input.

Windows uses direct user32 FFI and `SendInput`; it does not elevate, bypass
UIPI, or target the secure desktop. X11 invokes `xdotool` using argument arrays
and sends literal text through stdin, never shell interpolation. Unicode
typing is backend/application-dependent and does not alter the clipboard.
No desktop framework or image-codec dependency is added to the Rust client.

The complete batch is preflighted before input. Actions execute sequentially,
stop on first failure, and always attempt one final screenshot. Results report
the number of completed actions, execution error, and image dimensions.
Failures can leave a partially executed action; capture failure does not imply
input failure. There is no rollback or automatic replay. A new observation is
required before deciding whether to retry an interrupted operation.

Keys/buttons are held only inside an action, with best-effort release on
failure. Cancellation is forwarded through the gateway. A desktop worker
retains the lock and finishes the current bounded action's cleanup even if
the MCP caller disconnects; subsequent actions stop. A process crash cannot
guarantee input release. Human input, other programs, and `run` commands can
still change the desktop while this lock is held.

Normal Ctrl-C shutdown waits for pending desktop cleanup before exiting.

Logs record batch counts and status, not action payloads or typed text.
Desktop input uses the same controller authorization as shell access and is
not a security sandbox alongside unrestricted shell access.

## Releases and verification

Linux release builds use `scripts/build-release.sh` and an installed musl
Rust target/toolchain; the resulting client is checked for static linkage.
Windows builds require a native MSVC CI runner with PowerShell 7. Its native
MCP/PowerShell and public WSS checks must pass before publishing the CI artifact;
Linux cross-compilation alone does not establish runtime correctness. R2 packages use immutable
versioned artifact keys and SHA-256 manifests, with the mutable stable pointer
published last. See [`docs/RELEASES.md`](docs/RELEASES.md).

Use the checked-in lockfile and run formatting, all-target check, Clippy with
warnings denied, and all-target tests. Exercise real native MCP initialization,
tools/list, calls, and cancellation on supported platforms. Test public WSS
separately from local WS: only WSS exercises TLS provider setup and public
certificate validation. An automated Unix prompt test must use an isolated
controlling PTY, not merely a PTY on stdin/stdout. Revoke temporary credentials
and remove test files/processes after interrupted as well as successful tests.
