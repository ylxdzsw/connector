# Connector Design

## Goal

Connector lets an authenticated MCP controller run non-persistent shell
commands on Unix clients behind NAT.

```text
Controller -- MCP/HTTPS --> Nginx -- Unix socket --> Gateway -- MCP/WebSocket --> Client -- bash -lc
                                                   |
                                                   +-- newline JSON-RPC on Unix sockets
```

The first version supports Unix clients, fresh shell commands, and best-effort
desktop screenshots. Windows, persistent terminals, and remote input are
future work.

## Participants

- **Client**: a program started by the script served by the gateway. It opens
  an outbound link, receives MCP tool calls, and executes its advertised shell.
- **Gateway**: `connector.ylxdzsw.com`, running behind Nginx on this server. It
  accepts HTTP only on a private Unix socket, authenticates participants,
  relays MCP through NAT, serves the connection script, and exposes local Unix
  sockets.
- **Controller**: an MCP client such as Mu or ChatGPT. Once authorized, it can
  control every connected client.
- **Channel**: a named route to one connected client. A channel is not a
  persistent shell.

## Authorization Policy

Controllers are authorized for Connector as a whole, not for individual
clients. An authenticated controller can list and control all clients that are
connected now or connect while its grant remains valid. V1 has no per-user,
per-controller, or per-client ACL.

The public MCP resource is:

```text
https://connector.ylxdzsw.com/mcp
```

The OAuth scope `control` grants access to all controller tools.

## Credentials

Three credentials have separate purposes:

| Credential | Presented by | Accepted by | Purpose |
| --- | --- | --- | --- |
| Nginx cookie | Browser | Nginx | Authenticate the human |
| OAuth token | Controller | Gateway `/mcp` | Authorize all client control |
| Connection code | Client | Gateway `/link` | Authenticate one named client |

A credential is never accepted in another role.

### Client credentials

The Nginx-authenticated management site asks for a client name and a validity
period in days. It creates:

```text
name
code
expires_at
```

`expires_at` is computed from the requested number of days. `name` is the
stable client ID and must be safe as a Unix socket filename.

The gateway generates an eight-character, case-insensitive Crockford Base32
connection code. The code contains 40 bits of entropy, identifies the client
record by itself, and is shown when created. The gateway stores the normalized
code directly and throttles failed `/link` authentication attempts. V1 accepts
the resulting disclosure and brute-force tradeoffs.

The code is reusable until expiry or revocation. Only one live connection is
allowed for its client record. A second connection is rejected. Disconnect
removes online state but retains the credential.

The management site can extend a non-revoked credential without changing its
connection code. Days are added to its current expiration, or from the current
time when the credential has already expired.

It can also rotate a non-revoked client code. Rotation replaces the stored code,
invalidates the previous code, disconnects the live link if present, and shows
the new copyable connection command. The client must run that command to
reconnect.

After creating a credential, the management site displays one copyable command
containing its connection code:

```bash
curl -fsSL https://connector.ylxdzsw.com/connect | bash -s -- '<connection-code>'
```

The script uses an executable `connector-client` already available in `PATH`,
or otherwise downloads a temporary copy and runs it. The script passes the
connection code to the binary as `--code`. The code is a reusable bearer
credential and the displayed command must be treated as secret. For manual use
without a command argument, the binary still prompts for the code and reads it
from `/dev/tty` without echo.

The distributed Linux client is built against musl as a fully static
executable. It has no glibc or shared-library dependency, although a separate
artifact is still required for each CPU architecture.

The client sends the code as a bearer credential to `/link`. The gateway looks
it up directly, obtains the corresponding name, and then upgrades the
connection to WebSocket.

### Controller credentials

The gateway is its own OAuth 2.1 authorization server and resource server. It
issues short-lived opaque access tokens and rotating refresh tokens. Stored
authorization codes and tokens are hashed.

ChatGPT is a predefined confidential OAuth client. Its client ID and exact
redirect URI are fixed in the gateway; only its secret is read from the
environment, and only the secret hash is persisted. Its credentials are entered
when the custom connector is created. V1 does not implement dynamic client
registration or Client ID Metadata Documents.

