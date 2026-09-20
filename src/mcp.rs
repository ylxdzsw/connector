use std::{collections::HashMap, path::PathBuf, sync::Arc};

use base64::{Engine, engine::general_purpose::STANDARD};
use rmcp::{
    ErrorData as McpError, RoleClient, RoleServer, ServerHandler,
    model::{
        CallToolRequest, CallToolRequestMethod, CallToolRequestParams, CallToolResponse,
        CallToolResult, CancelledNotificationParam, ClientRequest, ContentBlock, Implementation,
        JsonObject, ListToolsResult, MetaObject, PaginatedRequestParams, ServerCapabilities,
        ServerInfo, ServerResult, Tool, ToolAnnotations,
    },
    service::{Peer, PeerRequestOptions, RequestContext},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use tokio::sync::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;

use crate::{
    apply_patch::{ApplyPatchArgs, ApplyPatchOutput, apply_patch},
    computer::{self, Capability, ComputerArgs, Desktop},
    execution::{DEFAULT_TIMEOUT, RunArgs, RunOutput, run_shell},
    screenshot::Screenshot,
};

#[derive(Clone)]
pub struct LiveClient {
    pub connection_id: String,
    pub peer: Peer<RoleClient>,
    pub environment: ClientEnvironment,
    pub disconnect: CancellationToken,
}

pub type LiveClients = Arc<RwLock<HashMap<String, LiveClient>>>;

#[derive(Clone)]
pub struct GatewayMcp {
    clients: LiveClients,
}

#[derive(Clone)]
pub struct ChannelMcp {
    peer: Peer<RoleClient>,
}

#[derive(Clone, Default)]
pub struct ClientMcp {
    desktop: Arc<Mutex<Desktop>>,
    capability: Capability,
    disconnect: CancellationToken,
}

impl ClientMcp {
    pub async fn for_link(&self, disconnect: CancellationToken) -> Self {
        self.desktop.lock().await.invalidate();
        Self {
            desktop: self.desktop.clone(),
            capability: computer::capability().await,
            disconnect,
        }
    }

    pub async fn finish_desktop(&self) {
        // Wait for an interrupted action to release its inputs before runtime shutdown.
        drop(self.desktop.lock().await);
    }
}

const CLIENT_ENVIRONMENT_META: &str = "com.ylxdzsw.connector/client-environment";

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ClientEnvironment {
    pub system: String,
    pub shell: String,
    #[serde(default)]
    pub computer: Capability,
}

impl ClientEnvironment {
    fn current(computer: Capability) -> Self {
        Self {
            system: std::env::consts::OS.into(),
            shell: current_shell().into(),
            computer,
        }
    }
}

pub fn client_environment(peer: &Peer<RoleClient>) -> Option<ClientEnvironment> {
    peer.peer_info()
        .and_then(|info| {
            info.meta
                .as_ref()
                .and_then(|meta| meta.0.get(CLIENT_ENVIRONMENT_META))
                .cloned()
        })
        .and_then(|value| serde_json::from_value(value).ok())
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GatewayRunArgs {
    #[schemars(description = "Connected client name")]
    client: String,
    #[serde(flatten)]
    #[schemars(flatten)]
    run: RunArgs,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct GatewayApplyPatchArgs {
    #[schemars(description = "Connected client name")]
    client: String,
    #[schemars(description = "Mu/Codex-style patch envelope to apply")]
    patch: String,
    #[schemars(description = "Base directory for relative patch paths")]
    cwd: Option<PathBuf>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct GatewayScreenshotArgs {
    #[schemars(description = "Connected client name")]
    client: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ScreenshotArgs {}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct GatewayComputerArgs {
    #[schemars(description = "Connected client name")]
    client: String,
    #[schemars(length(max = 32))]
    actions: Vec<computer::Action>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct ClientsOutput {
    clients: Vec<ClientSummary>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct ClientSummary {
    #[schemars(description = "Connected client name")]
    name: String,
    #[schemars(description = "Operating system identifier reported by the client")]
    system: String,
    #[schemars(description = "Command shell reported by the client")]
    shell: String,
    computer: Capability,
}

impl GatewayMcp {
    pub fn new(clients: LiveClients) -> Self {
        Self { clients }
    }

    async fn invoke(
        &self,
        request: CallToolRequestParams,
        cancel: CancellationToken,
    ) -> Result<CallToolResult, McpError> {
        match request.name.as_ref() {
            "clients" => {
                let mut clients: Vec<_> = self
                    .clients
                    .read()
                    .await
                    .iter()
                    .map(|(name, client)| ClientSummary {
                        name: name.clone(),
                        system: client.environment.system.clone(),
                        shell: client.environment.shell.clone(),
                        computer: client.environment.computer.clone(),
                    })
                    .collect();
                clients.sort_by(|a, b| a.name.cmp(&b.name));
                Ok(CallToolResult::structured(json!(ClientsOutput { clients })))
            }
            "run" => {
                let args: GatewayRunArgs = parse_args(request.arguments)?;
                let client = self.clients.read().await.get(&args.client).cloned();
                let Some(client) = client else {
                    return Ok(tool_error(format!(
                        "client '{}' is not connected",
                        args.client
                    )));
                };
                relay_run(&client.peer, args.run).await
            }
            "apply_patch" => {
                let args: GatewayApplyPatchArgs = parse_args(request.arguments)?;
                let client = self.clients.read().await.get(&args.client).cloned();
                let Some(client) = client else {
                    return Ok(tool_error(format!(
                        "client '{}' is not connected",
                        args.client
                    )));
                };
                relay_apply_patch(
                    &client.peer,
                    ApplyPatchArgs {
                        patch: args.patch,
                        cwd: args.cwd,
                    },
                )
                .await
            }
            "screenshot" => {
                let args: GatewayScreenshotArgs = parse_args(request.arguments)?;
                let client = self.clients.read().await.get(&args.client).cloned();
                let Some(client) = client else {
                    return Ok(tool_error(format!(
                        "client '{}' is not connected",
                        args.client
                    )));
                };
                relay_screenshot(&client.peer).await
            }
            "computer" => {
                let args: GatewayComputerArgs = parse_args(request.arguments)?;
                let client = self.clients.read().await.get(&args.client).cloned();
                let Some(client) = client else {
                    return Ok(tool_error(format!(
                        "client '{}' is not connected",
                        args.client
                    )));
                };
                relay_computer(
                    &client.peer,
                    ComputerArgs {
                        actions: args.actions,
                    },
                    cancel,
                )
                .await
            }
            _ => Err(McpError::method_not_found::<CallToolRequestMethod>()),
        }
    }
}

impl ChannelMcp {
    pub fn new(peer: Peer<RoleClient>) -> Self {
        Self { peer }
    }
}

impl ServerHandler for GatewayMcp {
    fn get_info(&self) -> ServerInfo {
        server_info("connector-gateway", "Control connected clients")
    }

    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(vec![
            clients_tool(),
            gateway_run_tool(),
            gateway_apply_patch_tool(),
            gateway_screenshot_tool(),
            computer_tool(true),
        ]))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        match name {
            "clients" => Some(clients_tool()),
            "run" => Some(gateway_run_tool()),
            "apply_patch" => Some(gateway_apply_patch_tool()),
            "screenshot" => Some(gateway_screenshot_tool()),
            "computer" => Some(computer_tool(true)),
            _ => None,
        }
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        self.invoke(request, context.ct).await.map(Into::into)
    }
}

impl ServerHandler for ChannelMcp {
    fn get_info(&self) -> ServerInfo {
        server_info("connector-channel", "Control one connected client")
    }

    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(vec![
            run_tool(false),
            apply_patch_tool(false),
            screenshot_tool(false),
            computer_tool(false),
        ]))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        match name {
            "run" => Some(run_tool(false)),
            "apply_patch" => Some(apply_patch_tool(false)),
            "screenshot" => Some(screenshot_tool(false)),
            "computer" => Some(computer_tool(false)),
            _ => None,
        }
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        match request.name.as_ref() {
            "run" => {
                let args: RunArgs = parse_args(request.arguments)?;
                relay_run(&self.peer, args).await.map(Into::into)
            }
            "apply_patch" => {
                let args: ApplyPatchArgs = parse_args(request.arguments)?;
                relay_apply_patch(&self.peer, args).await.map(Into::into)
            }
            "screenshot" => {
                let _: ScreenshotArgs = parse_args(request.arguments)?;
                relay_screenshot(&self.peer).await.map(Into::into)
            }
            "computer" => {
                let args: ComputerArgs = parse_args(request.arguments)?;
                relay_computer(&self.peer, args, context.ct)
                    .await
                    .map(Into::into)
            }
            _ => Err(McpError::method_not_found::<CallToolRequestMethod>()),
        }
    }
}

impl ServerHandler for ClientMcp {
    fn get_info(&self) -> ServerInfo {
        let mut info = server_info(
            "connector-client",
            "Run fresh commands, apply patches, capture screenshots, and control this client's desktop",
        );
        let mut meta = JsonObject::new();
        meta.insert(
            CLIENT_ENVIRONMENT_META.into(),
            serde_json::to_value(ClientEnvironment::current(self.capability.clone()))
                .expect("environment serializes"),
        );
        info.meta = Some(MetaObject(meta));
        info
    }

    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(vec![
            run_tool(false),
            apply_patch_tool(false),
            screenshot_tool(false),
            computer_tool(false),
        ]))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        match name {
            "run" => Some(run_tool(false)),
            "apply_patch" => Some(apply_patch_tool(false)),
            "screenshot" => Some(screenshot_tool(false)),
            "computer" => Some(computer_tool(false)),
            _ => None,
        }
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        if request.name == "screenshot" {
            let _: ScreenshotArgs = parse_args(request.arguments)?;
            let result = match self.desktop.lock().await.capture().await {
                Ok(screenshot) => {
                    tracing::info!(
                        request_id = ?context.id,
                        backend = screenshot.backend,
                        bytes = screenshot.data.len(),
                        "screenshot captured"
                    );
                    screenshot_result(screenshot)
                }
                Err(error) => {
                    tracing::warn!(request_id = ?context.id, %error, "screenshot failed");
                    tool_error(error)
                }
            };
            return Ok(result.into());
        }
        if request.name == "computer" {
            let args: ComputerArgs = parse_args(request.arguments)?;
            tracing::info!(request_id = ?context.id, actions = args.actions.len(), "executing desktop batch");
            let output = tokio::select! {
                output = computer::execute(self.desktop.clone(), args, self.disconnect.child_token()) => output,
                _ = context.ct.cancelled() => Err("desktop batch cancelled; inspect desktop before retrying".into()),
                _ = self.disconnect.cancelled() => Err("client disconnected; batch interrupted; inspect desktop before retrying".into()),
            };
            let result = match output {
                Ok(output) => {
                    tracing::info!(request_id = ?context.id, completed = output.completed, failed = output.error.is_some(), "desktop batch finished");
                    computer_result(output)
                }
                Err(error) => tool_error(error),
            };
            return Ok(result.into());
        }
        if request.name == "apply_patch" {
            let args: ApplyPatchArgs = parse_args(request.arguments)?;
            tracing::info!(
                request_id = ?context.id,
                patch = ?args.patch,
                cwd = ?args.cwd,
                "applying patch"
            );
            let result = match tokio::task::spawn_blocking(move || apply_patch(args)).await {
                Ok(Ok(output)) => {
                    tracing::info!(
                        request_id = ?context.id,
                        output = ?output.output,
                        "patch applied"
                    );
                    apply_patch_result(output)
                }
                Ok(Err(error)) => {
                    tracing::warn!(request_id = ?context.id, %error, "patch failed");
                    tool_error(format!("{error:#}"))
                }
                Err(error) => {
                    tracing::warn!(request_id = ?context.id, %error, "patch task failed");
                    tool_error(format!("patch task failed: {error}"))
                }
            };
            return Ok(result.into());
        }
        if request.name != "run" {
            return Err(McpError::method_not_found::<CallToolRequestMethod>());
        }
        let args: RunArgs = parse_args(request.arguments)?;
        tracing::info!(
            request_id = ?context.id,
            command = ?args.command,
            cwd = ?args.cwd,
            timeout = args.timeout.unwrap_or(DEFAULT_TIMEOUT),
            stdin = ?args.stdin,
            shell = current_shell(),
            "executing shell command"
        );
        let result = match run_shell(args).await {
            Ok(output) => {
                tracing::info!(
                    request_id = ?context.id,
                    stdout = ?output.output,
                    exit_code = output.exit_code,
                    shell = current_shell(),
                    "shell command completed"
                );
                run_result(output)
            }
            Err(error) => {
                tracing::warn!(request_id = ?context.id, %error, "Bash command failed");
                tool_error(error)
            }
        };
        Ok(result.into())
    }
}

async fn relay_run(peer: &Peer<RoleClient>, args: RunArgs) -> Result<CallToolResult, McpError> {
    let arguments = serde_json::to_value(args)
        .map_err(|error| McpError::internal_error(error.to_string(), None))?
        .as_object()
        .cloned()
        .unwrap_or_default();
    match peer
        .call_tool(CallToolRequestParams::new("run").with_arguments(arguments))
        .await
    {
        Ok(result) => Ok(result),
        Err(error) => Ok(tool_error(format!("client became unavailable: {error}"))),
    }
}

async fn relay_apply_patch(
    peer: &Peer<RoleClient>,
    args: ApplyPatchArgs,
) -> Result<CallToolResult, McpError> {
    let arguments = serde_json::to_value(args)
        .map_err(|error| McpError::internal_error(error.to_string(), None))?
        .as_object()
        .cloned()
        .unwrap_or_default();
    match peer
        .call_tool(CallToolRequestParams::new("apply_patch").with_arguments(arguments))
        .await
    {
        Ok(result) => Ok(result),
        Err(error) => Ok(tool_error(format!("client became unavailable: {error}"))),
    }
}

async fn relay_screenshot(peer: &Peer<RoleClient>) -> Result<CallToolResult, McpError> {
    match peer
        .call_tool(CallToolRequestParams::new("screenshot"))
        .await
    {
        Ok(result) => Ok(result),
        Err(error) => Ok(tool_error(format!("screenshot unavailable: {error}"))),
    }
}

async fn relay_computer(
    peer: &Peer<RoleClient>,
    args: ComputerArgs,
    cancel: CancellationToken,
) -> Result<CallToolResult, McpError> {
    let arguments = serde_json::to_value(args)
        .expect("computer arguments serialize")
        .as_object()
        .unwrap()
        .clone();
    let peer = peer.clone();
    let cancel = cancel.child_token();
    let _cancel_on_drop = cancel.clone().drop_guard();
    let result = tokio::spawn(async move {
        let request = ClientRequest::CallToolRequest(CallToolRequest::new(CallToolRequestParams::new("computer").with_arguments(arguments)));
        let handle = peer.send_request_with_option(request, PeerRequestOptions::with_timeout(std::time::Duration::from_secs(90))).await?;
        let id = handle.id.clone();
        tokio::select! {
            result = handle.await_response() => result,
            _ = cancel.cancelled() => {
                let _ = peer.notify_cancelled(CancelledNotificationParam::new(Some(id), Some("controller cancelled desktop batch".into()))).await;
                Err(rmcp::service::ServiceError::TransportClosed)
            }
        }
    }).await;
    match result {
        Ok(Ok(ServerResult::CallToolResult(result))) => Ok(result),
        _ => Ok(tool_error(
            "desktop batch interrupted or client unavailable; input may have executed. Inspect a fresh screenshot before retrying.",
        )),
    }
}

fn parse_args<T: DeserializeOwned>(arguments: Option<JsonObject>) -> Result<T, McpError> {
    serde_json::from_value(Value::Object(arguments.unwrap_or_default()))
        .map_err(|error| McpError::invalid_params(error.to_string(), None))
}

fn run_result(output: RunOutput) -> CallToolResult {
    CallToolResult::structured(serde_json::to_value(output).expect("RunOutput serializes"))
}

fn apply_patch_result(output: ApplyPatchOutput) -> CallToolResult {
    CallToolResult::structured(serde_json::to_value(output).expect("ApplyPatchOutput serializes"))
}

fn screenshot_result(image: Screenshot) -> CallToolResult {
    let metadata = json!({"width": image.width, "height": image.height, "coordinates": "screenshot pixels; origin at top-left"});
    let mut result = CallToolResult::success(vec![
        ContentBlock::text(metadata.to_string()),
        ContentBlock::image(STANDARD.encode(image.data), image.mime_type),
    ]);
    result.structured_content = Some(metadata);
    result
}

fn computer_result(output: computer::Output) -> CallToolResult {
    let screenshot_error = output.screenshot.as_ref().err().cloned();
    let mut result = match output.screenshot {
        Ok(image) => screenshot_result(image),
        Err(error) => tool_error(format!("Final screenshot failed: {error}")),
    };
    let summary = json!({"completed": output.completed, "error": output.error});
    result
        .content
        .insert(0, ContentBlock::text(summary.to_string()));
    if output.error.is_some() {
        result.is_error = Some(true);
    }
    let metadata = result.structured_content.get_or_insert_with(|| json!({}));
    metadata["completed"] = json!(output.completed);
    metadata["error"] = json!(output.error);
    if let Some(error) = screenshot_error {
        metadata["screenshot_error"] = json!(error);
    }
    result
}

fn tool_error(message: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message.into())])
}

