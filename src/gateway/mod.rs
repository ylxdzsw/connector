use std::{
    collections::{HashMap, HashSet, VecDeque},
    os::unix::fs::PermissionsExt,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    extract::{
        Form, Path, Query, Request, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::{DateTime, Utc};
use futures::{SinkExt, StreamExt, channel::mpsc};
use regex::Regex;
use rmcp::{
    RoleClient, ServiceExt,
    model::{ClientJsonRpcMessage, ServerJsonRpcMessage},
    transport::{
        StreamableHttpServerConfig, StreamableHttpService,
        streamable_http_server::session::local::LocalSessionManager,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::{
    net::UnixListener,
    sync::{Mutex, RwLock},
};
use tokio_util::sync::CancellationToken;
use tower_http::trace::TraceLayer;
use url::Url;

use crate::{
    config::{
        CLIENT_BINARY, Config, DATABASE, OAUTH_CLIENT_ID, OAUTH_REDIRECT_URI, PUBLIC_URL,
        RESOURCE_URL, SOCKET_DIR, SUBJECT_HEADER,
    },
    crypto::{connection_code, normalize_code, random_token},
    db::{AuthCodeBinding, Database, OAuthError},
    mcp::{ChannelMcp, GatewayMcp, LiveClient, LiveClients, client_environment},
};

#[derive(Clone)]
pub struct AppState {
    pub db: Database,
    pub clients: LiveClients,
    pending: Arc<Mutex<HashSet<String>>>,
    failures: Arc<Mutex<HashMap<String, VecDeque<Instant>>>>,
    shutdown: CancellationToken,
}

impl AppState {
    pub async fn new(config: Config) -> Result<Self> {
        let db = Database::open(DATABASE)?;
        db.register_oauth_client(
            OAUTH_CLIENT_ID,
            &config.oauth_client_secret,
            OAUTH_REDIRECT_URI,
        )
        .await?;
        tokio::fs::create_dir_all(SOCKET_DIR)
            .await
            .with_context(|| format!("create {SOCKET_DIR}"))?;
        std::fs::set_permissions(SOCKET_DIR, std::fs::Permissions::from_mode(0o710))?;
        Ok(Self {
            db,
            clients: Arc::new(RwLock::new(HashMap::new())),
            pending: Arc::new(Mutex::new(HashSet::new())),
            failures: Arc::new(Mutex::new(HashMap::new())),
            shutdown: CancellationToken::new(),
        })
    }

    pub async fn shutdown(&self) {
        self.shutdown.cancel();
        let clients = std::mem::take(&mut *self.clients.write().await);
        for client in clients.into_values() {
            client.disconnect.cancel();
        }
    }
}

pub fn router(state: AppState) -> Router {
    let mcp_state = state.clone();
    let mcp_clients = state.clients.clone();
    let mcp_service: StreamableHttpService<GatewayMcp, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(GatewayMcp::new(mcp_clients.clone())),
            Default::default(),
            StreamableHttpServerConfig::default()
                .with_allowed_hosts([host_of(PUBLIC_URL), "localhost".into(), "127.0.0.1".into()])
                .with_cancellation_token(state.shutdown.child_token()),
        );
    let protected_mcp = Router::new()
        .nest_service("/mcp", mcp_service)
        .route_layer(middleware::from_fn_with_state(mcp_state, mcp_auth));

    Router::new()
        .route("/", get(management))
        .route("/clients", post(create_client))
        .route("/clients/{name}/extend", post(extend_client))
        .route("/clients/{name}/revoke", post(revoke_client))
        .route("/grants/{id}/revoke", post(revoke_grant))
        .route("/oauth/authorize", get(authorize).post(authorize_consent))
        .route("/oauth/token", post(token))
        .route("/oauth/revoke", post(revoke_token))
        .route(
            "/.well-known/oauth-protected-resource",
            get(protected_resource_metadata),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(authorization_server_metadata),
        )
        .route("/link", get(link))
        .route("/connect", get(connect_script))
        .route("/download/client", get(download_client))
        .route("/assets/styles.css", get(styles))
        .merge(protected_mcp)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn mcp_auth(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let token = bearer(request.headers()).map(str::to_owned);
    if let Some(token) = token
        && state
            .db
            .validate_access_token(&token, RESOURCE_URL, "control")
            .await
            .unwrap_or(false)
    {
        return next.run(request).await;
    }
    let mut response = (StatusCode::UNAUTHORIZED, "OAuth access token required").into_response();
    response.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_str(&format!(
            "Bearer resource_metadata=\"{}/.well-known/oauth-protected-resource\", scope=\"control\"",
            PUBLIC_URL
        )).unwrap(),
    );
    response
}

#[derive(Serialize)]
struct ProtectedResourceMetadata {
    resource: String,
    authorization_servers: Vec<String>,
    scopes_supported: Vec<&'static str>,
}

async fn protected_resource_metadata() -> Json<ProtectedResourceMetadata> {
    Json(ProtectedResourceMetadata {
        resource: RESOURCE_URL.into(),
        authorization_servers: vec![PUBLIC_URL.into()],
        scopes_supported: vec!["control"],
    })
}

async fn authorization_server_metadata() -> Json<serde_json::Value> {
    let base = PUBLIC_URL;
    Json(json!({
        "issuer": base,
        "authorization_endpoint": format!("{base}/oauth/authorize"),
        "token_endpoint": format!("{base}/oauth/token"),
        "revocation_endpoint": format!("{base}/oauth/revoke"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["client_secret_basic"],
        "scopes_supported": ["control", "offline_access"],
        "authorization_response_iss_parameter_supported": true
    }))
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct AuthorizeParams {
    response_type: String,
    client_id: String,
    redirect_uri: String,
    scope: String,
    resource: String,
    code_challenge: String,
    code_challenge_method: String,
    state: String,
}

#[derive(Deserialize)]
struct ConsentForm {
    #[serde(flatten)]
    oauth: AuthorizeParams,
    csrf: String,
    decision: String,
}

async fn authorize(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<AuthorizeParams>,
) -> Response {
    let subject = match subject(&headers) {
        Ok(subject) => subject,
        Err(status) => return status.into_response(),
    };
    if let Err(message) = validate_authorize(&state, &query).await {
        return error_page(
            StatusCode::BAD_REQUEST,
            "Invalid authorization request",
            &message,
        );
    }
    let (csrf, set_cookie) = csrf_for(&headers);
    let hidden = oauth_hidden(&query);
    let offline = if query
        .scope
        .split_ascii_whitespace()
        .any(|s| s == "offline_access")
    {
        "<div class=\"scope\"><span>Stay authorized</span><strong>Until revoked</strong></div>"
    } else {
        ""
    };
    let body = format!(
        r#"
<div class="consent"><div class="panel">
  <div class="consent-mark">C</div>
  <h1>Allow controller access?</h1>
  <p><strong>{}</strong> is requesting control of Connector as <strong>{}</strong>.</p>
  <div class="warning">This controller will be able to run shell commands and capture graphical desktop screenshots on all current and future connected clients until access is revoked.</div>
  <div class="scope"><span>Discover clients</span><strong>Allowed</strong></div>
  <div class="scope"><span>Run commands and capture screenshots</span><strong>Allowed</strong></div>{}
  <form method="post" action="/oauth/authorize" class="consent-actions">{}<input type="hidden" name="csrf" value="{}">
    <button class="secondary" name="decision" value="deny">Deny</button>
    <button name="decision" value="allow">Allow access</button>
  </form>
</div></div>"#,
        escape(&query.client_id),
        escape(&subject),
        offline,
        hidden,
        escape(&csrf)
    );
    html_response("Controller authorization", &body, set_cookie)
}

async fn authorize_consent(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<ConsentForm>,
) -> Response {
    let subject = match subject(&headers) {
        Ok(subject) => subject,
        Err(status) => return status.into_response(),
    };
    if !check_csrf(&headers, &form.csrf) {
        return error_page(
            StatusCode::FORBIDDEN,
            "Request rejected",
            "The form expired. Return to the authorization request and try again.",
        );
    }
    if let Err(message) = validate_authorize(&state, &form.oauth).await {
        return error_page(
            StatusCode::BAD_REQUEST,
            "Invalid authorization request",
            &message,
        );
    }
    let mut redirect = Url::parse(&form.oauth.redirect_uri).expect("validated redirect URI");
    if form.decision != "allow" {
        redirect
            .query_pairs_mut()
            .append_pair("error", "access_denied")
            .append_pair("state", &form.oauth.state)
            .append_pair("iss", PUBLIC_URL);
        return Redirect::to(redirect.as_str()).into_response();
    }
    let scopes = canonical_scopes(&form.oauth.scope).expect("validated scopes");
    let code = match state
        .db
        .create_authorization_code(AuthCodeBinding {
            subject,
            client_id: form.oauth.client_id,
            redirect_uri: form.oauth.redirect_uri.clone(),
            resource: form.oauth.resource,
            scopes,
            code_challenge: form.oauth.code_challenge,
        })
        .await
    {
        Ok(code) => code,
        Err(error) => {
            return error_page(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Authorization failed",
                &error.to_string(),
            );
        }
    };
    redirect
        .query_pairs_mut()
        .append_pair("code", &code)
        .append_pair("state", &form.oauth.state)
        .append_pair("iss", PUBLIC_URL);
    Redirect::to(redirect.as_str()).into_response()
}

async fn validate_authorize(state: &AppState, query: &AuthorizeParams) -> Result<(), String> {
    if query.response_type != "code"
        || query.code_challenge_method != "S256"
        || query.resource != RESOURCE_URL
    {
        return Err("Unsupported response type, PKCE method, or resource.".into());
    }
    if query.state.is_empty()
        || query.code_challenge.len() != 43
        || !query
            .code_challenge
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b))
    {
        return Err("State and a valid S256 PKCE challenge are required.".into());
    }
    canonical_scopes(&query.scope)?;
    let client = state
        .db
        .oauth_client(&query.client_id)
        .await
        .map_err(|_| "Authorization storage is unavailable.")?
        .ok_or("Unknown OAuth client.")?;
    if client.redirect_uri != query.redirect_uri {
        return Err("The redirect URI is not registered for this client.".into());
    }
    Ok(())
}

fn canonical_scopes(value: &str) -> Result<String, String> {
    let parts: HashSet<_> = value.split_ascii_whitespace().collect();
    if !parts.contains("control")
        || parts
            .iter()
            .any(|scope| *scope != "control" && *scope != "offline_access")
    {
        return Err(
            "The request must use the control scope and may request offline_access.".into(),
        );
    }
    Ok(if parts.contains("offline_access") {
        "control offline_access"
    } else {
        "control"
    }
    .into())
}

#[derive(Deserialize)]
struct TokenForm {
    grant_type: String,
    code: Option<String>,
    redirect_uri: Option<String>,
    resource: Option<String>,
    code_verifier: Option<String>,
    refresh_token: Option<String>,
}

async fn token(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<TokenForm>,
) -> Response {
    let (client_id, secret) = match basic_credentials(&headers) {
        Some(credentials) => credentials,
        None => {
            return oauth_error(
                StatusCode::UNAUTHORIZED,
                "invalid_client",
                "HTTP Basic client authentication is required",
            );
        }
    };
    if state
        .db
        .authenticate_oauth_client(&client_id, &secret)
        .await
        .ok()
        .flatten()
        .is_none()
    {
        return oauth_error(
            StatusCode::UNAUTHORIZED,
            "invalid_client",
            "Client authentication failed",
        );
    }
    let resource = match form.resource.as_deref() {
        Some(value) if value == RESOURCE_URL => value,
        _ => {
            return oauth_error(
                StatusCode::BAD_REQUEST,
                "invalid_target",
                "The Connector resource is required",
            );
        }
    };
    let issue = match form.grant_type.as_str() {
        "authorization_code" => match (&form.code, &form.redirect_uri, &form.code_verifier) {
            (Some(code), Some(redirect), Some(verifier)) => {
                state
                    .db
                    .exchange_authorization_code(code, &client_id, redirect, resource, verifier)
                    .await
            }
            _ => Err(OAuthError::InvalidRequest),
        },
        "refresh_token" => match &form.refresh_token {
            Some(refresh) => {
                state
                    .db
                    .rotate_refresh_token(refresh, &client_id, resource)
                    .await
            }
            None => Err(OAuthError::InvalidRequest),
        },
        _ => {
            return oauth_error(
                StatusCode::BAD_REQUEST,
                "unsupported_grant_type",
                "Unsupported grant type",
            );
        }
    };
    match issue {
        Ok(issue) => no_store(Json(issue).into_response()),
        Err(OAuthError::InvalidRequest) => oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "Required token parameters are missing",
        ),
        Err(OAuthError::InvalidGrant) => oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_grant",
            "The grant is invalid, expired, consumed, or revoked",
        ),
        Err(_) => oauth_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "temporarily_unavailable",
            "Authorization storage is unavailable",
        ),
    }
}