## OAuth Discovery

The gateway publishes protected-resource metadata:

```text
GET /.well-known/oauth-protected-resource
```

```json
{
  "resource": "https://connector.ylxdzsw.com/mcp",
  "authorization_servers": ["https://connector.ylxdzsw.com"],
  "scopes_supported": ["control"]
}
```

It also publishes authorization-server metadata:

```text
GET /.well-known/oauth-authorization-server
```

```json
{
  "issuer": "https://connector.ylxdzsw.com",
  "authorization_endpoint": "https://connector.ylxdzsw.com/oauth/authorize",
  "token_endpoint": "https://connector.ylxdzsw.com/oauth/token",
  "revocation_endpoint": "https://connector.ylxdzsw.com/oauth/revoke",
  "response_types_supported": ["code"],
  "grant_types_supported": ["authorization_code", "refresh_token"],
  "code_challenge_methods_supported": ["S256"],
  "token_endpoint_auth_methods_supported": ["client_secret_basic"],
  "scopes_supported": ["control", "offline_access"],
  "authorization_response_iss_parameter_supported": true
}
```

## OAuth Workflow

### 1. Challenge and discovery

An unauthenticated MCP request receives:

```http
HTTP/1.1 401 Unauthorized
WWW-Authenticate: Bearer resource_metadata="https://connector.ylxdzsw.com/.well-known/oauth-protected-resource", scope="control"
```

ChatGPT reads the protected-resource and authorization-server metadata.

### 2. Authorization request

ChatGPT opens `/oauth/authorize` in the user's browser with:

```text
response_type=code
client_id=<registered ChatGPT client>
redirect_uri=<registered ChatGPT callback>
scope=control offline_access
resource=https://connector.ylxdzsw.com/mcp
code_challenge=<PKCE challenge>
code_challenge_method=S256
state=<ChatGPT state>
```

Nginx requires its existing authentication cookie on this endpoint. The
current cookie gate has one identity, so Nginx forwards the constant subject
`owner` in a trusted header. If the gate becomes multi-user later, this may be
replaced by a stable user ID. Nginx must remove any inbound copy of the header
before setting it.

The gateway validates the OAuth client, exact redirect URI, scope, resource,
and PKCE method. It then asks the user to approve this grant:

```text
Allow this controller to run shell commands and capture graphical desktop
screenshots on all current and future connected clients until access is
revoked?
```

The consent POST uses CSRF protection. Consent is recorded for the authenticated
subject, OAuth client, and scopes.

### 3. Authorization code

After approval, the gateway creates a short-lived, single-use code bound to:

```text
subject
oauth_client_id
redirect_uri
resource
scopes
code_challenge
expires_at
```

It redirects to the registered callback with `code`, the original `state`, and
an `iss` parameter equal to the advertised issuer.

### 4. Token exchange

ChatGPT authenticates with HTTP Basic and exchanges the code at `/oauth/token`.
The request repeats the redirect URI and resource and supplies the PKCE
verifier. The gateway validates all bindings and atomically consumes the code.

The response contains a short-lived access token and, when `offline_access` was
granted, a refresh token. The access token is bound to the controller subject,
OAuth client, `control` scope, and Connector resource.

### 5. MCP requests

The controller presents the access token on every request:

```http
Authorization: Bearer <access-token>
```

The gateway checks the token hash, expiry, revocation state, resource, scope,
and OAuth client before dispatching a tool call.

### 6. Refresh and revocation

ChatGPT may exchange a refresh token for a new access token without the Nginx
cookie. Refresh tokens rotate on use. Reuse of an already rotated token revokes
that grant.

The management site can revoke a controller grant or a client credential.
Controller revocation invalidates its access and refresh tokens. Client
revocation closes that client's link. The two operations are independent.

## MCP Interfaces

### Controller to gateway

The gateway exposes standard MCP Streamable HTTP at `/mcp`. It is the MCP
server; the controller is the MCP client.

The gateway exposes three tools:

```text
clients()
run(client, command, cwd?, timeout?, stdin?)
screenshot(client)
```