fn server_info(name: &str, instructions: &str) -> ServerInfo {
    ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
        .with_server_info(Implementation::new(name, env!("CARGO_PKG_VERSION")))
        .with_instructions(instructions)
}

fn clients_tool() -> Tool {
    secure(
        Tool::new(
            "clients",
            "List connected clients with their system, shell, and detected desktop-input capability",
            empty_schema(),
        )
        .with_raw_output_schema(schema::<ClientsOutput>())
        .with_annotations(
            ToolAnnotations::new()
                .read_only(true)
                .destructive(false)
                .idempotent(true)
                .open_world(false),
        ),
    )
}

fn gateway_run_tool() -> Tool {
    secure(
        Tool::new(
            "run",
            "Run one non-persistent command using a connected client's shell",
            schema::<GatewayRunArgs>(),
        )
        .with_raw_output_schema(schema::<RunOutput>()),
    )
}

fn gateway_apply_patch_tool() -> Tool {
    secure(
        Tool::new(
            "apply_patch",
            "Apply one structured patch to files on a connected client",
            schema::<GatewayApplyPatchArgs>(),
        )
        .with_raw_output_schema(schema::<ApplyPatchOutput>())
        .with_annotations(apply_patch_annotations()),
    )
}

fn gateway_screenshot_tool() -> Tool {
    secure(
        Tool::new(
            "screenshot",
            "Capture the full graphical desktop of a connected client; returns unavailable when the client cannot capture it",
            schema::<GatewayScreenshotArgs>(),
        )
        .with_annotations(screenshot_annotations()),
    )
}

