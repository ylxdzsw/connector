use std::sync::Arc;

use base64::{Engine, engine::general_purpose::STANDARD};
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler,
    model::{
        CallToolRequestMethod, CallToolRequestParams, CallToolResponse, CallToolResult,
        ContentBlock, Implementation, JsonObject, ListToolsResult, MetaObject,
        PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool, ToolAnnotations,
    },
    service::RequestContext,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::{
    apply_patch::{ApplyPatchArgs, ApplyPatchOutput, apply_patch},
    computer::{self, Capability, ComputerArgs, Desktop},
    execution::{DEFAULT_TIMEOUT, RunArgs, RunOutput, run_shell},
    notes::{self, Notes},
    screenshot::Screenshot,
};

#[derive(Clone, Default)]
pub struct ClientMcp {
    desktop: Arc<Mutex<Desktop>>,
    capability: Capability,
    disconnect: CancellationToken,
    notes: Notes,
}

impl ClientMcp {
    pub async fn new() -> Self {
        let notes = Notes::initialize().await;
        if let Some(error) = &notes.error {
            tracing::warn!(%error, "client notes unavailable");
        }
        Self {
            notes,
            ..Self::default()
        }
    }

    pub async fn for_link(&self, disconnect: CancellationToken) -> Self {
        self.desktop.lock().await.invalidate();
        Self {
            desktop: self.desktop.clone(),
            capability: computer::capability().await,
            disconnect,
            notes: self.notes.refresh().await,
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
    #[serde(default)]
    pub notes: Notes,
}

impl ClientEnvironment {
    fn current(computer: Capability, notes: Notes) -> Self {
        Self {
            system: std::env::consts::OS.into(),
            shell: current_shell().into(),
            computer,
            notes,
        }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct EmptyArgs {}

impl ServerHandler for ClientMcp {
    fn get_info(&self) -> ServerInfo {
        let mut info = server_info(
            "connector-client",
            &format!(
                "Run fresh commands, apply patches, capture screenshots, and control this client's desktop.\n\n{}",
                local_notes_instructions(&self.notes)
            ),
        );
        let mut meta = JsonObject::new();
        meta.insert(
            CLIENT_ENVIRONMENT_META.into(),
            serde_json::to_value(ClientEnvironment::current(
                self.capability.clone(),
                self.notes.clone(),
            ))
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
            run_tool(),
            apply_patch_tool(),
            screenshot_tool(),
            computer_tool(),
            notes_metadata_tool(),
        ]))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        match name {
            "run" => Some(run_tool()),
            "apply_patch" => Some(apply_patch_tool()),
            "screenshot" => Some(screenshot_tool()),
            "computer" => Some(computer_tool()),
            "notes_metadata" => Some(notes_metadata_tool()),
            _ => None,
        }
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        if request.name == "notes_metadata" {
            let _: EmptyArgs = parse_args(request.arguments)?;
            return Ok(CallToolResult::structured(json!(self.notes.refresh().await)).into());
        }
        if request.name == "screenshot" {
            let _: EmptyArgs = parse_args(request.arguments)?;
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
                tracing::warn!(request_id = ?context.id, %error, "shell command failed");
                tool_error(error)
            }
        };
        Ok(result.into())
    }
}

fn local_notes_instructions(notes: &Notes) -> String {
    let location = match &notes.path {
        Some(path) => format!(
            "Persistent Markdown notes on this client (not the gateway): {}.",
            serde_json::to_string(path).expect("path serializes"),
        ),
        None => "This client did not provide a notes path.".into(),
    };
    format!("{location}\n\n{}", notes::INSTRUCTIONS)
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

fn notes_metadata_tool() -> Tool {
    Tool::new(
        "notes_metadata",
        "Report the persistent notes file's absolute path and current line count without reading its contents into the result; used by the gateway's client listing",
        empty_schema(),
    )
    .with_raw_output_schema(schema::<Notes>())
    .with_annotations(screenshot_annotations())
}

fn run_tool() -> Tool {
    Tool::new(
        "run",
        "Run one non-persistent command using this client's shell and return combined output and exit code",
        schema::<RunArgs>(),
    )
    .with_raw_output_schema(schema::<RunOutput>())
}

#[cfg(unix)]
fn current_shell() -> &'static str {
    "bash"
}

#[cfg(windows)]
fn current_shell() -> &'static str {
    "pwsh"
}

fn apply_patch_tool() -> Tool {
    Tool::new(
        "apply_patch",
        "Apply one Mu/Codex-style patch envelope after preflighting all file changes",
        schema::<ApplyPatchArgs>(),
    )
    .with_raw_output_schema(schema::<ApplyPatchOutput>())
    .with_annotations(apply_patch_annotations())
}

fn screenshot_tool() -> Tool {
    Tool::new(
        "screenshot",
        "Capture the full graphical desktop; returns unavailable when no supported screenshot program can access it",
        empty_schema(),
    )
    .with_annotations(screenshot_annotations())
}

fn computer_tool() -> Tool {
    Tool::new(
        "computer",
        "Execute up to 32 desktop actions sequentially, then always capture one screenshot. Windows and X11 input only; requires an accessible desktop and input backend. Coordinates are integer pixels in the most recent full screenshot, origin top-left; capture first (actions: [] also captures). Stop on first failure; completed actions are not rolled back. Never replay an interrupted batch without observing. Keys/buttons are released within each action. Use wait for asynchronous UI changes; each wait is at most 5000ms, batch execution budget 30s. Desktop changes by humans or shell commands are not serialized.",
        schema::<ComputerArgs>(),
    ).with_annotations(ToolAnnotations::new().read_only(false).destructive(true).idempotent(false).open_world(true))
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
        let client = ClientMcp::default();
        assert!(client.get_tool("run").is_some());
        assert!(client.get_tool("apply_patch").is_some());
        assert!(client.get_tool("screenshot").is_some());
        assert!(client.get_tool("computer").is_some());
        assert!(client.get_tool("notes_metadata").is_some());
        assert!(client.get_tool("bash").is_none());
        let info = client.get_info();
        let environment: ClientEnvironment =
            serde_json::from_value(info.meta.unwrap().0[CLIENT_ENVIRONMENT_META].clone()).unwrap();
        assert_eq!(
            environment,
            ClientEnvironment::current(Capability::default(), Notes::default())
        );
    }
}
