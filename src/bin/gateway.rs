use std::{
    os::unix::fs::{FileTypeExt, PermissionsExt},
    path::Path,
};

use anyhow::{Context, Result, bail};
use connector::{
    config::{Config, LISTEN_SOCKET},
    gateway::{AppState, router},
};
use tokio::net::{UnixListener, UnixStream};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "connector=info,tower_http=info".into()),
        )
        .init();
    let config = Config::from_env()?;
    let socket = Path::new(LISTEN_SOCKET);
    let state = AppState::new(config).await?;
    let listener = bind_listener(socket).await?;
    let socket_guard = SocketGuard(socket);
    let shutdown_state = state.clone();
    tracing::info!(socket=%socket.display(), "Connector gateway listening");
    axum::serve(listener, router(state))
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            shutdown_state.shutdown().await;
        })
        .await?;
    drop(socket_guard);
    Ok(())
}

async fn bind_listener(path: &Path) -> Result<UnixListener> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("gateway socket path has no parent"))?;
    let metadata = tokio::fs::metadata(parent)
        .await
        .with_context(|| format!("access gateway socket directory {}", parent.display()))?;
    if !metadata.is_dir() {
        bail!(
            "gateway socket parent is not a directory: {}",
            parent.display()
        );
    }
    if tokio::fs::try_exists(path).await? {
        if UnixStream::connect(path).await.is_ok() {
            bail!("gateway socket is already in use: {}", path.display());
        }
        let metadata = tokio::fs::symlink_metadata(path).await?;
        if !metadata.file_type().is_socket() {
            bail!("gateway socket path is not a socket: {}", path.display());
        }
        tokio::fs::remove_file(path)
            .await
            .with_context(|| format!("remove stale socket {}", path.display()))?;
    }
    let listener = UnixListener::bind(path)
        .with_context(|| format!("bind gateway socket {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))?;
    Ok(listener)
}

struct SocketGuard<'a>(&'a Path);

impl Drop for SocketGuard<'_> {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.0);
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            signal.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = terminate => {} }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[tokio::test]
    async fn replaces_stale_socket_and_rejects_active_listener() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("gateway.sock");
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());

        let listener = bind_listener(&path).await.unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o660
        );
        assert!(bind_listener(&path).await.is_err());

        drop(listener);
        drop(SocketGuard(&path));
        assert!(!path.exists());
    }
}
