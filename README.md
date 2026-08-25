# Connector

Connector lets an OAuth-authorized MCP controller run fresh Bash commands on
Unix clients behind NAT. Clients establish outbound WebSocket links; the
gateway exposes Streamable HTTP MCP and one local Unix socket per live client.

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
for a different architecture. Set `CONNECTOR_CLIENT_BINARY` to the resulting
client binary that `/connect` should download.

## Configure

The gateway is configured through environment variables:

| Variable | Default | Purpose |
| --- | --- | --- |
| `CONNECTOR_LISTEN` | `127.0.0.1:3000` | Private gateway listener |
| `CONNECTOR_PUBLIC_URL` | `http://127.0.0.1:3000` | Public issuer URL; production must use HTTPS |
| `CONNECTOR_DATABASE` | `data/connector.db` | SQLite state file |
| `CONNECTOR_SOCKET_DIR` | `/run/connector` | Live channel sockets |
| `CONNECTOR_CLIENT_BINARY` | `target/x86_64-unknown-linux-musl/release/connector-client` | Static binary served by `/download/client` |
| `CONNECTOR_SUBJECT_HEADER` | `x-connector-subject` | Identity header set only by the trusted proxy |
| `CONNECTOR_TRUST_PROXY` | `false` | Use the first `X-Forwarded-For` address for link throttling |
| `CONNECTOR_OAUTH_CLIENT_ID` | unset | Predefined confidential OAuth client ID |
| `CONNECTOR_OAUTH_CLIENT_SECRET` | unset | OAuth client secret; only its SHA-256 hash is persisted |
| `CONNECTOR_OAUTH_REDIRECT_URI` | unset | Exact registered controller callback |

The three OAuth client variables must either all be set or all be absent. A
gateway without them can accept linked clients but cannot authorize a
controller.

## Run

```bash
CONNECTOR_PUBLIC_URL=https://connector.example.com \
CONNECTOR_OAUTH_CLIENT_ID=chatgpt \
CONNECTOR_OAUTH_CLIENT_SECRET='generate-a-long-random-secret' \
CONNECTOR_OAUTH_REDIRECT_URI='https://chatgpt.com/connector_platform_oauth_redirect' \
target/release/connector-gateway
```

Put the private listener behind TLS and an existing browser authentication
gate. Only management routes and `/oauth/authorize` may receive the trusted
subject header. Public OAuth, MCP, link, script, and discovery routes must have
any inbound copy removed. See [deploy/nginx.conf](deploy/nginx.conf).

The management page creates an eight-character connection code. On a client,
run the command shown there:

```bash
curl -fsSL https://connector.example.com/connect | bash
```

The client binary reads the code without echo from `/dev/tty`, then remains in
the foreground and reconnects after transient network failures. If an
executable `connector-client` is already in `PATH`, the script runs it instead
of downloading another copy.

The management page can extend a non-revoked client credential without changing
its connection code. The client logs each Bash command, working directory,
timeout, and full standard input before execution. After execution it logs the
combined standard output and error plus the exit code. Operators must protect
these process logs because they can contain sensitive data.

## MCP

Controllers connect to `https://connector.example.com/mcp` and receive two
tools:

- `clients()` returns the current online client names.
- `bash(client, command, cwd?, timeout?, stdin?)` runs `bash -lc` once and
  returns `{ "output": string, "exit_code": number }`.

While a client is online, `/run/connector/<name>.sock` exposes that client's
`bash(command, cwd?, timeout?, stdin?)` MCP server as newline-delimited JSON-RPC.
The socket mode is `0600`; the directory mode is `0700`.

## Test

```bash
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
```