#[derive(Deserialize)]
struct RevokeForm {
    token: String,
}

async fn revoke_token(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<RevokeForm>,
) -> Response {
    let Some((client_id, secret)) = basic_credentials(&headers) else {
        return oauth_error(
            StatusCode::UNAUTHORIZED,
            "invalid_client",
            "HTTP Basic client authentication is required",
        );
    };
    if state
        .db
        .authenticate_oauth_client(&client_id, &secret)
        .await
        .ok()
        .flatten()
        .is_none()
    {
        return oauth_error(
            StatusCode::UNAUTHORIZED,
            "invalid_client",
            "Client authentication failed",
        );
    }
    let _ = state.db.revoke_token(&form.token, &client_id).await;
    no_store(StatusCode::OK.into_response())
}

#[derive(Deserialize)]
struct CreateClientForm {
    name: String,
    days: u32,
    csrf: String,
}

async fn management(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let owner = match subject(&headers) {
        Ok(v) => v,
        Err(status) => return status.into_response(),
    };
    render_management(&state, &headers, &owner, None).await
}

async fn create_client(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<CreateClientForm>,
) -> Response {
    let owner = match subject(&headers) {
        Ok(v) => v,
        Err(status) => return status.into_response(),
    };
    if !check_csrf(&headers, &form.csrf) {
        return error_page(
            StatusCode::FORBIDDEN,
            "Request rejected",
            "The form expired. Reload the page and try again.",
        );
    }
    let valid_name = Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$")
        .unwrap()
        .is_match(&form.name)
        && form.name != "."
        && form.name != "..";
    if !valid_name || !(1..=3650).contains(&form.days) {
        return error_page(
            StatusCode::BAD_REQUEST,
            "Invalid client",
            "Names may use letters, numbers, dot, underscore, and hyphen. Validity must be 1 to 3650 days.",
        );
    }
    let code = connection_code();
    let expires = Utc::now().timestamp() + i64::from(form.days) * 86_400;
    if let Err(error) = state.db.create_client(&form.name, &code, expires).await {
        return error_page(
            StatusCode::CONFLICT,
            "Client not created",
            &format!("The name or code already exists: {error}"),
        );
    }
    render_management(&state, &headers, &owner, Some((&form.name, &code))).await
}

