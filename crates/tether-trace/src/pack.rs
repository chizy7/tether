//! One-file traces: a tar of the JSONL and the blobs it refers to.
//!
//! ```text
//! <run>.tar
//!   <run>.jsonl
//!   <run>.blobs/blake3/<ab>/<full hex>     only blobs the trace refers to
//! ```
//!
//! A plain, uncompressed tar that any tar tool can read. Packing the same
//! trace twice gives the same bytes: entries are in a fixed order with fixed
//! metadata. A trace is verified before it is packed and after it is
//! unpacked, so a pack is never made from, or opened into, a broken trace.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use crate::blob::HASH_PREFIX;
use crate::reader::blob_refs;
use crate::{verify, BlobStore, Result, TraceError, TraceReader, VerifyReport};

fn pack_error(why: impl Into<String>) -> TraceError {
    TraceError::Pack(why.into())
}

/// The trace's file name without `.jsonl`: the name everything in the pack
/// is filed under.
fn stem(trace: &Path) -> Result<String> {
    trace
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(".jsonl"))
        .filter(|stem| !stem.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| pack_error(format!("{} is not a <run>.jsonl", trace.display())))
}

fn append(tar: &mut tar::Builder<File>, name: &str, data: &[u8]) -> Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_size(data.len() as u64);
    header.set_mode(0o644);
    // No times, owners or anything else that would differ between two
    // packs of the same trace.
    header.set_mtime(0);
    header.set_uid(0);
    header.set_gid(0);
    tar.append_data(&mut header, name, data)?;
    Ok(())
}

/// Pack `trace` and the blobs it refers to into `out`, which must not exist.
/// Returns what verifying the trace found.
pub fn pack(trace: impl AsRef<Path>, out: impl AsRef<Path>) -> Result<VerifyReport> {
    let (trace, out) = (trace.as_ref(), out.as_ref());
    let stem = stem(trace)?;
    let report = verify(trace)?;

    let mut hashes = BTreeSet::new();
    for event in TraceReader::open(trace)? {
        hashes.extend(blob_refs(&event?.body));
    }
    let blobs = BlobStore::for_trace(trace);

    let file = OpenOptions::new().write(true).create_new(true).open(out)?;
    let mut tar = tar::Builder::new(file);
    append(&mut tar, &format!("{stem}.jsonl"), &fs::read(trace)?)?;
    for hash in hashes {
        let hex = hash.strip_prefix(HASH_PREFIX).unwrap_or(&hash);
        let name = format!("{stem}.blobs/blake3/{}/{hex}", &hex[..2]);
        append(&mut tar, &name, &blobs.get(&hash)?)?;
    }
    let mut file = tar.into_inner()?;
    file.flush()?;
    file.sync_all()?;
    Ok(report)
}

/// Whether `path` names a blob in the store beside `<stem>.jsonl`.
fn is_blob_path(stem: &str, path: &Path) -> bool {
    // Every part must be a plain name: no `..`, no root, nothing that could
    // take the path somewhere other than under the directory unpacked into.
    let mut parts = Vec::new();
    for part in path.components() {
        match part {
            Component::Normal(name) => match name.to_str() {
                Some(name) => parts.push(name),
                None => return false,
            },
            _ => return false,
        }
    }
    let hex = |text: &str| text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    match parts.as_slice() {
        [store, "blake3", prefix, name] => {
            *store == format!("{stem}.blobs")
                && name.len() == 64
                && hex(name)
                && name.starts_with(prefix)
                && prefix.len() == 2
        }
        _ => false,
    }
}

/// Unpack `tar` into `dir` and verify what came out. Returns the path of the
/// trace. Nothing is overwritten, and if the trace does not verify nothing
/// is left behind.
///
/// Only the two kinds of entry a pack holds are accepted: one `<run>.jsonl`
/// and blobs under `<run>.blobs/blake3/`. Anything else, including a path
/// that would land outside `dir`, is refused before anything is written.
pub fn unpack(tar: impl AsRef<Path>, dir: impl AsRef<Path>) -> Result<PathBuf> {
    let (tar, dir) = (tar.as_ref(), dir.as_ref());
    let mut archive = tar::Archive::new(File::open(tar)?);
    let mut entries: Vec<(PathBuf, Vec<u8>)> = Vec::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        if !entry.header().entry_type().is_file() {
            return Err(pack_error(format!(
                "{} is not a regular file",
                path.display()
            )));
        }
        let mut data = Vec::new();
        entry.read_to_end(&mut data)?;
        entries.push((path, data));
    }

    // The trace first, by name; then every other entry must be one of its blobs.
    let is_trace = |path: &Path| {
        path.components().count() == 1 && path.extension().is_some_and(|ext| ext == "jsonl")
    };
    let mut traces = entries.iter().filter(|(path, _)| is_trace(path));
    let (Some((trace_name, _)), None) = (traces.next(), traces.next()) else {
        return Err(pack_error("it does not hold exactly one <run>.jsonl"));
    };
    let stem = stem(trace_name)?;
    for (path, _) in &entries {
        if !is_trace(path) && !is_blob_path(&stem, path) {
            return Err(pack_error(format!(
                "{} is not part of a packed trace",
                path.display()
            )));
        }
    }

    fs::create_dir_all(dir)?;
    let trace = dir.join(trace_name);
    let store = dir.join(format!("{stem}.blobs"));
    if trace.exists() || store.exists() {
        return Err(pack_error(format!(
            "{} is already unpacked in {}",
            stem,
            dir.display()
        )));
    }
    let written = (|| -> Result<()> {
        for (path, data) in &entries {
            let target = dir.join(path);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(target, data)?;
        }
        verify(&trace)?;
        Ok(())
    })();
    if let Err(e) = written {
        // Leave nothing half unpacked or unverified.
        let _ = fs::remove_file(&trace);
        let _ = fs::remove_dir_all(&store);
        return Err(e);
    }
    Ok(trace)
}

/// True if `path` looks like a packed trace rather than a `.jsonl`.
pub fn is_packed(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext == "tar")
}
