use std::{collections::HashMap, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use clap::Parser;
use connector::{
    computer::{self, ComputerArgs, Desktop},
    execution::{RunArgs, run_shell},
    notes::Notes,
    screenshot::Screenshot,
};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    sync::Mutex,
    task::JoinSet,
    time::{self, Instant, sleep},
};
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{Message, client::IntoClientRequest, protocol::WebSocketConfig},
};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

const PROTOCOL: &str = "sayuri-connector-v1";
const MAX_MESSAGE: usize = 24 * 1024 * 1024;
const MAX_CALLS: usize = 16;

#[derive(Parser)]
#[command(version, about = "Connect this host to a Connector gateway")]
struct Args {
    /// Full connection URL, including /link?name=ID (use ws:// only for local testing)
    #[arg(long, value_name = "WS_URL")]
    url: String,
    /// Opaque, case-sensitive client token
    #[arg(
        long,
        value_name = "TOKEN",
        conflicts_with = "code",
        allow_hyphen_values = true
    )]
    token: Option<String>,
    /// Compatibility alias for --token; preserves the exact value
    #[arg(long, value_name = "TOKEN", allow_hyphen_values = true)]
    code: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("could not install the TLS crypto provider"))?;
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| "connector=info".into()),
        )
        .with_ansi(!cfg!(windows))
        .with_writer(std::io::stderr)
        .init();
    let args = Args::parse();
    let endpoint = websocket_endpoint(&args.url)?;
    let token = match (args.token, args.code) {
        (Some(token), _) => token,
        (_, Some(code)) => code,
        _ => tokio::task::spawn_blocking(read_token).await??,
    };
    let authorization = token_header(&token)?;
    let desktop = Arc::new(Mutex::new(Desktop::default()));
    let notes = Notes::initialize().await;
    let mut delay = 1;
    loop {
        let result = tokio::select! {
            result = connect(endpoint, &authorization, &desktop, &notes) => Some(result),
            _ = tokio::signal::ctrl_c() => None,
        };
        let Some(result) = result else {
            drop(desktop.lock().await);
            return Ok(());
        };
        match result {
            Ok(()) => {
                tracing::warn!("connection closed; reconnecting");
                delay = 1;
            }
            Err(LinkError::Rejected(status)) => bail!("gateway rejected the connection ({status})"),
            Err(LinkError::Other(error)) => tracing::warn!(%error, "could not connect"),
        }
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                drop(desktop.lock().await);
                return Ok(());
            },
            _ = sleep(Duration::from_secs(delay)) => {}
        }
        delay = (delay * 2).min(30);
    }
}

fn read_token() -> Result<String> {
    Ok(rpassword::prompt_password("Connector token: ")?)
}

fn token_header(token: &str) -> Result<http::HeaderValue> {
    if token.is_empty() || token.bytes().any(|b| b.is_ascii_whitespace()) {
        bail!("token must be nonempty and contain no whitespace");
    }
    let mut header = http::HeaderValue::from_str(&format!("Bearer {token}"))
        .map_err(|_| anyhow::anyhow!("token is not a valid HTTP bearer credential"))?;
    header.set_sensitive(true);
    Ok(header)
}

fn websocket_endpoint(url: &str) -> Result<&str> {
    let endpoint: http::Uri = url.parse().context("Invalid WebSocket URL")?;
    if !matches!(endpoint.scheme_str(), Some("wss" | "ws")) || endpoint.host().is_none() {
        bail!("Connection URL must begin with wss:// or ws://");
    }
    Ok(url)
}

