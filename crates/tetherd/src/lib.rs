//! `tetherd`: the daemon SDKs talk to. One gRPC service on a Unix socket.
//!
//! Service definition: `proto/tether.proto`. The daemon holds the runs; an
//! agent in any language keeps its own loop and calls `Complete` and
//! `CallTool` in place of its provider SDK and its tool dispatch.
//!
//! The daemon is started on demand and stops by itself: once it has had no
//! run in progress and no event stream open for the idle period, it exits
//! and removes its socket.

use std::path::Path;
use std::time::Duration;

use anyhow::Context;
use hyper_util::rt::TokioIo;
use tokio::net::{UnixListener, UnixStream};
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::{Channel, Endpoint, Server, Uri};

mod service;

/// The generated service and message types.
pub mod proto {
    tonic::include_proto!("tether.v1");
}

use proto::tether_client::TetherClient;
use proto::tether_server::TetherServer;

/// Where the daemon listens when nothing says otherwise.
pub const DEFAULT_SOCKET: &str = ".tether/tetherd.sock";

/// How long the daemon stays up with nothing to do.
pub const DEFAULT_IDLE: Duration = Duration::from_secs(10);

/// Serve on `socket` until the daemon has been idle for `idle`.
///
/// Returns at once, without error, if a daemon is already serving there. A
/// socket file nobody answers is a leftover and is replaced. The socket is
/// readable and writable by its owner only: whoever can reach it can run
/// tools as this user.
pub async fn serve(socket: &Path, idle: Duration) -> anyhow::Result<()> {
    if let Some(dir) = socket.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    if socket.exists() {
        if UnixStream::connect(socket).await.is_ok() {
            tracing::info!("a daemon is already serving {}", socket.display());
            return Ok(());
        }
        std::fs::remove_file(socket)
            .with_context(|| format!("removing the stale socket {}", socket.display()))?;
    }
    let listener =
        UnixListener::bind(socket).with_context(|| format!("binding {}", socket.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))?;
    }

    let daemon = service::Daemon::new();
    let idle_for = daemon.idle_for(idle);
    let served = Server::builder()
        .add_service(TetherServer::new(daemon))
        .serve_with_incoming_shutdown(UnixListenerStream::new(listener), idle_for)
        .await;
    let _ = std::fs::remove_file(socket);
    served.context("serving")
}

/// Connect to the daemon on `socket`.
pub async fn connect(socket: &Path) -> anyhow::Result<TetherClient<Channel>> {
    let path = socket.to_path_buf();
    // The address is never used: the connector below dials the socket.
    let channel = Endpoint::try_from("http://[::]:50051")?
        .connect_with_connector(tower::service_fn(move |_: Uri| {
            let path = path.clone();
            async move { Ok::<_, std::io::Error>(TokioIo::new(UnixStream::connect(path).await?)) }
        }))
        .await
        .with_context(|| format!("connecting to {}", socket.display()))?;
    Ok(TetherClient::new(channel))
}
