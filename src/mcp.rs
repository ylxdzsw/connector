use std::{collections::HashMap, sync::Arc};

use rmcp::{
    ErrorData as McpError, RoleClient, RoleServer, ServerHandler,
    model::{
        CallToolRequestMethod, CallToolRequestParams, CallToolResponse, CallToolResult,
        ContentBlock, Implementation, JsonObject, ListToolsResult, PaginatedRequestParams,
        ServerCapabilities, ServerInfo, Tool, ToolAnnotations,
    },
    service::{Peer, RequestContext},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::execution::{BashArgs, BashOutput, DEFAULT_TIMEOUT, run_bash};

#[derive(Clone)]
pub struct LiveClient {
    pub connection_id: String,
    pub peer: Peer<RoleClient>,
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
pub struct ClientMcp;

#[derive(Debug, Deserialize, JsonSchema)]
struct GatewayBashArgs {
    #[schemars(description = "Connected client name")]
    client: String,
    #[serde(flatten)]
    #[schemars(flatten)]
    bash: BashArgs,
}

#[derive(Debug, Serialize, JsonSchema)]
struct ClientsOutput {
    clients: Vec<String>,
}

impl GatewayMcp {
    pub fn new(clients: LiveClients) -> Self {
        Self { clients }
    }

    async fn invoke(&self, request: CallToolRequestParams) -> Result<CallToolResult, McpError> {
        match request.name.as_ref() {
            "clients" => {
                let mut names: Vec<_> = self.clients.read().await.keys().cloned().collect();
                names.sort();
                Ok(CallToolResult::structured(json!(ClientsOutput {
                    clients: names
                })))
            }
            "bash" => {
                let args: GatewayBashArgs = parse_args(request.arguments)?;
                let client = self.clients.read().await.get(&args.client).cloned();
                let Some(client) = client else {
                    return Ok(tool_error(format!(
                        "client '{}' is not connected",
                        args.client
                    )));
                };
                relay_bash(&client.peer, args.bash).await
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
        server_info("connector-gateway", "Control connected Unix clients")
    }

    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(vec![
            clients_tool(),
            gateway_bash_tool(),
        ]))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        match name {
            "clients" => Some(clients_tool()),
            "bash" => Some(gateway_bash_tool()),
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
        server_info("connector-channel", "Control one connected Unix client")
    }

    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(vec![bash_tool(false)]))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        (name == "bash").then(|| bash_tool(false))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        if request.name != "bash" {
            return Err(McpError::method_not_found::<CallToolRequestMethod>());
        }
        let args: BashArgs = parse_args(request.arguments)?;
        relay_bash(&self.peer, args).await.map(Into::into)
    }
}

impl ServerHandler for ClientMcp {
    fn get_info(&self) -> ServerInfo {
        server_info(
            "connector-client",
            "Run fresh Bash commands on this Unix client",
        )
    }

    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(vec![bash_tool(false)]))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        (name == "bash").then(|| bash_tool(false))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        if request.name != "bash" {
            return Err(McpError::method_not_found::<CallToolRequestMethod>());
        }
        let args: BashArgs = parse_args(request.arguments)?;
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
                bash_result(output)
            }
            Err(error) => {
                tracing::warn!(request_id = ?context.id, %error, "Bash command failed");
                tool_error(error)
            }
        };
        Ok(result.into())
    }
}

async fn relay_bash(peer: &Peer<RoleClient>, args: BashArgs) -> Result<CallToolResult, McpError> {
    let arguments = serde_json::to_value(args)
        .map_err(|error| McpError::internal_error(error.to_string(), None))?
        .as_object()
        .cloned()
        .unwrap_or_default();
    match peer
        .call_tool(CallToolRequestParams::new("bash").with_arguments(arguments))
        .await
    {
        Ok(result) => Ok(result),
        Err(error) => Ok(tool_error(format!("client became unavailable: {error}"))),
    }
}

fn parse_args<T: DeserializeOwned>(arguments: Option<JsonObject>) -> Result<T, McpError> {
    serde_json::from_value(Value::Object(arguments.unwrap_or_default()))
        .map_err(|error| McpError::invalid_params(error.to_string(), None))
}

fn bash_result(output: BashOutput) -> CallToolResult {
    CallToolResult::structured(serde_json::to_value(output).expect("BashOutput serializes"))
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
            "List clients connected right now",
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

fn gateway_bash_tool() -> Tool {
    secure(
        Tool::new(
            "bash",
            "Run one non-persistent Bash command on a connected client",
            schema::<GatewayBashArgs>(),
        )
        .with_raw_output_schema(schema::<BashOutput>()),
    )
}

fn bash_tool(protected: bool) -> Tool {
    let tool = Tool::new(
        "bash",
        "Run one non-persistent Bash command and return combined output and exit code",
        schema::<BashArgs>(),
    )
    .with_raw_output_schema(schema::<BashOutput>());
    if protected { secure(tool) } else { tool }
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