#[derive(Deserialize)]
struct CsrfForm {
    csrf: String,
}

#[derive(Deserialize)]
struct ExtendClientForm {
    days: u32,
    csrf: String,
}

async fn extend_client(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Form(form): Form<ExtendClientForm>,
) -> Response {
    if subject(&headers).is_err() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if !check_csrf(&headers, &form.csrf) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if !(1..=3650).contains(&form.days) {
        return error_page(
            StatusCode::BAD_REQUEST,
            "Invalid extension",
            "Validity must be extended by 1 to 3650 days.",
        );
    }
    match state.db.extend_client(&name, form.days).await {
        Ok(true) => Redirect::to("/").into_response(),
        Ok(false) => error_page(
            StatusCode::NOT_FOUND,
            "Client not extended",
            "The client does not exist or has been revoked.",
        ),
        Err(error) => error_page(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Client not extended",
            &error.to_string(),
        ),
    }
}

async fn revoke_client(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Form(form): Form<CsrfForm>,
) -> Response {
    if subject(&headers).is_err() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if !check_csrf(&headers, &form.csrf) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let _ = state.db.revoke_client(&name).await;
    if let Some(client) = state.clients.write().await.remove(&name) {
        client.disconnect.cancel();
    }
    Redirect::to("/").into_response()
}