#[derive(Debug, thiserror::Error)]
enum LinkError {
    #[error("HTTP {0}")]
    Rejected(http::StatusCode),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Incoming {
    Call {
        id: String,
        #[serde(flatten)]
        operation: Operation,
    },
    Cancel {
        id: String,
    },
}

#[derive(Deserialize)]
#[serde(tag = "op", content = "args", rename_all = "snake_case")]
enum Operation {
    Exec(RunArgs),
    Desktop(ComputerArgs),
}

fn image_result(image: Screenshot) -> Value {
    json!({"data": STANDARD.encode(image.data), "mime_type": image.mime_type,
        "width": image.width, "height": image.height})
}

async fn execute(
    operation: Operation,
    desktop: Arc<Mutex<Desktop>>,
    cancel: CancellationToken,
) -> Result<Value, String> {
    match operation {
        Operation::Exec(args) => Ok(json!(run_shell(args).await?)),
        Operation::Desktop(args) if args.actions.is_empty() => {
            let image = desktop.lock().await.capture().await?;
            Ok(json!({"completed": 0, "error": null, "image": image_result(image)}))
        }
        Operation::Desktop(args) => {
            let output = computer::execute(desktop, args, cancel).await?;
            let (image, screenshot_error) = match output.screenshot {
                Ok(image) => (Some(image_result(image)), None),
                Err(error) => (None, Some(error)),
            };
            Ok(json!({"completed": output.completed, "error": output.error,
                "screenshot_error": screenshot_error, "image": image}))
        }
    }
}

async fn connect(
    endpoint: &str,
    authorization: &http::HeaderValue,
    desktop: &Arc<Mutex<Desktop>>,
    notes: &Notes,
) -> Result<(), LinkError> {
    let mut request = endpoint
        .into_client_request()
        .map_err(anyhow::Error::from)?;
    request
        .headers_mut()
        .insert(http::header::AUTHORIZATION, authorization.clone());
    request.headers_mut().insert(
        http::header::SEC_WEBSOCKET_PROTOCOL,
        http::HeaderValue::from_static(PROTOCOL),
    );
    let config = WebSocketConfig::default().max_message_size(Some(MAX_MESSAGE));
    let (mut socket, _) = match connect_async_with_config(request, Some(config), false).await {
        Ok(connection) => connection,
        Err(tokio_tungstenite::tungstenite::Error::Http(response))
            if response.status().is_client_error() =>
        {
            return Err(LinkError::Rejected(response.status()));
        }
        Err(error) => return Err(anyhow::Error::from(error).into()),
    };
    desktop.lock().await.invalidate();
    let capability = computer::capability().await;
    let shell = if cfg!(windows) { "pwsh" } else { "bash" };
    let ready = json!({"type": "ready", "system": std::env::consts::OS, "shell": shell,
        "desktop": capability, "notes": notes.path});
    socket
        .send(Message::Text(ready.to_string().into()))
        .await
        .map_err(anyhow::Error::from)?;
    tracing::info!("connected");

    let mut calls = HashMap::<String, CancellationToken>::new();
    let mut tasks = JoinSet::new();
    let mut ping = time::interval(Duration::from_secs(15));
    let mut last_pong = Instant::now();
    loop {
        let send = tokio::select! {
            incoming = socket.next() => match incoming {
                Some(Ok(Message::Text(text))) => match serde_json::from_str::<Incoming>(&text) {
                    Ok(Incoming::Call { id, operation }) => {
                        if calls.contains_key(&id) { break; }
                        if calls.len() >= MAX_CALLS {
                            Some(Message::Text(reply(&id, Err("Native client is busy".into()))?.into()))
                        } else {
                            let cancel = CancellationToken::new();
                            calls.insert(id.clone(), cancel.clone());
                            let desktop = desktop.clone();
                            tasks.spawn(async move {
                                let result = tokio::select! {
                                    _ = cancel.cancelled() => Err("Request cancelled; inspect state before retrying".into()),
                                    result = execute(operation, desktop, cancel.clone()) => result,
                                };
                                (id, result)
                            });
                            None
                        }
                    },
                    Ok(Incoming::Cancel { id }) => {
                        if let Some(cancel) = calls.get(&id) { cancel.cancel(); }
                        None
                    },
                    Err(_) => break,
                },
                Some(Ok(Message::Pong(_))) => { last_pong = Instant::now(); None },
                Some(Ok(Message::Ping(_))) => { socket.flush().await.map_err(anyhow::Error::from)?; None },
                _ => break,
            },
            completed = tasks.join_next(), if !tasks.is_empty() => {
                let (id, result) = completed.expect("task pending").map_err(anyhow::Error::from)?;
                calls.remove(&id);
                Some(Message::Text(reply(&id, result)?.into()))
            },
            _ = ping.tick() => {
                if last_pong.elapsed() > Duration::from_secs(45) { break; }
                Some(Message::Ping(Vec::new().into()))
            },
        };
        if let Some(message) = send {
            time::timeout(Duration::from_secs(15), socket.send(message))
                .await
                .map_err(anyhow::Error::from)?
                .map_err(anyhow::Error::from)?;
        }
    }
    Ok(())
}

struct MessageBuffer(Vec<u8>);

impl std::io::Write for MessageBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_MESSAGE - self.0.len() {
            return Err(std::io::Error::other("Link message exceeds 24 MiB"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn reply(id: &str, result: Result<Value, String>) -> Result<String> {
    let reply = match result {
        Ok(result) => json!({"type": "result", "id": id, "result": result}),
        Err(error) => json!({"type": "result", "id": id, "error": error}),
    };
    let mut buffer = MessageBuffer(Vec::new());
    if serde_json::to_writer(&mut buffer, &reply).is_err() {
        return Ok(json!({"type": "result", "id": id,
            "error": "Result exceeds 24 MiB; inspect state before retrying"})
        .to_string());
    }
    Ok(String::from_utf8(buffer.0).expect("JSON is UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_named_websocket_url() {
        assert_eq!(
            websocket_endpoint("wss://example.com/link?name=My-PC").unwrap(),
            "wss://example.com/link?name=My-PC"
        );
        assert!(websocket_endpoint("https://example.com/").is_err());
    }

    #[test]
    fn preserves_opaque_tokens() {
        for flag in ["--token", "--code"] {
            let args = Args::try_parse_from([
                "connector-client",
                "--url",
                "wss://example.com/link?name=fixture",
                flag,
                "oil23456",
            ])
            .unwrap();
            assert_eq!(args.token.or(args.code).as_deref(), Some("oil23456"));
        }
        assert_eq!(token_header("oil23456").unwrap(), "Bearer oil23456");
        assert!(token_header("bad\ntoken").is_err());
    }

    #[test]
    fn oversized_result_is_an_error_with_the_same_id() {
        let text = reply("42", Ok(json!("\0".repeat(MAX_MESSAGE / 6)))).unwrap();
        assert!(text.len() <= MAX_MESSAGE);
        let response: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(response["id"], "42");
        assert!(response["error"].is_string());
    }
}
