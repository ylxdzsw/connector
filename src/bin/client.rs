use std::{
    fs::OpenOptions,
    io::{BufRead, BufReader, Write},
    os::fd::AsRawFd,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use clap::Parser;
use connector::{crypto::normalize_code, mcp::ClientMcp};
use futures::{SinkExt, StreamExt};
use rmcp::{
    ServiceExt,
    model::{ClientJsonRpcMessage, ServerJsonRpcMessage},
};
use tokio::time::sleep;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{Message, client::IntoClientRequest},
};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(version, about = "Connect this Unix host to a Connector gateway")]
struct Args {
    #[arg(long, default_value = "https://connector.ylxdzsw.com")]
    gateway: String,
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
        .with_writer(std::io::stderr)
        .init();
    let args = Args::parse();
    let code = tokio::task::spawn_blocking(read_code).await??;
    let endpoint = websocket_endpoint(&args.gateway)?;
    let mut delay = 1;
    loop {
        let result = tokio::select! {
            result = connect(&endpoint, &code) => result,
            _ = tokio::signal::ctrl_c() => return Ok(()),
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
            _ = tokio::signal::ctrl_c() => return Ok(()),
            _ = sleep(Duration::from_secs(delay)) => {}
        }
        delay = (delay * 2).min(30);
    }
}

fn read_code() -> Result<String> {
    let mut tty = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .context("open /dev/tty to read the connection code")?;
    tty.write_all(b"Connector connection code: ")?;
    tty.flush()?;
    let echo = EchoGuard::disable(tty.as_raw_fd())?;
    let mut code = String::new();
    let mut reader = BufReader::new(tty);
    reader.read_line(&mut code)?;
    drop(echo);
    reader.get_mut().write_all(b"\n")?;
    let code = normalize_code(&code);
    if code.len() != 8 {
        bail!("connection code must contain 8 characters");
    }
    Ok(code)
}

struct EchoGuard {
    fd: i32,
    original: libc::termios,
}

impl EchoGuard {
    fn disable(fd: i32) -> Result<Self> {
        let mut original = std::mem::MaybeUninit::<libc::termios>::uninit();
        if unsafe { libc::tcgetattr(fd, original.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let original = unsafe { original.assume_init() };
        let mut hidden = original;
        hidden.c_lflag &= !libc::ECHO;
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &hidden) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(Self { fd, original })
    }
}

impl Drop for EchoGuard {
    fn drop(&mut self) {
        unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.original) };
    }
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

async fn connect(endpoint: &str, code: &str) -> Result<(), LinkError> {
    let mut request = endpoint
        .into_client_request()
        .map_err(|e| LinkError::Other(e.into()))?;
    request.headers_mut().insert(
        http::header::AUTHORIZATION,
        format!("Bearer {code}").parse().unwrap(),
    );
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
    let (sink, stream) = socket.split();
    let sink = sink.with(|message: ServerJsonRpcMessage| async move {
        Ok::<_, tokio_tungstenite::tungstenite::Error>(Message::Text(
            serde_json::to_string(&message)
                .expect("MCP message serializes")
                .into(),
        ))
    });
    let stream = stream.filter_map(|message| async move {
        match message {
            Ok(Message::Text(text)) => serde_json::from_str::<ClientJsonRpcMessage>(&text)
                .map_err(|error| tracing::warn!(%error, "invalid gateway MCP message"))
                .ok(),
            _ => None,
        }
    });
    let service = ClientMcp
        .serve((Box::pin(sink), Box::pin(stream)))
        .await
        .map_err(|e| LinkError::Other(e.into()))?;
    service
        .waiting()
        .await
        .map_err(|e| LinkError::Other(e.into()))?;
    Ok(())
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
}