fn run_tool(protected: bool) -> Tool {
    let tool = Tool::new(
        "run",
        "Run one non-persistent command using this client's shell and return combined output and exit code",
        schema::<RunArgs>(),
    )
    .with_raw_output_schema(schema::<RunOutput>());
    if protected { secure(tool) } else { tool }
}

#[cfg(unix)]
fn current_shell() -> &'static str {
    "bash"
}

#[cfg(windows)]
fn current_shell() -> &'static str {
    "pwsh"
}

fn apply_patch_tool(protected: bool) -> Tool {
    let tool = Tool::new(
        "apply_patch",
        "Apply one Mu/Codex-style patch envelope after preflighting all file changes",
        schema::<ApplyPatchArgs>(),
    )
    .with_raw_output_schema(schema::<ApplyPatchOutput>())
    .with_annotations(apply_patch_annotations());
    if protected { secure(tool) } else { tool }
}

fn screenshot_tool(protected: bool) -> Tool {
    let tool = Tool::new(
        "screenshot",
        "Capture the full graphical desktop; returns unavailable when no supported screenshot program can access it",
        empty_schema(),
    )
    .with_annotations(screenshot_annotations());
    if protected { secure(tool) } else { tool }
}

fn computer_tool(protected: bool) -> Tool {
    let tool = Tool::new(
        "computer",
        "Execute up to 32 desktop actions sequentially, then always capture one screenshot. Windows and X11 input only; requires an accessible desktop and input backend. Coordinates are integer pixels in the most recent full screenshot, origin top-left; capture first (actions: [] also captures). Stop on first failure; completed actions are not rolled back. Never replay an interrupted batch without observing. Keys/buttons are released within each action. Use wait for asynchronous UI changes; each wait is at most 5000ms, batch execution budget 30s. Desktop changes by humans or shell commands are not serialized.",
        if protected { schema::<GatewayComputerArgs>() } else { schema::<ComputerArgs>() },
    ).with_annotations(ToolAnnotations::new().read_only(false).destructive(true).idempotent(false).open_world(true));
    if protected { secure(tool) } else { tool }
}