`clients` returns each connected client's name, system, and shell. For example:

```json
{"clients":[{"name":"build-server","system":"linux","shell":"bash"}]}
```

The result is only a snapshot; callers must handle a client disconnecting
before a later `run` call.

`run` selects a client by name and relays one non-persistent command to that
client's advertised shell. An unknown or disconnected client produces a
tool-level availability error.

`screenshot` asks a client to capture its full graphical desktop and returns a
standard MCP PNG or JPEG image. The tool is always present; a headless client,
an unsupported platform, or a client without a usable capture program returns
a tool-level availability error.

All tools declare OAuth security metadata requiring the `control` scope.

### Gateway to client

The client opens a WebSocket to the gateway, but MCP roles follow message
direction: the gateway is the MCP client and the client is the MCP server.

The link carries exact MCP JSON-RPC over a custom WebSocket transport. Each
text frame contains one complete UTF-8 MCP message. The client credential is
authenticated during upgrade, and WebSocket Ping/Pong provides liveness. No
private heartbeat or authentication messages are mixed into MCP.

The client exposes two tools:

```text
run(command, cwd?, timeout?, stdin?)
screenshot()
```

The client reports its system and shell in standard MCP initialization
metadata. The gateway terminates the external and internal MCP sessions. It
handles `clients` itself and maps external `run` calls to the selected client's
`run` tool. It also relays `screenshot` calls and their MCP image content.
Request IDs, results, errors, and cancellation are mapped through both
sessions. A client that omits its environment metadata or provides malformed
values is rejected during link initialization.

### Local Unix socket

While a client is connected, the gateway listens on:

```text
/run/connector/<name>.sock
```

The socket exposes that client's `run(command, cwd?, timeout?, stdin?)` and
`screenshot()` tools using newline-delimited MCP JSON-RPC.
Channel sockets are mode `0600`. The runtime directory is mode `0710`, allowing
the Nginx worker group to traverse to the mode-`0660` HTTP gateway socket
without listing the directory or accessing channels. The gateway removes a
channel socket on disconnect.

## Bash Execution

Each call starts a fresh `bash -lc` process. `cwd` selects its working
directory, `stdin` supplies literal input, and `timeout` bounds execution. The
process exits before the call completes; shell state never carries between
calls.

Before execution, the client logs the command, working directory, timeout, and
full standard input to standard error through its tracing subscriber. After
execution it logs the full combined standard output and error plus the exit
code. The two records carry the MCP request ID for correlation. Operators must
protect client process logs because commands, input, and output may contain
sensitive data.

The result contains combined command output and the exit code. A nonzero exit
code is a normal tool result. MCP errors are reserved for invocation failures.
Mu's `title` and `risk` fields are omitted because they are controller UI and
policy metadata rather than execution inputs.

## Screenshot Capture

Each `screenshot` call detects the graphical session and invokes common capture
software found in the client's `PATH`. Wayland uses desktop-native tools and
`grim`; X11 uses desktop-native tools, `maim`, `scrot`, or ImageMagick. Commands
are invoked directly without a shell. A Wayland session never falls back to
X11 through Xwayland because that can return an incomplete desktop.

Capture is best-effort and has no persistent session state. One capture runs at
a time. Programs have bounded execution time, write into a private temporary
directory, and are killed with their process group on timeout or cancellation.
Only valid PNG and JPEG output is accepted. Images are limited to 8 MiB; when
available, ImageMagick may reduce an oversized valid capture to a bounded JPEG.

Connector does not persist screenshots or log their bytes. It logs only the
selected backend and image size. Screenshots can expose notifications,
credentials, private application content, and everything else visible to the
Unix user's graphical session, so controller grants and client logs must be
protected accordingly.

## State

SQLite persists:

- Client connection codes and expiry
- Registered OAuth clients
- Consent grants
- Authorization code hashes and bindings
- Access and refresh token hashes, expiry, rotation, and revocation

Live WebSockets, request mappings, and online status remain in memory. Connector
does not persist command logs, command output, or screenshots; an external
supervisor may retain the client's standard-error log. In-flight calls fail on
disconnect or gateway restart and are never replayed.

