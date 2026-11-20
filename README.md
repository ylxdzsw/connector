# Connector native client

Connector lets an authorized controller operate Unix and Windows machines
behind NAT. This repository builds **only `connector-client`**, the Rust
process that opens an outbound WebSocket and runs native tools. The gateway is
a TypeScript Cloudflare Worker with a SQLite Durable Object in
`/root/sayuri/connector`; Sayuri provides authorization and R2 stores releases.
The retired Rust gateway and its deployment configuration are available in Git
history, not as a supported fallback.

```text
Controller -- HTTPS --> Worker / Durable Object -- WebSocket --> Native client
                              |          |                              |
                         Sayuri auth   R2 releases                shell/files/desktop
```

Connector management is at `https://connector.ylxdzsw.com/`; the controller
endpoint is `https://connector.ylxdzsw.com/v1/clients`. Both native links and
controllers use Sayuri bearer tokens. Deployment state and release provenance
belong in `/root/sayuri/connector/DEPLOYMENT.md`.

## Connect a machine

Create a client token at `https://connector.ylxdzsw.com/`, reveal it, and use
the generated bootstrap command or pass the named URL to a downloaded client:

```sh
connector-client --url 'wss://connector.ylxdzsw.com/link?name=desktop'
```

The hidden `Connector token:` prompt reads `/dev/tty` on Unix or the console on
Windows, not standard input. For automation, `--token 'SAYURI_CLIENT_TOKEN'`
passes the credential explicitly. `--code` is only a compatibility spelling
for the same exact opaque value: **no trimming, case folding, or Crockford
normalization**. Credentials are not read from environment variables. Protect
command-line tokens from process inspection and shell history; never log or
commit them.

`--url` takes the complete WebSocket URL. The gateway requires exactly one
`name` query parameter. Use `wss://` outside trusted local testing.
Transient failures reconnect with delays up to 30 seconds; HTTP 4xx admission
failures stop the client. Reconnection never replays operations.

Unix requires Bash. Windows requires PowerShell 7 (`pwsh.exe`) in `PATH` and
uses the native Windows build. Interactive desktop access is needed for
screenshots and input. Windows release artifacts come from native CI, including
PowerShell/WebSocket and public WSS checks; see [`docs/RELEASES.md`](docs/RELEASES.md).
Bootstrap scripts and downloads are owned by the Worker, not this package.

## Operations and security

The gateway offers Bearer-only `GET /v1/clients` for discovery, `POST /v1/exec`
for shell work, and `POST /v1/desktop` for observation or batches of actions.
The native link uses the `sayuri-connector-v1` WebSocket subprotocol. There are
no controller sessions or MCP tool envelopes.

The gateway checks `connector:client:<name>` for the name in the URL, using
literal prefix matching. `connector:control` authorizes fleet-wide controller
access. These are distinct authorizations. A client token does
not itself authorize control. An authorized controller can operate all
connected clients with the privileges of their launching OS users. Connector
is not a sandbox and does not elevate privileges.

- Shell state never carries between calls. Nonzero exit status is a normal
  result; invocation and timeout failures are errors. Retained combined shell
  output is capped at 8 MiB, and serialized outgoing link messages at 24 MiB.
  Overflow returns an error, not successful truncation or automatic replay.
- Screenshots are serialized, bounded, validated, and limited to 8 MiB. Input
  supports Windows `SendInput` and X11 `xdotool`; Wayland input is unsupported.
  Capture before coordinate actions, and inspect a fresh screenshot after an
  interruption. Completed actions cannot be rolled back.
- The client creates `~/.connector/NOTES.md` if missing (under the Windows user
  profile), without overwriting existing content. Metadata reports an absolute
  client-side path, not file contents. Maintain durable
  context using ordinary shell commands; never record secrets or treat
  historical notes as authority over current user instructions.
- The client does not log command text, output, or desktop payloads.

See [`DESIGN.md`](DESIGN.md) for native semantics and trust boundaries, and
[`docs/LINK.md`](docs/LINK.md) for the exact WebSocket wire contract.

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

Validate native Windows tool behavior and the actual named WebSocket link before
publishing an artifact. A Linux cross-check is not a Windows execution test.
For TLS changes, test public WSS separately from local WS. Artifact packaging,
immutable release keys, checksums, and R2 publication are documented in
[`docs/RELEASES.md`](docs/RELEASES.md).

## License

MIT.
