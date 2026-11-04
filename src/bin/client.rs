use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Result, bail};
use clap::Parser;
use connector::mcp::ClientMcp;
use futures::{SinkExt, StreamExt, channel::mpsc};
use rmcp::{
    ServiceExt,
    model::{
        CallToolResult, ClientJsonRpcMessage, ContentBlock, ServerJsonRpcMessage, ServerResult,
    },
};
use tokio::time::sleep;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(version, about = "Connect this host to a Connector gateway")]
struct Args {
    /// Gateway base URL (use HTTP only for trusted local testing)
    #[arg(long, default_value = "https://connector.ylxdzsw.com")]
    gateway: String,
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
    let token = match (args.token, args.code) {
        (Some(token), _) => token,
        (_, Some(code)) => code,
        _ => tokio::task::spawn_blocking(read_token).await??,
    };
    let authorization = token_header(&token)?;
    let endpoint = websocket_endpoint(&args.gateway)?;
    let client = ClientMcp::new().await;
    let mut delay = 1;
    loop {
        let result = tokio::select! {
            result = connect(&endpoint, &authorization, &client) => Some(result),
            _ = tokio::signal::ctrl_c() => None,
        };
        let Some(result) = result else {
            client.finish_desktop().await;
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
                client.finish_desktop().await;
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

fn websocket_endpoint(gateway: &str) -> Result<String> {
    let gateway = gateway.trim_end_matches('/');
    let endpoint = if let Some(rest) = gateway.strip_prefix("https://") {
        format!("wss://{rest}/link")
    } else if let Some(rest) = gateway.strip_prefix("http://") {
        format!("ws://{rest}/link")
    } else {
        bail!("gateway URL must begin with http:// or https://")
    };
    Ok(endpoint)
}

#[derive(Debug, thiserror::Error)]
enum LinkError {
    #[error("HTTP {0}")]
    Rejected(http::StatusCode),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

async fn connect(
    endpoint: &str,
    authorization: &http::HeaderValue,
    client: &ClientMcp,
) -> Result<(), LinkError> {
    let cancel = CancellationToken::new();
    let _cancel_on_drop = cancel.clone().drop_guard();
    let client = client.for_link(cancel.clone()).await;
    let mut request = endpoint
        .into_client_request()
        .map_err(|e| LinkError::Other(e.into()))?;
    request
        .headers_mut()
        .insert(http::header::AUTHORIZATION, authorization.clone());
    let (socket, _) = match connect_async(request).await {
        Ok(connection) => connection,
        Err(tokio_tungstenite::tungstenite::Error::Http(response))
            if response.status().is_client_error() =>
        {
            return Err(LinkError::Rejected(response.status()));
        }
        Err(error) => return Err(LinkError::Other(error.into())),
    };
    tracing::info!("connected");
    let (outgoing, outgoing_rx) = mpsc::unbounded::<ServerJsonRpcMessage>();
    let (incoming_tx, incoming) = mpsc::unbounded::<ClientJsonRpcMessage>();
    let mut io_task = tokio::spawn(websocket_io(
        socket,
        outgoing_rx,
        incoming_tx,
        cancel.clone(),
    ));
    let service = client
        .serve_with_ct((outgoing, incoming), cancel.clone())
        .await
        .map_err(|e| LinkError::Other(e.into()))?;
    tokio::select! {
        result = service.waiting() => {
            result.map_err(|e| LinkError::Other(e.into()))?;
        }
        _ = &mut io_task => {}
    }
    cancel.cancel();
    if !io_task.is_finished() {
        let _ = io_task.await;
    }
    Ok(())
}

async fn websocket_io(
    socket: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    mut outgoing: mpsc::UnboundedReceiver<ServerJsonRpcMessage>,
    incoming: mpsc::UnboundedSender<ClientJsonRpcMessage>,
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
                    Err(error) => tracing::warn!(%error, "invalid gateway MCP message"),
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
                Some(message) => {
                    let text = match outgoing_message(message) {
                        Ok(text) => text,
                        Err(error) => {
                            tracing::warn!(%error, "cannot send MCP message");
                            break;
                        }
                    };
                    if write.send(Message::Text(text.into())).await.is_err() { break; }
                },
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

const MAX_MCP_MESSAGE_BYTES: usize = 24 * 1024 * 1024;

struct MessageBuffer(Vec<u8>);

impl std::io::Write for MessageBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > MAX_MCP_MESSAGE_BYTES - self.0.len() {
            return Err(std::io::Error::other("MCP message exceeds 24 MiB"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn serialize_message(message: &ServerJsonRpcMessage) -> Result<String> {
    let mut buffer = MessageBuffer(Vec::new());
    serde_json::to_writer(&mut buffer, message)?;
    Ok(String::from_utf8(buffer.0).expect("JSON is UTF-8"))
}

fn outgoing_message(mut message: ServerJsonRpcMessage) -> Result<String> {
    match serialize_message(&message) {
        Ok(text) => Ok(text),
        Err(error) => {
            if let ServerJsonRpcMessage::Response(response) = &mut message
                && matches!(response.result, ServerResult::CallToolResult(_))
            {
                response.result = ServerResult::CallToolResult(CallToolResult::error(vec![
                    ContentBlock::text(
                        "Tool result exceeds the 24 MiB link limit and was discarded. The operation may have completed; inspect its effects before retrying. It has not been replayed.",
                    ),
                ]));
                serialize_message(&message)
            } else {
                Err(error)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_gateway_url() {
        assert_eq!(
            websocket_endpoint("https://example.com/").unwrap(),
            "wss://example.com/link"
        );
        assert_eq!(
            websocket_endpoint("http://127.0.0.1:3000").unwrap(),
            "ws://127.0.0.1:3000/link"
        );
    }

    #[test]
    fn preserves_opaque_tokens() {
        for flag in ["--token", "--code"] {
            let args = Args::try_parse_from(["connector-client", flag, "oil23456"]).unwrap();
            assert_eq!(args.token.or(args.code).as_deref(), Some("oil23456"));
        }
        assert_eq!(token_header("oil23456").unwrap(), "Bearer oil23456");
        assert!(token_header("bad\ntoken").is_err());
    }

    #[test]
    fn oversized_result_is_a_tool_error_with_the_same_id() {
        let message = ServerJsonRpcMessage::response(
            ServerResult::CallToolResult(CallToolResult::success(vec![ContentBlock::text(
                "\0".repeat(MAX_MCP_MESSAGE_BYTES / 6),
            )])),
            rmcp::model::RequestId::Number(42),
        );
        let text = outgoing_message(message).unwrap();
        assert!(text.len() <= MAX_MCP_MESSAGE_BYTES);
        let response: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(response["id"], 42);
        assert_eq!(response["result"]["isError"], true);
    }
}