fn screenshot_annotations() -> ToolAnnotations {
    ToolAnnotations::new()
        .read_only(true)
        .destructive(false)
        .idempotent(true)
        .open_world(false)
}

fn apply_patch_annotations() -> ToolAnnotations {
    ToolAnnotations::new()
        .read_only(false)
        .destructive(true)
        .idempotent(false)
        .open_world(false)
}

fn secure(mut tool: Tool) -> Tool {
    tool.meta = Some(
        serde_json::from_value(json!({
            "securitySchemes": [{"type": "oauth2", "scopes": ["control"]}]
        }))
        .expect("security metadata is an object"),
    );
    tool
}

fn schema<T: JsonSchema>() -> Arc<JsonObject> {
    let value = serde_json::to_value(schemars::schema_for!(T)).expect("schema serializes");
    Arc::new(
        value
            .as_object()
            .cloned()
            .expect("root schema is an object"),
    )
}

fn empty_schema() -> Arc<JsonObject> {
    Arc::new(
        serde_json::from_value(json!({"type": "object", "additionalProperties": false})).unwrap(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposes_run_tools_and_client_environment() {
        let gateway = GatewayMcp::new(Arc::new(RwLock::new(HashMap::new())));
        assert!(gateway.get_tool("run").is_some());
        assert!(gateway.get_tool("apply_patch").is_some());
        assert!(gateway.get_tool("bash").is_none());

        assert!(gateway.get_tool("screenshot").is_some());
        assert!(gateway.get_tool("computer").is_some());

        let client = ClientMcp::default();
        assert!(client.get_tool("run").is_some());
        assert!(client.get_tool("apply_patch").is_some());
        assert!(client.get_tool("screenshot").is_some());
        assert!(client.get_tool("computer").is_some());
        assert!(client.get_tool("bash").is_none());
        let info = client.get_info();
        let environment: ClientEnvironment =
            serde_json::from_value(info.meta.unwrap().0[CLIENT_ENVIRONMENT_META].clone()).unwrap();
        assert_eq!(
            environment,
            ClientEnvironment::current(Capability::default())
        );
    }

    #[test]
    fn client_listing_contains_name_system_and_shell() {
        let value = serde_json::to_value(ClientsOutput {
            clients: vec![ClientSummary {
                name: "build-server".into(),
                system: "linux".into(),
                shell: "bash".into(),
                computer: Capability::default(),
            }],
        })
        .unwrap();
        assert_eq!(
            value,
            json!({"clients": [{
                "name": "build-server",
                "system": "linux",
                "shell": "bash",
                "computer": {"available": false, "reason": "client did not advertise desktop input"}
            }]})
        );
    }
}
