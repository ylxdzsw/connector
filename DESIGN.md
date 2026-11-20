# Connector design

The Rust client maintains one outbound WebSocket to the Sayuri Worker, which
relays stateless HTTP operations to the named connection. The native client
validates and runs shell or desktop operations on its host. The gateway retains
only small connection metadata and in-memory request mappings. See
[`docs/LINK.md`](docs/LINK.md) for the message contract.

## Credentials and trust

Sayuri tokens are opaque, case-sensitive bearers. The native client supplies an
explicit name in the WebSocket URL. The Worker checks `connector:client:<name>`
by literal prefix for that name, and separately checks `connector:control` on
`/v1/` for fleet-wide access. These authorizations are distinct. Expiry and
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
text message contains one complete Connector v1 JSON message. Admission HTTP 4xx
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
operators accordingly. Neither side needs to persist operation payloads. No credentials belong in
logs, committed files, or client notes.

## Limits, cancellation, and state

The link accepts only bounded full JSON messages. Interrupted shell calls drop
child processes; Unix uses process-group cleanup, while Windows uses child
cleanup. Desktop input stops subsequent actions, but bounded input cleanup can
outlive the request. Completed changes cannot be undone. Do not replay requests.

The client keeps no credential database or controller state. Reconnect resets
screenshot-to-input geometry after outstanding desktop work unwinds. The
client still initializes `~/.connector/NOTES.md` privately without overwriting
it; only its path is advertised, and normal remote shell commands can read it.

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
serialized link limit can also reject a result below this raw cap because JSON
escaping can expand it. Neither case triggers a
replay.

Execution returns combined output and an exit code. The client does not record
commands, stdin, or output in logs. File operations use the remote shell.

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
the controller disconnects; subsequent actions stop. A process crash cannot
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
WebSocket/PowerShell and public WSS checks must pass before publishing the CI artifact;
Linux cross-compilation alone does not establish runtime correctness. R2 packages use immutable
versioned artifact keys and SHA-256 manifests, with the mutable stable pointer
published last. See [`docs/RELEASES.md`](docs/RELEASES.md).

Use the checked-in lockfile and run formatting, all-target check, Clippy with
warnings denied, and all-target tests. Exercise native WebSocket readiness, shell calls, and cancellation on supported platforms. Test public WSS
separately from local WS: only WSS exercises TLS provider setup and public
certificate validation. An automated Unix prompt test must use an isolated
controlling PTY, not merely a PTY on stdin/stdout. Revoke temporary credentials
and remove test files/processes after interrupted as well as successful tests.
