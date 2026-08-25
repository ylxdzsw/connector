use std::{collections::HashMap, sync::Arc};

use base64::{Engine, engine::general_purpose::STANDARD};
use rmcp::{
    ErrorData as McpError, RoleClient, RoleServer, ServerHandler,
    model::{
        CallToolRequestMethod, CallToolRequestParams, CallToolResponse, CallToolResult,
        ContentBlock, Implementation, JsonObject, ListToolsResult, MetaObject,
        PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool, ToolAnnotations,
    },
    service::{Peer, RequestContext},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use tokio::sync::{RwLock, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::{
    execution::{DEFAULT_TIMEOUT, RunArgs, RunOutput, run_bash},
    screenshot,
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

#[derive(Clone)]
pub struct ClientMcp {
    screenshot: Arc<Semaphore>,
}

impl Default for ClientMcp {
    fn default() -> Self {
        Self {
            screenshot: Arc::new(Semaphore::new(1)),
        }
    }
}

const CLIENT_ENVIRONMENT_META: &str = "com.ylxdzsw.connector/client-environment";

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ClientEnvironment {
    pub system: String,
    pub shell: String,
}

impl ClientEnvironment {
    fn current() -> Self {
        Self {
            system: std::env::consts::OS.into(),
            shell: "bash".into(),
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
struct GatewayScreenshotArgs {
    #[schemars(description = "Connected client name")]
    client: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ScreenshotArgs {}

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
}

impl GatewayMcp {
    pub fn new(clients: LiveClients) -> Self {
        Self { clients }
    }

    async fn invoke(&self, request: CallToolRequestParams) -> Result<CallToolResult, McpError> {
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
            gateway_screenshot_tool(),
        ]))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        match name {
            "clients" => Some(clients_tool()),
            "run" => Some(gateway_run_tool()),
            "screenshot" => Some(gateway_screenshot_tool()),
            _ => None,
        }
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        self.invoke(request).await.map(Into::into)
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
            screenshot_tool(false),
        ]))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        match name {
            "run" => Some(run_tool(false)),
            "screenshot" => Some(screenshot_tool(false)),
            _ => None,
        }
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        match request.name.as_ref() {
            "run" => {
                let args: RunArgs = parse_args(request.arguments)?;
                relay_run(&self.peer, args).await.map(Into::into)
            }
            "screenshot" => {
                let _: ScreenshotArgs = parse_args(request.arguments)?;
                relay_screenshot(&self.peer).await.map(Into::into)
            }
            _ => Err(McpError::method_not_found::<CallToolRequestMethod>()),
        }
    }
}

impl ServerHandler for ClientMcp {
    fn get_info(&self) -> ServerInfo {
        let mut info = server_info(
            "connector-client",
            "Run fresh commands with Bash and capture screenshots on this Linux client",
        );
        let mut meta = JsonObject::new();
        meta.insert(
            CLIENT_ENVIRONMENT_META.into(),
            serde_json::to_value(ClientEnvironment::current()).expect("environment serializes"),
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
            screenshot_tool(false),
        ]))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        match name {
            "run" => Some(run_tool(false)),
            "screenshot" => Some(screenshot_tool(false)),
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
            let _permit = self
                .screenshot
                .acquire()
                .await
                .map_err(|error| McpError::internal_error(error.to_string(), None))?;
            let result = match screenshot::capture().await {
                Ok(screenshot) => {
                    tracing::info!(
                        request_id = ?context.id,
                        backend = screenshot.backend,
                        bytes = screenshot.data.len(),
                        "screenshot captured"
                    );
                    CallToolResult::success(vec![ContentBlock::image(
                        STANDARD.encode(screenshot.data),
                        screenshot.mime_type,
                    )])
                }
                Err(error) => {
                    tracing::warn!(request_id = ?context.id, %error, "screenshot failed");
                    tool_error(error)
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
            "executing Bash command"
        );
        let result = match run_bash(args).await {
            Ok(output) => {
                tracing::info!(
                    request_id = ?context.id,
                    stdout = ?output.output,
                    exit_code = output.exit_code,
                    "Bash command completed"
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

async fn relay_screenshot(peer: &Peer<RoleClient>) -> Result<CallToolResult, McpError> {
    match peer
        .call_tool(CallToolRequestParams::new("screenshot"))
        .await
    {
        Ok(result) => Ok(result),
        Err(error) => Ok(tool_error(format!("screenshot unavailable: {error}"))),
    }
}

fn parse_args<T: DeserializeOwned>(arguments: Option<JsonObject>) -> Result<T, McpError> {
    serde_json::from_value(Value::Object(arguments.unwrap_or_default()))
        .map_err(|error| McpError::invalid_params(error.to_string(), None))
}

fn run_result(output: RunOutput) -> CallToolResult {
    CallToolResult::structured(serde_json::to_value(output).expect("RunOutput serializes"))
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
            "List connected clients with their system and shell",
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
        "Run one non-persistent Bash command and return combined output and exit code",
        schema::<RunArgs>(),
    )
    .with_raw_output_schema(schema::<RunOutput>());
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

fn screenshot_annotations() -> ToolAnnotations {
    ToolAnnotations::new()
        .read_only(true)
        .destructive(false)
        .idempotent(true)
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
        assert!(gateway.get_tool("bash").is_none());

        assert!(gateway.get_tool("screenshot").is_some());

        let client = ClientMcp::default();
        assert!(client.get_tool("run").is_some());
        assert!(client.get_tool("screenshot").is_some());
        assert!(client.get_tool("bash").is_none());
        let info = client.get_info();
        let environment: ClientEnvironment =
            serde_json::from_value(info.meta.unwrap().0[CLIENT_ENVIRONMENT_META].clone()).unwrap();
        assert_eq!(environment, ClientEnvironment::current());
    }

    #[test]
    fn client_listing_contains_name_system_and_shell() {
        let value = serde_json::to_value(ClientsOutput {
            clients: vec![ClientSummary {
                name: "build-server".into(),
                system: "linux".into(),
                shell: "bash".into(),
            }],
        })
        .unwrap();
        assert_eq!(
            value,
            json!({"clients": [{
                "name": "build-server",
                "system": "linux",
                "shell": "bash"
            }]})
        );
    }
}
