//! Streaming reader with chain verification.

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader, Lines, Read};
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
    check: Checker,
    done: bool,
}

/// Parses lines in order and checks each against the ones before it.
#[derive(Debug)]
struct Checker {
    line_no: u64,
    verify: bool,
    next_seq: u64,
    prev_hash: String,
    run: Option<RunId>,
}

impl TraceReader {
    /// Open a trace and verify it while reading.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Ok(Self {
            lines: BufReader::new(File::open(path)?).lines(),
            check: Checker::new(),
            done: false,
        })
    }

    /// Open a trace without checking seq or the hash chain, to look inside a
    /// damaged one. Lines that do not parse are still errors.
    pub fn open_unverified(path: impl AsRef<Path>) -> Result<Self> {
        let mut reader = Self::open(path)?;
        reader.check.verify = false;
        Ok(reader)
    }

    /// Hash of the last event read: the head of the chain so far.
    pub fn head(&self) -> &str {
        &self.check.prev_hash
    }
}

impl Checker {
    fn new() -> Self {
        Self {
            line_no: 0,
            verify: true,
            next_seq: 0,
            prev_hash: String::new(),
            run: None,
        }
    }

    fn malformed(&self, reason: impl Into<String>) -> TraceError {
        TraceError::Malformed {
            line: self.line_no,
            reason: reason.into(),
        }
    }

    /// The event on the next line of the trace.
    fn read_line(&mut self, line: &str) -> Result<Event> {
        self.line_no += 1;
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
        let result = self.check.read_line(&line);
        if result.is_err() && self.check.verify {
            self.done = true;
        }
        Some(result)
    }
}

/// Reads a trace that may still be being written, verifying it as it goes.
///
/// Each [`poll`](Self::poll) returns the events written since the last one.
/// A last line with no newline yet is an event half written: it is left for
/// the next poll, not reported as damage.
#[derive(Debug)]
pub struct TraceFollower {
    file: File,
    /// Bytes read that do not yet end in a newline.
    pending: Vec<u8>,
    check: Checker,
}

impl TraceFollower {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Ok(Self {
            file: File::open(path)?,
            pending: Vec::new(),
            check: Checker::new(),
        })
    }

    /// The events appended since the last poll, in order. Empty when there
    /// are none yet. An error is final: the trace is damaged from there on.
    pub fn poll(&mut self) -> Result<Vec<Event>> {
        self.file.read_to_end(&mut self.pending)?;
        let Some(end) = self.pending.iter().rposition(|byte| *byte == b'\n') else {
            return Ok(Vec::new());
        };
        let rest = self.pending.split_off(end + 1);
        let complete = std::mem::replace(&mut self.pending, rest);
        let text = std::str::from_utf8(&complete)
            .map_err(|e| self.check.malformed(format!("not UTF-8: {e}")))?;
        text.lines()
            .map(|line| self.check.read_line(line))
            .collect()
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