## Trust Boundaries

### Browser to Nginx

The Nginx cookie establishes the single `owner` identity. Nginx alone validates
it. The owner can provision clients and approve controller access to all
clients.

### Nginx to gateway

The gateway accepts HTTP only through a mode-`0660` Unix socket in a directory
searchable by the Nginx worker group. It trusts Nginx's identity header only on
management and authorization routes. Nginx strips client-supplied identity
headers. OAuth discovery, token, MCP, and client-link routes do not accept the
Nginx identity header as authentication.

### Controller to gateway

The OAuth client secret identifies registered controller software. PKCE binds
the authorization response to the initiating controller. The access token
authorizes the resulting controller session. A refresh token grants durable
control until expiry or revocation and must never appear in URLs or logs.

### Client to gateway

The connection code authenticates one named client link. It grants no
controller or management access. If disclosed, however, it can be used to
impersonate that client while the genuine client is disconnected, receive
commands intended for it, forge results, or occupy its connection slot. It
cannot create other client records. A connected client trusts the gateway to
send authorized calls and does not know which controller originated them.

### Gateway to clients

The gateway is the central authority and can command every connected client.
Compromise of the gateway, Nginx authentication path, or an active controller
grant permits commands on all clients.

### Client to operating system

Bash and screenshot capture programs run as the Unix user that launched the
client. Connector does not run the client as root or elevate privileges. OAuth
control therefore grants all privileges available to that Unix user and can
expose everything visible in that user's graphical session.

### Local processes

Filesystem ownership and mode protect Unix sockets. A process that can connect
to a channel socket can control that client without OAuth.

## Rationale

- **Global controller grants** match controllers that need to discover a
  changing set of clients.
- **Separate credential domains** prevent controller credentials from
  impersonating clients.
- **Connector-issued OAuth tokens** satisfy ChatGPT and bind tokens to the
  Connector MCP resource.
- **Existing Nginx authentication** avoids another human identity system.
- **Outbound WebSocket links** cross NAT and provide full-duplex relay and
  transport-level liveness.
- **Exact MCP at both boundaries** avoids a second command protocol.
- **Non-persistent Bash** matches Mu-style workflows without terminal session
  APIs.
- **External screenshot programs** cover common Linux desktops without adding
  native X11, Wayland, portal, or image-codec dependencies to the static client.
- **Unix sockets** expose live channels to local controllers and represent them
  in the filesystem.

## Alternatives and Rejections

### Direct GitHub or Linux.do OAuth tokens

Rejected because those providers authenticate users but do not issue tokens
for the Connector MCP resource. They may remain upstream identity providers
behind the existing Nginx cookie authentication.

### Per-client controller grants

Rejected because client availability is dynamic and an authenticated
controller is intended to control all clients. The global grant is explicit in
the consent screen.

### Client names embedded in MCP endpoints

Rejected because controllers are not bound to one client. `/mcp` is one
resource, `clients` discovers targets, and `bash` selects one.

### Shared client and controller token

Rejected because it lets a controller impersonate a disconnected client and
cannot provide ChatGPT's OAuth workflow.

### Standard Streamable HTTP for the client link

Rejected because it assumes the MCP client connects to the MCP server. Here
the MCP server is behind NAT and must initiate the network link.

### Raw TCP or HTTP/2 streaming

Rejected for v1 because both add deployment or custom stream complexity
without a clear benefit over WebSocket.

### Custom gateway-client command protocol

Rejected because MCP already provides tool discovery, calls, results, errors,
and cancellation.

### Persistent shell tools

Rejected for v1. Independent Bash calls are sufficient for agent workflows and
avoid exposing terminal lifecycle APIs.

### One-time connection codes

Rejected because clients must reconnect after restarts and network failures.
Named expiring records with reusable connection codes provide stable identity
and controlled reuse.

### In-memory credential storage

Rejected because credentials and OAuth grants must survive gateway restarts.
SQLite adds little operational complexity.

### Accounts inside Connector

Rejected because Nginx already establishes human identity. Connector stores
only the stable subject needed for consent and audit relationships.
