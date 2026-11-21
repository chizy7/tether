//! The `tetherd` binary. Everything it does is in the library.

use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;

#[derive(Parser)]
#[command(
    name = "tetherd",
    version,
    about = "The Tether daemon: gRPC on a Unix socket."
)]
struct Args {
    /// The Unix socket to listen on. Default: `$TETHER_SOCKET`, else
    /// `.tether/tetherd.sock`.
    #[arg(long)]
    socket: Option<PathBuf>,
    /// Exit after this many seconds with no run in progress.
    #[arg(long, default_value_t = tetherd::DEFAULT_IDLE.as_secs())]
    idle_secs: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    let args = Args::parse();
    let socket = args
        .socket
        .or_else(|| std::env::var_os("TETHER_SOCKET").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(tetherd::DEFAULT_SOCKET));
    tetherd::serve(&socket, Duration::from_secs(args.idle_secs)).await
}