async fn revoke_grant(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> Response {
    if subject(&headers).is_err() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if !check_csrf(&headers, &form.csrf) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let _ = state.db.revoke_grant(id).await;
    Redirect::to("/").into_response()
}

async fn render_management(
    state: &AppState,
    headers: &HeaderMap,
    owner: &str,
    created: Option<(&str, &str)>,
) -> Response {
    let clients = state.db.list_clients().await.unwrap_or_default();
    let grants = state.db.list_grants().await.unwrap_or_default();
    let online: HashSet<_> = state.clients.read().await.keys().cloned().collect();
    let (csrf, set_cookie) = csrf_for(headers);
    let notice = created.map(|(name, code)| format!(
        "<div class=\"notice\"><strong>Client {} created</strong>Connection code: <code>{}</code>. This is the only time the code is shown.</div>", escape(name), escape(code)
    )).unwrap_or_default();
    let client_rows: String = if clients.is_empty() {
        "<tr><td colspan=\"4\" class=\"empty\">No client credentials yet.</td></tr>".into()
    } else {
        let now = Utc::now().timestamp();
        clients.into_iter().map(|client| {
            let status = if client.revoked_at.is_some() { "<span class=\"pill revoked\">Revoked</span>" } else if client.expires_at <= now { "<span class=\"pill expired\">Expired</span>" } else if online.contains(&client.name) { "<span class=\"pill online\">Online</span>" } else { "<span class=\"pill\">Offline</span>" };
            let action = if client.revoked_at.is_none() { format!("<div class=\"client-actions\"><form class=\"extend\" method=\"post\" action=\"/clients/{}/extend\"><input type=\"hidden\" name=\"csrf\" value=\"{}\"><input required type=\"number\" min=\"1\" max=\"3650\" value=\"30\" name=\"days\" aria-label=\"Days to extend\" title=\"Days to extend\"><button class=\"secondary\">Extend</button></form><form method=\"post\" action=\"/clients/{}/revoke\"><input type=\"hidden\" name=\"csrf\" value=\"{}\"><button class=\"danger\">Revoke</button></form></div>", escape(&client.name), escape(&csrf), escape(&client.name), escape(&csrf)) } else { String::new() };
            format!("<tr><td><strong>{}</strong></td><td>{}</td><td>{}</td><td class=\"actions\">{}</td></tr>", escape(&client.name), status, time_label(client.expires_at), action)
        }).collect()
    };
    let grant_rows: String = if grants.is_empty() {
        "<tr><td colspan=\"5\" class=\"empty\">No controller grants yet.</td></tr>".into()
    } else {
        grants.into_iter().map(|grant| {
            let status = if grant.revoked_at.is_some() { "<span class=\"pill revoked\">Revoked</span>" } else { "<span class=\"pill online\">Active</span>" };
            let action = if grant.revoked_at.is_none() { format!("<form method=\"post\" action=\"/grants/{}/revoke\"><input type=\"hidden\" name=\"csrf\" value=\"{}\"><button class=\"danger\">Revoke</button></form>", grant.id, escape(&csrf)) } else { String::new() };
            format!("<tr><td>{}</td><td><strong>{}</strong></td><td>{}</td><td>{}</td><td class=\"actions\">{}</td></tr>", escape(&grant.client_id), escape(&grant.scopes), status, time_label(grant.created_at), action)
        }).collect()
    };
    let online_count = online.len();
    let body = format!(
        r#"
<header class="topbar"><div class="topbar-inner"><div class="brand"><span class="mark">C</span>Connector</div><span class="subject">{}</span></div></header>
<main>{}<div class="page-head"><div><h1>Connection control</h1><p>Provision Unix clients and manage controller access.</p></div><span class="status"><span class="dot online"></span>{} client{} online</span></div>
<section><div class="section-head"><h2>Connect a client</h2></div><div class="panel"><div class="command"><code>curl -fsSL {}/connect | bash</code></div><form class="create" method="post" action="/clients"><input type="hidden" name="csrf" value="{}"><label>Client name<input required maxlength="64" name="name" placeholder="build-server" pattern="[A-Za-z0-9][A-Za-z0-9._-]*"></label><label>Valid for<input required type="number" min="1" max="3650" value="30" name="days"></label><button>Create credential</button></form></div></section>
<section class="section"><div class="section-head"><h2>Clients</h2></div><div class="panel table-wrap"><table><thead><tr><th>Name</th><th>Status</th><th>Expires</th><th></th></tr></thead><tbody>{}</tbody></table></div></section>
<section class="section"><div class="section-head"><h2>Controller grants</h2></div><div class="panel table-wrap"><table><thead><tr><th>Controller</th><th>Scopes</th><th>Status</th><th>Created</th><th></th></tr></thead><tbody>{}</tbody></table></div></section>
</main>"#,
        escape(owner),
        notice,
        online_count,
        if online_count == 1 { "" } else { "s" },
        escape(PUBLIC_URL),
        escape(&csrf),
        client_rows,
        grant_rows
    );
    html_response("Connector", &body, set_cookie)
}

async fn link(State(state): State<AppState>, headers: HeaderMap, ws: WebSocketUpgrade) -> Response {
    let key = client_ip(&headers);
    if throttled(&state, &key).await {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "Too many failed connection attempts",
        )
            .into_response();
    }
    let Some(raw_code) = bearer(&headers) else {
        record_failure(&state, key).await;
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let code = normalize_code(raw_code);
    let record = match state.db.client_for_code(&code).await {
        Ok(Some(record)) => record,
        _ => {
            record_failure(&state, key).await;
            return StatusCode::UNAUTHORIZED.into_response();
        }
    };
    state.failures.lock().await.remove(&key);
    let mut pending = state.pending.lock().await;
    if state.clients.read().await.contains_key(&record.name) || !pending.insert(record.name.clone())
    {
        return (StatusCode::CONFLICT, "Client is already connected").into_response();
    }
    drop(pending);
    let name = record.name;
    ws.on_upgrade(move |socket| client_session(state, name, code, socket))
        .into_response()
}

async fn client_session(state: AppState, name: String, code: String, socket: WebSocket) {
    let result = establish_client(&state, &name, &code, socket).await;
    state.pending.lock().await.remove(&name);
    if let Err(error) = result {
        tracing::warn!(client=%name, %error, "client link ended");
    }
}

async fn establish_client(
    state: &AppState,
    name: &str,
    code: &str,
    socket: WebSocket,
) -> Result<()> {
    let disconnect = CancellationToken::new();
    let (outgoing, outgoing_rx) = mpsc::unbounded::<ClientJsonRpcMessage>();
    let (incoming_tx, incoming) = mpsc::unbounded::<ServerJsonRpcMessage>();
    let io_task = tokio::spawn(websocket_io(
        socket,
        outgoing_rx,
        incoming_tx,
        disconnect.clone(),
    ));
    let service = ().serve((outgoing, incoming)).await.context("initialize client MCP")?;
    let environment =
        client_environment(service.peer()).context("client did not report its system and shell")?;
    if state
        .db
        .client_for_code(code)
        .await?
        .as_ref()
        .map(|r| r.name.as_str())
        != Some(name)
    {
        disconnect.cancel();
        return Ok(());
    }
    let (listener, path) = bind_unix_socket(name).await?;
    let id = random_token();
    state.clients.write().await.insert(
        name.to_owned(),
        LiveClient {
            connection_id: id.clone(),
            peer: service.peer().clone(),
            environment: environment.clone(),
            disconnect: disconnect.clone(),
        },
    );
    tracing::info!(client=%name, system=%environment.system, shell=%environment.shell, "client connected");
    let socket_task = tokio::spawn(serve_unix_socket(
        listener,
        path,
        service.peer().clone(),
        disconnect.clone(),
    ));
    tokio::select! {
        _ = disconnect.cancelled() => {},
        result = service.waiting() => { result.context("client MCP task")?; }
    }
    disconnect.cancel();
    let _ = socket_task.await;
    let _ = io_task.await;
    let mut clients = state.clients.write().await;
    if clients
        .get(name)
        .is_some_and(|client| client.connection_id == id)
    {
        clients.remove(name);
    }
    tracing::info!(client=%name, "client disconnected");
    Ok(())
}

async fn websocket_io(
    socket: WebSocket,
    mut outgoing: mpsc::UnboundedReceiver<ClientJsonRpcMessage>,
    incoming: mpsc::UnboundedSender<ServerJsonRpcMessage>,
    cancel: CancellationToken,
) {
    let (mut write, mut read) = socket.split();
    let last_pong = Arc::new(std::sync::Mutex::new(Instant::now()));
    let reader_pong = last_pong.clone();
    let reader_cancel = cancel.clone();
    let mut reader = tokio::spawn(async move {
        while let Some(message) = read.next().await {
            match message {
                Ok(Message::Text(text)) => match serde_json::from_str(&text) {
                    Ok(message) => {
                        if incoming.unbounded_send(message).is_err() {
                            break;
                        }
                    }
                    Err(error) => tracing::warn!(%error, "invalid client MCP message"),
                },
                Ok(Message::Pong(_)) => *reader_pong.lock().expect("pong lock") = Instant::now(),
                Ok(Message::Close(_)) | Err(_) => break,
                _ => {}
            }
        }
        reader_cancel.cancel();
    });
    let mut ping = tokio::time::interval(Duration::from_secs(15));
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = ping.tick() => {
                if last_pong.lock().expect("pong lock").elapsed() > Duration::from_secs(45) { break; }
                if write.send(Message::Ping(Vec::new().into())).await.is_err() { break; }
            }
            message = outgoing.next() => match message {
                Some(message) => if write.send(Message::Text(serde_json::to_string(&message).expect("MCP message serializes").into())).await.is_err() { break; },
                None => break,
            }
        }
    }
    cancel.cancel();
    let _ = write.send(Message::Close(None)).await;
    drop(write);
    if tokio::time::timeout(Duration::from_secs(2), &mut reader)
        .await
        .is_err()
    {
        reader.abort();
    }
}

