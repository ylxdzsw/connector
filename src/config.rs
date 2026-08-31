use anyhow::{Context, Result};

pub const LISTEN_SOCKET: &str = "/run/connector/.gateway.sock";
pub const PUBLIC_URL: &str = "https://connector.ylxdzsw.com";
pub const RESOURCE_URL: &str = "https://connector.ylxdzsw.com/mcp";
pub const DATABASE: &str = "/var/lib/connector/connector.db";
pub const SOCKET_DIR: &str = "/run/connector";
pub const CLIENT_BINARY: &str = "/opt/connector/connector-client";
pub const WINDOWS_CLIENT_BINARY: &str = "/opt/connector/connector-client-windows-x86_64.exe";
pub const SUBJECT_HEADER: &str = "x-connector-subject";
pub const OAUTH_CLIENT_ID: &str = "chatgpt";
pub const OAUTH_REDIRECT_URI: &str = "https://chatgpt.com/connector_platform_oauth_redirect";

pub struct Config {
    pub oauth_client_secret: String,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            oauth_client_secret: std::env::var("CONNECTOR_OAUTH_CLIENT_SECRET")
                .context("CONNECTOR_OAUTH_CLIENT_SECRET is required")?,
        })
    }
}
