use std::{net::SocketAddr, path::PathBuf};

use anyhow::{Context, Result, bail};

#[derive(Clone, Debug)]
pub struct Config {
    pub listen: SocketAddr,
    pub public_url: String,
    pub database: PathBuf,
    pub socket_dir: PathBuf,
    pub subject_header: String,
    pub trust_proxy: bool,
    pub client_binary: PathBuf,
    pub oauth_client_id: Option<String>,
    pub oauth_client_secret: Option<String>,
    pub oauth_redirect_uri: Option<String>,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let public_url = env("CONNECTOR_PUBLIC_URL", "http://127.0.0.1:3000")
            .trim_end_matches('/')
            .to_owned();
        if !(public_url.starts_with("https://")
            || public_url.starts_with("http://localhost")
            || public_url.starts_with("http://127.0.0.1"))
        {
            bail!("CONNECTOR_PUBLIC_URL must use HTTPS (localhost HTTP is allowed)");
        }
        Ok(Self {
            listen: env("CONNECTOR_LISTEN", "127.0.0.1:3000")
                .parse()
                .context("invalid CONNECTOR_LISTEN")?,
            public_url,
            database: env("CONNECTOR_DATABASE", "data/connector.db").into(),
            socket_dir: env("CONNECTOR_SOCKET_DIR", "/run/connector").into(),
            subject_header: env("CONNECTOR_SUBJECT_HEADER", "x-connector-subject"),
            trust_proxy: env("CONNECTOR_TRUST_PROXY", "false")
                .parse()
                .context("invalid CONNECTOR_TRUST_PROXY")?,
            client_binary: env(
                "CONNECTOR_CLIENT_BINARY",
                "target/x86_64-unknown-linux-musl/release/connector-client",
            )
            .into(),
            oauth_client_id: std::env::var("CONNECTOR_OAUTH_CLIENT_ID").ok(),
            oauth_client_secret: std::env::var("CONNECTOR_OAUTH_CLIENT_SECRET").ok(),
            oauth_redirect_uri: std::env::var("CONNECTOR_OAUTH_REDIRECT_URI").ok(),
        })
    }

    pub fn resource(&self) -> String {
        format!("{}/mcp", self.public_url)
    }
}

fn env(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}