async fn bind_unix_socket(name: &str) -> Result<(UnixListener, std::path::PathBuf)> {
    let path = std::path::Path::new(SOCKET_DIR).join(format!("{name}.sock"));
    if path.exists() {
        let _ = tokio::fs::remove_file(&path).await;
    }
    let listener = UnixListener::bind(&path).with_context(|| format!("bind {}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    Ok((listener, path))
}

async fn serve_unix_socket(
    listener: UnixListener,
    path: std::path::PathBuf,
    peer: rmcp::service::Peer<RoleClient>,
    cancel: CancellationToken,
) {
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let server = ChannelMcp::new(peer.clone());
                    tokio::spawn(async move {
                        if let Ok(service) = server.serve(stream).await { let _ = service.waiting().await; }
                    });
                }
                Err(error) => { tracing::warn!(%error, "Unix socket accept failed"); break; }
            }
        }
    }
    drop(listener);
    let _ = tokio::fs::remove_file(path).await;
}

async fn connect_script() -> Response {
    let script = connect_script_body(PUBLIC_URL);
    (
        [
            (header::CONTENT_TYPE, "text/x-shellscript; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        script,
    )
        .into_response()
}

fn connect_script_body(base: &str) -> String {
    format!(
        r#"#!/bin/sh
set -eu
client="$(command -v connector-client 2>/dev/null || true)"
if [ -z "$client" ] || [ ! -x "$client" ]; then
    client="$(mktemp "${{TMPDIR:-/tmp}}/connector-client.XXXXXX")"
    trap 'rm -f "$client"' EXIT HUP INT TERM
    curl -fsSL '{base}/download/client' -o "$client"
    chmod 700 "$client"
fi
"$client" --gateway '{base}'
"#
    )
}

async fn download_client() -> Response {
    match tokio::fs::read(CLIENT_BINARY).await {
        Ok(binary) => (
            [
                (header::CONTENT_TYPE, "application/octet-stream"),
                (
                    header::CONTENT_DISPOSITION,
                    "attachment; filename=connector-client",
                ),
            ],
            binary,
        )
            .into_response(),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Client binary is not available on the gateway",
        )
            .into_response(),
    }
}

async fn styles() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=3600"),
        ],
        include_str!("../../assets/styles.css"),
    )
        .into_response()
}

