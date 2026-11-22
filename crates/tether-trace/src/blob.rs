//! Content-addressed blob store. Keyed by `blake3:<hex>`.
//!
//! A blob lives at `<root>/blake3/<first two hex chars>/<full hex>`. Blobs are
//! opaque bytes; a JSON payload is stored as its compact JSON encoding.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use tether_types::Payload;

use crate::{Result, TraceError, INLINE_LIMIT};

pub const HASH_PREFIX: &str = "blake3:";
const HEX_LEN: usize = 64;

/// `blake3:<hex>` of `data`.
pub fn hash_bytes(data: &[u8]) -> String {
    format!("{HASH_PREFIX}{}", blake3::hash(data).to_hex())
}

#[derive(Debug, Clone)]
pub struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The store that sits beside a trace: `<run>.jsonl` gets `<run>.blobs`.
    pub fn for_trace(trace: &Path) -> Self {
        Self::new(trace.with_extension("blobs"))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where a blob lives. Rejects anything that is not `blake3:` plus 64
    /// lowercase hex characters, so a hash read from a trace cannot name a
    /// path outside the store.
    fn path_for(&self, hash: &str) -> Result<PathBuf> {
        let hex = hash
            .strip_prefix(HASH_PREFIX)
            .filter(|hex| {
                hex.len() == HEX_LEN && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
            })
            .ok_or_else(|| TraceError::InvalidBlobHash(hash.to_owned()))?;
        Ok(self.root.join("blake3").join(&hex[..2]).join(hex))
    }

    /// Store `data` and return its hash. Storing the same bytes twice is a no-op.
    pub fn put(&self, data: &[u8]) -> Result<String> {
        let hash = hash_bytes(data);
        let path = self.path_for(&hash)?;
        if path.exists() {
            return Ok(hash);
        }
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        // Write beside the target and rename, so a crash never leaves a
        // partial blob under its final name.
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        let mut file = fs::File::create(&tmp)?;
        file.write_all(data)?;
        file.sync_all()?;
        fs::rename(&tmp, &path)?;
        Ok(hash)
    }

    /// The content stored under `hash`, checked against it.
    ///
    /// A blob store can come from anyone, alongside a trace. A blob is only
    /// ever a plain file whose content hashes to its name: a link to some
    /// other file on this machine, or content that does not match, is
    /// refused, so reading a trace can never pull in a file it does not
    /// contain.
    pub fn get(&self, hash: &str) -> Result<Vec<u8>> {
        let path = self.path_for(hash)?;
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_file() => {}
            Ok(_) => return Err(TraceError::BlobNotAFile(hash.to_owned())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(TraceError::MissingBlob(hash.to_owned()))
            }
            Err(e) => return Err(e.into()),
        }
        let data = match fs::read(&path) {
            Ok(data) => data,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(TraceError::MissingBlob(hash.to_owned()))
            }
            Err(e) => return Err(e.into()),
        };
        let found = hash_bytes(&data);
        if found != hash {
            return Err(TraceError::BlobCorrupt {
                expected: hash.to_owned(),
                found,
            });
        }
        Ok(data)
    }

    pub fn contains(&self, hash: &str) -> bool {
        self.path_for(hash).is_ok_and(|path| path.exists())
    }

    /// Check a blob is present and its content still matches its hash.
    /// Returns its size in bytes.
    pub fn check(&self, hash: &str) -> Result<u64> {
        Ok(self.get(hash)?.len() as u64)
    }

    /// Move an inline payload into the store if it is over [`INLINE_LIMIT`],
    /// or if it has the shape of a blob reference and would be misread as one.
    pub fn spill(&self, payload: Payload) -> Result<Payload> {
        let Payload::Inline(value) = payload else {
            return Ok(payload);
        };
        let encoded = serde_json::to_vec(&value)?;
        if encoded.len() <= INLINE_LIMIT && !is_blob_ref(&value) {
            return Ok(Payload::Inline(value));
        }
        Ok(Payload::Blob {
            blob: self.put(&encoded)?,
            bytes: encoded.len() as u64,
        })
    }
}

/// True for a JSON value that deserializes as `Payload::Blob`.
pub(crate) fn is_blob_ref(value: &serde_json::Value) -> bool {
    blob_ref_hash(value).is_some()
}

pub(crate) fn blob_ref_hash(value: &serde_json::Value) -> Option<&str> {
    let hash = value.get("blob")?.as_str()?;
    value.get("bytes")?.as_u64()?;
    Some(hash)
}
