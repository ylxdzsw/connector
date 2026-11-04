# Connector native client

Connector lets an authorized MCP controller operate Unix and Windows machines
behind NAT. This repository builds **only `connector-client`**, the Rust
process that opens an outbound WebSocket and runs native tools. The gateway is
a TypeScript Cloudflare Worker with a SQLite Durable Object in
`/root/sayuri/connector`; Sayuri provides authorization and R2 stores releases.
The retired Rust gateway and its deployment configuration are available in Git
history, not as a supported fallback.

```text
Controller -- MCP/HTTPS --> Worker / Durable Object -- MCP/WebSocket --> Native client
                              |          |                              |
                         Sayuri auth   R2 releases                shell/files/desktop
```

Production has used Cloudflare since 2026-11-04. Token management is at
`https://token.ylxdzsw.com/`; the controller endpoint is
`https://connector.ylxdzsw.com/mcp`. The gateway has no local Unix sockets and
does not accept the retired connection codes, OAuth grants, or Nginx identity
header. Controller OAuth remains supported by the Worker. Deployment state and
release provenance belong in `/root/sayuri/connector/DEPLOYMENT.md`.

## Connect a machine

Obtain a Sayuri token with capability `connector:client:<name>`, then run:

```sh
connector-client
```

The hidden `Connector token:` prompt reads `/dev/tty` on Unix or the console on
Windows, not standard input. For automation, `--token 'SAYURI_CLIENT_TOKEN'`
passes the credential explicitly. `--code` is only a compatibility spelling
for the same exact opaque value: **no trimming, case folding, or Crockford
normalization**. Credentials are not read from environment variables. Protect
command-line tokens from process inspection and shell history; never log or
commit them.

The default gateway is `https://connector.ylxdzsw.com`. `--gateway` accepts an
HTTP(S) base URL; HTTP is unencrypted and only for trusted local testing.
Transient failures reconnect with delays up to 30 seconds; HTTP 4xx admission
failures stop the client. Reconnection never replays operations.

Unix requires Bash. Windows requires PowerShell 7 (`pwsh.exe`) in `PATH` and
uses the native Windows build. Interactive desktop access is needed for
screenshots and input. Windows release artifacts come from native CI, including
PowerShell/MCP and public WSS checks; see [`docs/RELEASES.md`](docs/RELEASES.md).
Bootstrap scripts and downloads are owned by the Worker, not this package.

## Tools and security

The Worker exposes:

| Tool | Purpose |
| --- | --- |
| `clients()` | Discover connected names, system/shell, input capability, and client-side notes metadata. |
| `run(client, command, cwd?, timeout?, stdin?)` | Run a fresh Bash or PowerShell process; return combined output and exit code. |
| `apply_patch(client, patch, cwd?)` | Preflight and apply a structured add/update/move/delete patch. |
| `screenshot(client)` | Return a best-effort PNG/JPEG capture of the desktop. |
| `computer(client, actions)` | Execute sequential mouse/keyboard actions, then attempt a final screenshot. |

The native tools omit `client`; `notes_metadata()` is a read-only link tool,
not a controller tool. The gateway terminates both MCP sessions, maps request
IDs and cancellation, and relays complete content including images.

Sayuri client capability `connector:client:<name>` authenticates one named
link; `connector:control` authorizes controller access, as does a Worker-issued
controller OAuth token. These are distinct authorizations. A client token does
not itself authorize control. An authorized controller can operate all
connected clients with the privileges of their launching OS users. Connector
is not a sandbox and does not elevate privileges.

- Shell state never carries between calls. Nonzero exit status is a normal
  result; invocation and timeout failures are errors. Retained combined shell
  output is capped at 8 MiB, and serialized outgoing MCP messages at 24 MiB.
  Overflow returns an error, not successful truncation or automatic replay.
- Patches preflight the whole envelope, but publishing multiple files is not
  atomic. Updates preserve existing-file identity and permissions. Filesystem
  work already started can continue after cancellation or disconnect.
- Screenshots are serialized, bounded, validated, and limited to 8 MiB. Input
  supports Windows `SendInput` and X11 `xdotool`; Wayland input is unsupported.
  Capture before coordinate actions, and inspect a fresh screenshot after an
  interruption. Completed actions cannot be rolled back.
- The client creates `~/.connector/NOTES.md` if missing (under the Windows user
  profile), without overwriting existing content. Metadata reports an absolute
  client-side path and current line count, not file contents. Maintain durable
  context using ordinary shell/patch tools; never record secrets or treat
  historical notes as authority over current user instructions.
- Client logs contain full commands, stdin, retained output, patches, and patch
  summaries. Protect logs as sensitive. Screenshot bytes and typed desktop
  action payloads are not logged by their tools.

See [`DESIGN.md`](DESIGN.md) for native semantics and trust boundaries, and
[`docs/LINK.md`](docs/LINK.md) for the exact MCP wire contract.

## Build and validate

Use the checked-in lockfile and an installed current Rust toolchain:

```sh
cargo fmt --all -- --check
cargo check --all-targets --locked
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
```

Build the static Linux x86-64 client with the musl target/toolchain installed:

```sh
./scripts/build-release.sh
```

The artifact is `target/x86_64-unknown-linux-musl/release/connector-client`.
`CONNECTOR_CLIENT_TARGET` selects another installed musl target. The script
checks that the executable has no dynamic interpreter or shared dependencies.

On a native Windows runner with the MSVC toolchain and PowerShell 7:

```powershell
cargo build --release --locked --target x86_64-pc-windows-msvc --bin connector-client
```

Validate native Windows tool behavior and actual MCP initialization before
publishing an artifact. A Linux cross-check is not a Windows execution test.
For TLS changes, test public WSS separately from local WS. Artifact packaging,
immutable release keys, checksums, and R2 publication are documented in
[`docs/RELEASES.md`](docs/RELEASES.md).

## License

MIT.