fn subject(headers: &HeaderMap) -> Result<String, StatusCode> {
    headers
        .get(SUBJECT_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or(StatusCode::UNAUTHORIZED)
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .filter(|value| !value.is_empty())
}

fn basic_credentials(headers: &HeaderMap) -> Option<(String, String)> {
    let encoded = headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Basic ")?;
    let decoded = String::from_utf8(STANDARD.decode(encoded).ok()?).ok()?;
    let (id, secret) = decoded.split_once(':')?;
    Some((id.to_owned(), secret.to_owned()))
}

fn csrf_for(headers: &HeaderMap) -> (String, Option<String>) {
    if let Some(value) = cookie(headers, "connector_csrf") {
        return (value.to_owned(), None);
    }
    let token = random_token();
    let cookie =
        format!("connector_csrf={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age=3600; Secure");
    (token, Some(cookie))
}

fn check_csrf(headers: &HeaderMap, form: &str) -> bool {
    cookie(headers, "connector_csrf").is_some_and(|cookie| {
        subtle::ConstantTimeEq::ct_eq(cookie.as_bytes(), form.as_bytes()).into()
    })
}

fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .map(str::trim)
        .find_map(|item| item.strip_prefix(&format!("{name}=")))
}

fn html_response(title: &str, body: &str, set_cookie: Option<String>) -> Response {
    let html = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>{}</title><link rel=\"stylesheet\" href=\"/assets/styles.css\"></head><body>{}</body></html>",
        escape(title),
        body
    );
    let mut response = Html(html).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if let Some(cookie) = set_cookie {
        response
            .headers_mut()
            .insert(header::SET_COOKIE, HeaderValue::from_str(&cookie).unwrap());
    }
    response
}

