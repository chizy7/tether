//! Streaming reader with chain verification.

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader, Lines};
use std::path::Path;

use tether_types::{Event, EventType, Payload, RunId, SCHEMA_VERSION};

use crate::blob::blob_ref_hash;
use crate::chain::{chain_hash, split_hash};
use crate::{BlobStore, Result, TraceError};

/// Reads a trace one event at a time.
///
/// By default every event is checked as it is read: schema version, run id,
/// dense `seq` and the hash chain. Iteration ends at the first error.
#[derive(Debug)]
pub struct TraceReader {
    lines: Lines<BufReader<File>>,
    line_no: u64,
    verify: bool,
    next_seq: u64,
    prev_hash: String,
    run: Option<RunId>,
    done: bool,
}

impl TraceReader {
    /// Open a trace and verify it while reading.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Ok(Self {
            lines: BufReader::new(File::open(path)?).lines(),
            line_no: 0,
            verify: true,
            next_seq: 0,
            prev_hash: String::new(),
            run: None,
            done: false,
        })
    }

    /// Open a trace without checking seq or the hash chain, to look inside a
    /// damaged one. Lines that do not parse are still errors.
    pub fn open_unverified(path: impl AsRef<Path>) -> Result<Self> {
        let mut reader = Self::open(path)?;
        reader.verify = false;
        Ok(reader)
    }

    /// Hash of the last event read: the head of the chain so far.
    pub fn head(&self) -> &str {
        &self.prev_hash
    }

    fn malformed(&self, reason: impl Into<String>) -> TraceError {
        TraceError::Malformed {
            line: self.line_no,
            reason: reason.into(),
        }
    }

    fn read_line(&mut self, line: &str) -> Result<Event> {
        let event: Event = serde_json::from_str(line).map_err(|e| self.malformed(e.to_string()))?;
        if !self.verify {
            return Ok(event);
        }

        if event.v != SCHEMA_VERSION {
            return Err(TraceError::UnsupportedVersion {
                seq: event.seq,
                v: event.v,
                supported: SCHEMA_VERSION,
            });
        }
        if event.seq != self.next_seq {
            return Err(TraceError::SeqGap {
                expected: self.next_seq,
                found: event.seq,
            });
        }
        match &self.run {
            Some(run) if *run != event.run => {
                return Err(self.malformed(format!(
                    "run id changed from {:?} to {:?}",
                    run.0, event.run.0
                )));
            }
            Some(_) => {}
            None => self.run = Some(event.run.clone()),
        }

        let (head, written, tail) = split_hash(line)
            .filter(|(_, written, _)| *written == event.hash)
            .ok_or_else(|| self.malformed("envelope fields are not in canonical order"))?;
        let expected = chain_hash(head, &self.prev_hash, tail);
        if expected != written {
            return Err(TraceError::ChainBroken {
                seq: event.seq,
                expected,
                found: written.to_owned(),
            });
        }

        self.prev_hash.clone_from(&event.hash);
        self.next_seq += 1;
        Ok(event)
    }
}

impl Iterator for TraceReader {
    type Item = Result<Event>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let line = match self.lines.next()? {
            Ok(line) => line,
            Err(e) => {
                self.done = true;
                return Some(Err(e.into()));
            }
        };
        self.line_no += 1;
        let result = self.read_line(&line);
        if result.is_err() && self.verify {
            self.done = true;
        }
        Some(result)
    }
}

/// Every blob hash an event refers to.
pub fn blob_refs(body: &EventType) -> Vec<String> {
    let mut refs = Vec::new();
    match body {
        EventType::ModelRequest(req) => refs.extend(req.messages.iter().cloned()),
        EventType::ModelResponse(resp) => {
            refs.push(resp.message.clone());
            refs.extend(resp.raw.clone());
        }
        EventType::ToolResult(result) => {
            if let Payload::Blob { blob, .. } = &result.content {
                refs.push(blob.clone());
            }
        }
        EventType::RunCreated(data)
        | EventType::RunFinished(data)
        | EventType::SandboxExec(data)
        | EventType::SandboxExit(data)
        | EventType::Nondeterminism(data)
        | EventType::Ext { data, .. } => collect_blob_refs(data, &mut refs),
        // Tool call arguments are the agent's own data and never hold refs.
        EventType::ToolCall(_)
        | EventType::RunStateChanged { .. }
        | EventType::RunChild { .. }
        | EventType::PolicyDecision { .. }
        | EventType::PolicyAsk { .. }
        | EventType::PolicyAnswer { .. }
        | EventType::BudgetTick { .. }
        | EventType::Checkpoint { .. } => {}
    }
    refs
}

fn collect_blob_refs(value: &serde_json::Value, refs: &mut Vec<String>) {
    if let Some(hash) = blob_ref_hash(value) {
        refs.push(hash.to_owned());
        return;
    }
    match value {
        serde_json::Value::Array(items) => {
            items.iter().for_each(|v| collect_blob_refs(v, refs));
        }
        serde_json::Value::Object(map) => {
            map.values().for_each(|v| collect_blob_refs(v, refs));
        }
        _ => {}
    }
}

/// What [`verify`] found in a sound trace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyReport {
    pub events: u64,
    /// Distinct blobs referenced, all present and intact.
    pub blobs: u64,
    pub blob_bytes: u64,
    /// Hash of the last event; empty for an empty trace.
    pub head: String,
}

/// Verify a whole trace: the hash chain, then that every referenced blob is
/// in the store beside it and still matches its hash.
pub fn verify(path: impl AsRef<Path>) -> Result<VerifyReport> {
    let path = path.as_ref();
    let store = BlobStore::for_trace(path);
    let mut reader = TraceReader::open(path)?;
    let mut seen = HashSet::new();
    let mut report = VerifyReport {
        events: 0,
        blobs: 0,
        blob_bytes: 0,
        head: String::new(),
    };
    for event in reader.by_ref() {
        let event = event?;
        report.events += 1;
        for hash in blob_refs(&event.body) {
            if seen.insert(hash.clone()) {
                report.blob_bytes += store.check(&hash)?;
                report.blobs += 1;
            }
        }
    }
    report.head = reader.head().to_owned();
    Ok(report)
}
