//! `tether pack` and `tether unpack`, and reading a packed trace in place.

use std::path::Path;

use anyhow::Context;
use tether_trace::pack::is_packed;

pub fn pack(run: &Path, out: Option<&Path>) -> anyhow::Result<()> {
    let out = out.map_or_else(|| run.with_extension("tar"), Path::to_path_buf);
    let report = tether_trace::pack::pack(run, &out)
        .with_context(|| format!("packing {}", run.display()))?;
    println!(
        "packed {} events and {} blobs into {}",
        report.events,
        report.blobs,
        out.display()
    );
    Ok(())
}

pub fn unpack(tar: &Path, dir: Option<&Path>) -> anyhow::Result<()> {
    let beside = tar.parent().filter(|dir| !dir.as_os_str().is_empty());
    let dir = dir.or(beside).unwrap_or(Path::new("."));
    let trace = tether_trace::pack::unpack(tar, dir)
        .with_context(|| format!("unpacking {}", tar.display()))?;
    println!("unpacked and verified {}", trace.display());
    Ok(())
}

/// A trace ready to read: the `.jsonl` it was given, or the one unpacked
/// from a tar into a scratch directory that is removed when this is dropped.
pub struct Opened {
    trace: String,
    /// Held for its `Drop`, which removes the directory.
    _scratch: Option<tempfile::TempDir>,
}

impl Opened {
    pub fn path(&self) -> &str {
        &self.trace
    }
}

/// Open `path` for reading, unpacking it first if it is a packed trace.
///
/// The scratch directory is made fresh, with a random name and for this
/// user only, so nothing another user put in the shared temp directory can
/// stand in for it.
pub fn open(path: &str) -> anyhow::Result<Opened> {
    if !is_packed(Path::new(path)) {
        return Ok(Opened {
            trace: path.to_owned(),
            _scratch: None,
        });
    }
    let scratch = tempfile::Builder::new()
        .prefix("tether-")
        .tempdir()
        .context("making a scratch directory")?;
    let trace = tether_trace::pack::unpack(path, scratch.path())
        .with_context(|| format!("opening {path}"))?;
    Ok(Opened {
        trace: trace.to_string_lossy().into_owned(),
        _scratch: Some(scratch),
    })
}
