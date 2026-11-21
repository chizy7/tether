//! Sandbox: one trait, three backends.
//!
//! `process` (default) is a cwd jail, an env scrub, rlimits and, on Linux, a
//! seccomp filter. It is **not** a security boundary and is never described
//! as one. `docker` (cargo feature `docker`) adds filesystem and network
//! namespaces. `firecracker`
//! adds a separate kernel. The README states what each isolates and what it
//! does not.

use std::path::{Component, Path, PathBuf};

use async_trait::async_trait;

/// What a sandbox is asked to hold its commands to. `None` means no limit.
/// A backend refuses at `start` what it cannot enforce.
#[derive(Debug, Clone)]
pub struct SandboxSpec {
    pub workspace: PathBuf,
    pub network: Network,
    /// CPU share in thousandths of a core, for the container backends.
    pub cpu_millis: Option<u32>,
    /// Total CPU time one command may use, in seconds.
    pub cpu_time_secs: Option<u64>,
    pub memory_bytes: Option<u64>,
    /// File descriptors one command may have open at once.
    pub max_open_files: Option<u64>,
    /// Wall-clock time one command may run, in seconds.
    pub time_limit_secs: Option<u64>,
}

impl SandboxSpec {
    /// A spec for `workspace` with no network and no limits.
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
            network: Network::None,
            cpu_millis: None,
            cpu_time_secs: None,
            memory_bytes: None,
            max_open_files: None,
            time_limit_secs: None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Network {
    None,
    Allowlist(Vec<String>),
    All,
}

#[derive(Debug, Clone)]
pub struct Handle(pub String);

#[derive(Debug, Clone)]
pub struct SnapshotId(pub String);

#[derive(Debug, Clone)]
pub struct Exec {
    pub argv: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    pub stdin: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct ExecResult {
    pub exit_code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub duration_ms: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("unsupported by this backend: {0}")]
    Unsupported(&'static str),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, SandboxError>;

/// Accept only a relative path with no `..`, so it cannot name anything
/// outside the workspace. What a symlink inside the workspace points at is the
/// backend's concern.
pub(crate) fn inside(path: &Path) -> Result<&Path> {
    let stays_inside = path
        .components()
        .all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
    if stays_inside {
        Ok(path)
    } else {
        Err(SandboxError::Other(format!(
            "path leaves the workspace: {}",
            path.display()
        )))
    }
}

#[async_trait]
pub trait Sandbox: Send + Sync {
    /// The backend's name, recorded in `run.created`: `process`, `docker`.
    fn backend(&self) -> &'static str;
    async fn start(&self, spec: &SandboxSpec) -> Result<Handle>;
    async fn exec(&self, h: &Handle, cmd: Exec) -> Result<ExecResult>;
    async fn read(&self, h: &Handle, path: &Path) -> Result<Vec<u8>>;
    async fn write(&self, h: &Handle, path: &Path, data: Vec<u8>) -> Result<()>;
    /// Backends that cannot snapshot return `Unsupported`; resume then
    /// replays writes from the trace into a fresh sandbox.
    async fn snapshot(&self, h: &Handle) -> Result<SnapshotId>;
    async fn stop(&self, h: Handle) -> Result<()>;
}

#[cfg(feature = "process")]
pub mod process;
#[cfg(all(feature = "process", target_os = "linux"))]
mod seccomp;

#[cfg(feature = "docker")]
pub mod docker;

#[cfg(feature = "firecracker")]
pub mod firecracker {
    //! Firecracker microVM backend; snapshot via VM snapshot. v1.0.
}