fn error_page(status: StatusCode, title: &str, detail: &str) -> Response {
    let body = format!(
        "<div class=\"error\"><h1>{}</h1><p>{}</p></div>",
        escape(title),
        escape(detail)
    );
    let mut response = html_response(title, &body, None);
    *response.status_mut() = status;
    response
}

fn oauth_error(status: StatusCode, error: &str, description: &str) -> Response {
    no_store(
        (
            status,
            Json(json!({"error": error, "error_description": description})),
        )
            .into_response(),
    )
}

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
        .headers_mut()
        .insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    response
}

fn oauth_hidden(params: &AuthorizeParams) -> String {
    [
        ("response_type", &params.response_type),
        ("client_id", &params.client_id),
        ("redirect_uri", &params.redirect_uri),
        ("scope", &params.scope),
        ("resource", &params.resource),
        ("code_challenge", &params.code_challenge),
        ("code_challenge_method", &params.code_challenge_method),
        ("state", &params.state),
    ]
    .into_iter()
    .map(|(name, value)| {
        format!(
            "<input type=\"hidden\" name=\"{}\" value=\"{}\">",
            name,
            escape(value)
        )
    })
    .collect()
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn time_label(timestamp: i64) -> String {
    DateTime::from_timestamp(timestamp, 0)
        .map(|time: DateTime<Utc>| time.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| "Invalid date".into())
}

fn host_of(public_url: &str) -> String {
    Url::parse(public_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .unwrap_or_else(|| "localhost".into())
}

fn client_ip(headers: &HeaderMap) -> String {
    if let Some(value) = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
    {
        return value.trim().to_owned();
    }
    "unix".into()
}

async fn throttled(state: &AppState, key: &str) -> bool {
    let mut failures = state.failures.lock().await;
    let attempts = failures.entry(key.to_owned()).or_default();
    while attempts
        .front()
        .is_some_and(|at| at.elapsed() > Duration::from_secs(60))
    {
        attempts.pop_front();
    }
    attempts.len() >= 5
}

async fn record_failure(state: &AppState, key: String) {
    state
        .failures
        .lock()
        .await
        .entry(key)
        .or_default()
        .push_back(Instant::now());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_script_checks_for_an_installed_client_before_downloading() {
        let script = connect_script_body("https://connector.example.com");
        let check = script.find("command -v connector-client").unwrap();
        let download = script.find("curl -fsSL").unwrap();
        assert!(check < download);
        assert!(script.contains("\"$client\" --gateway 'https://connector.example.com'"));
    }
}
