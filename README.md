# Connector

Connector lets an OAuth-authorized MCP controller run fresh shell commands and
capture best-effort desktop screenshots on Unix clients behind NAT. Clients
establish outbound WebSocket links; the gateway exposes Streamable HTTP MCP and
one local Unix socket per live client.

## Build

```bash
rustup target add x86_64-unknown-linux-musl
./scripts/build-release.sh
```

The build produces:

- `target/release/connector-gateway`
- `target/x86_64-unknown-linux-musl/release/connector-client`

The client is a fully static musl executable, so it does not depend on the
target host's glibc version or shared libraries. It still targets x86-64 Linux;
set `CONNECTOR_CLIENT_TARGET` to another installed musl target when building
for a different architecture, then install the resulting client as
`/opt/connector/connector-client` for `/connect` to serve.

## Configure

The deployment identity and paths are fixed in `src/config.rs`:

| Setting | Value |
| --- | --- |
| Public URL | `https://connector.ylxdzsw.com` |
| Gateway socket | `/run/connector/.gateway.sock` |
| SQLite database | `/var/lib/connector/connector.db` |
| Channel directory | `/run/connector` |
| Served client | `/opt/connector/connector-client` |
| Trusted subject header | `x-connector-subject` |
| OAuth client ID | `chatgpt` |
| OAuth redirect URI | `https://chatgpt.com/connector_platform_oauth_redirect` |

Only `CONNECTOR_OAUTH_CLIENT_SECRET` is required at runtime. `RUST_LOG` may be
set to adjust logging. The gateway always trusts the first `X-Forwarded-For`
address because its HTTP listener is accessible only to the local Nginx worker
through the protected Unix socket.

## Run

```bash
CONNECTOR_OAUTH_CLIENT_SECRET='generate-a-long-random-secret' \
target/release/connector-gateway
```

Put the private Unix socket behind TLS and an existing browser authentication
gate. The socket is mode `0660`; its directory must be searchable by the Nginx
worker group. Only management routes and `/oauth/authorize` may receive the
trusted subject header. Public OAuth, MCP, link, script, and discovery routes
must have any inbound copy removed. See [deploy/nginx.conf](deploy/nginx.conf).

The management page creates an eight-character connection code. On a client,
copy and run the complete command shown after creating the credential:

```bash
curl -fsSL https://connector.ylxdzsw.com/connect | bash -s -- 'CONNECTION_CODE'
```

The command contains the reusable client connection credential, so treat it as
secret. The client remains in the foreground and reconnects after transient
network failures. If an executable `connector-client` is already in `PATH`, the
script runs it instead of downloading another copy. For manual use without a
command argument, `connector-client` still reads the code without echo from
`/dev/tty`.

The management page can extend a non-revoked client credential without changing
its connection code, or rotate the code to invalidate the previous command. A
rotation disconnects the current client link; run the newly displayed command
to reconnect it. The client logs each Bash command, working directory,
timeout, and full standard input before execution. After execution it logs the
combined standard output and error plus the exit code. Operators must protect
these process logs because they can contain sensitive data.

The client also offers best-effort full-desktop screenshots by invoking common
capture software from its `PATH`. On Wayland it tries desktop-native tools and
`grim`; on X11 it also tries `maim`, `scrot`, and ImageMagick. No capture
software is bundled into the static client. The tool returns unavailable on a
headless or unsupported client. Screenshot bytes are not persisted or logged,
but controllers can receive everything visible in the user's graphical
session.

## MCP

Controllers connect to `https://connector.ylxdzsw.com/mcp` and receive three
tools:

- `clients()` returns current online clients as
  `{ "clients": [{ "name": string, "system": string, "shell": string }] }`.
- `run(client, command, cwd?, timeout?, stdin?)` invokes the selected client's
  shell once and returns `{ "output": string, "exit_code": number }`.
- `screenshot(client)` returns the selected client's full graphical desktop as
  an MCP PNG or JPEG image, or a tool error when capture is unavailable.

While a client is online, `/run/connector/<name>.sock` exposes that client's
`run(command, cwd?, timeout?, stdin?)` and `screenshot()` tools as
newline-delimited MCP JSON-RPC.
Channel sockets are mode `0600`. The shared runtime directory is mode `0710`:
the Nginx worker group can traverse it to the mode-`0660` `.gateway.sock`, but
cannot list the directory or access channel sockets.

## Test

```bash
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
```
