//! Trace writer, reader, hash chain and blob store.
//!
//! Layout on disk, per run:
//!
//! ```text
//! <run>.jsonl               one Event per line, append-only
//! <run>.blobs/blake3/ab/abcd...   content-addressed payloads over INLINE_LIMIT
//! ```
//!
//! The writer is the single owner of the file; `tether-core` feeds it from a
//! channel so events have a total order and the hash chain is computed in one
//! place. Readers never need the index; `rusqlite` is an optional cache for
//! `inspect` and `diff` over large traces.
//!
//! Spec: `docs/TRACE_FORMAT.md`.

/// Payloads at or below this many bytes are stored inline in the event.
pub const INLINE_LIMIT: usize = 32 * 1024;

/// Hard cap on one serialized event. Anything larger must have spilled to a blob.
pub const MAX_EVENT_BYTES: usize = 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum TraceError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("hash chain broken at seq {seq}: expected {expected}, found {found}")]
    ChainBroken {
        seq: u64,
        expected: String,
        found: String,
    },
    #[error("seq gap: expected {expected}, found {found}")]
    SeqGap { expected: u64, found: u64 },
    #[error("line {line} is not a valid event: {reason}")]
    Malformed { line: u64, reason: String },
    #[error("event {seq} has schema version {v}; this build reads version {supported}")]
    UnsupportedVersion { seq: u64, v: u8, supported: u8 },
    #[error("missing blob {0}")]
    MissingBlob(String),
    #[error("blob {expected} is corrupt: content hashes to {found}")]
    BlobCorrupt { expected: String, found: String },
    #[error("not a blob hash: {0:?}")]
    InvalidBlobHash(String),
    #[error("pack: {0}")]
    Pack(String),
    #[error("event {seq} is {bytes} bytes, over the {limit} byte cap")]
    EventTooLarge {
        seq: u64,
        bytes: usize,
        limit: usize,
    },
}

pub type Result<T> = std::result::Result<T, TraceError>;

pub mod blob;
mod chain;
pub mod diff;
pub mod pack;
pub mod reader;
pub mod resumed;
pub mod writer;

pub use blob::BlobStore;
pub use diff::{diff, TraceDiff};
pub use reader::{verify, TraceReader, VerifyReport};
pub use resumed::effective;
pub use writer::{NewEvent, TraceWriter};
