# Connector

Connector lets an OAuth-authorized MCP controller operate Unix machines that
are behind NAT. Each machine opens an outbound WebSocket to a central gateway;
the controller can then discover connected machines, run fresh Bash commands,
apply structured file patches, and capture best-effort desktop screenshots.

Connector is made of two Rust binaries:

- `connector-gateway` serves OAuth, Streamable HTTP MCP, the management UI,
  WebSocket links, and local Unix-socket channels.
- `connector-client` runs on each controlled machine and reconnects to the
  gateway without requiring an inbound port.

```text
Controller -- MCP/HTTPS --> Nginx -- Unix socket --> Gateway
                                                   |
                                                   +-- MCP/WebSocket --> Client -- bash -lc
                                                   +-- MCP over local Unix sockets
```

## Typical Usage

### 1. Build and deploy the gateway

The checked-in configuration targets `https://connector.ylxdzsw.com` and the
paths listed in [`src/config.rs`](src/config.rs). A different deployment must
change those constants and the matching Nginx configuration.

Build the gateway and the fully static x86-64 Linux client:

```bash
rustup target add x86_64-unknown-linux-musl
./scripts/build-release.sh
```

The build produces:

- `target/release/connector-gateway`
- `target/x86_64-unknown-linux-musl/release/connector-client`

Install the binaries and use the checked-in systemd and Nginx files as the
deployment templates:

- [`deploy/connector.service`](deploy/connector.service)
- [`deploy/connector.ylxdzsw.com.conf`](deploy/connector.ylxdzsw.com.conf)
- [`deploy/connector-proxy-headers.conf`](deploy/connector-proxy-headers.conf)

The gateway requires `CONNECTOR_OAUTH_CLIENT_SECRET`. `RUST_LOG` is optional.
It listens only on `/run/connector/.gateway.sock`; Nginx terminates TLS, applies
the existing browser cookie gate to management and authorization routes, and
proxies public MCP, OAuth, download, and client-link routes.

### 2. Connect a Unix machine

Open the management site, create a named client credential, and run the command
shown by the site on that machine:

```bash
curl -fsSL https://connector.ylxdzsw.com/connect | bash -s -- 'CONNECTION_CODE'
```

The command uses `connector-client` from `PATH` or temporarily downloads the
static client. The client stays in the foreground and reconnects after
transient network failures. Running `connector-client` without `--code` reads
the code from `/dev/tty` without echo.

The complete command contains a reusable bearer credential. Do not put it in
shell history, logs, source control, or process supervision configuration.
Rotating or revoking the credential disconnects the current link.

### 3. Connect an MCP controller

Configure the controller with this Streamable HTTP MCP endpoint:

```text
https://connector.ylxdzsw.com/mcp
```

The controller follows the gateway's OAuth discovery metadata, opens the
browser consent flow, and receives the `control` scope. An approved controller
can use all clients that are online now or connect while the grant is valid.

The gateway exposes:

| Tool | Purpose |
| --- | --- |
| `clients()` | List connected clients and their system and shell metadata. |
| `run(client, command, cwd?, timeout?, stdin?)` | Run one fresh Bash process and return combined output and its exit code. |
| `apply_patch(client, patch, cwd?)` | Apply one Mu/Codex-style structured patch after complete preflight. |
| `screenshot(client)` | Return a PNG or JPEG of the client's full desktop when a supported capture backend is available. |

Each online client also has a mode-`0600` local channel at
`/run/connector/<name>.sock`. It exposes `run`, `apply_patch`, and `screenshot`
as newline-delimited MCP JSON-RPC without OAuth; filesystem permissions are the
authorization boundary for this local interface.

## Key Designs

### MCP end to end

Connector terminates two MCP sessions rather than translating MCP into a
private command protocol. The external controller is an MCP client of the
gateway. On the outbound WebSocket link, the gateway is an MCP client of the
Unix client. The gateway maps calls, results, errors, cancellation, and image
content between those sessions.

### Separate credential domains

Three credentials have deliberately separate roles:

| Credential | Accepted by | Authority |
| --- | --- | --- |
| Nginx authentication cookie | Management and OAuth authorization routes | Manage clients and approve grants |
| OAuth access token | `/mcp` | Control every connected client |
| Client connection code | `/link` | Authenticate one named client link |

No credential is accepted in another role. OAuth grants are global rather than
per-client, so approval grants the controller the Unix-user privileges of all
current and future connected clients until expiry or revocation.

### Outbound links and ephemeral channels

Clients initiate authenticated WebSocket connections, which avoids inbound
network access through NAT. WebSocket Ping/Pong supplies transport liveness,
and only one live link is allowed for each client record. Live links, online
status, channel sockets, and in-flight request mappings exist only in memory;
disconnects and gateway restarts fail in-flight work instead of replaying it.

### Fresh command processes

Every `run` call starts a new `bash -lc` process with an optional working
directory, literal standard input, and timeout. Shell state does not carry
between calls. A nonzero exit status is a normal tool result; startup,
availability, timeout, and transport failures are tool errors.

The client traces complete commands, input, output, and patches. A service
supervisor may persist those logs, so operators must treat them as sensitive.

### Preflighted structured patches

`apply_patch` supports add, update, move, and delete operations in one
`*** Begin Patch` / `*** End Patch` envelope. The client parses and preflights
the entire envelope before publishing any change, rejects conflicting targets,
and preserves existing-file inodes, permissions, hard links, and symlink target
relationships where applicable.

Publication is recoverable but is not a cross-file transaction. If a later
commit fails, the tool reports which earlier changes completed. See
[`DESIGN.md`](DESIGN.md) for the exact filesystem semantics.

### Bounded screenshot capture

Screenshots use common Wayland or X11 capture tools already present on the
client. Capture is serialized, time-bounded, validated as PNG or JPEG, and
limited to 8 MiB. Connector does not store image bytes, but a successful call
can expose everything visible in the user's graphical session.

### Persistent credentials, transient workloads

SQLite stores client credentials, OAuth clients and grants, authorization code
hashes, and token hashes and lifecycle state. Connector does not persist
commands, command output, patch contents, screenshots, live links, or MCP
request mappings.

## Development

Use the checked-in `Cargo.lock` and run the same checks as CI:

```bash
cargo fmt --all -- --check
cargo check --all-targets --locked
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
```

Release builds use [`scripts/build-release.sh`](scripts/build-release.sh). The
client target defaults to `x86_64-unknown-linux-musl`; set
`CONNECTOR_CLIENT_TARGET` to another installed musl target for a different CPU
architecture.

For protocol details, trust boundaries, OAuth flows, patch behavior, and design
rationale, read [`DESIGN.md`](DESIGN.md).

## License

Connector is licensed under the MIT License.
