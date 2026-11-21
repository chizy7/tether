use std::fs;
use std::path::{Path, PathBuf};

use serde_json::json;
use tether_trace::pack::{pack, unpack};
use tether_trace::{verify, NewEvent, TraceError, TraceWriter, INLINE_LIMIT};
use tether_types::{EventType, ModelRequest, Payload, RunId, RunState, ToolResult};
use time::OffsetDateTime;

fn dir(test: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(test);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create test dir");
    dir
}

fn new(body: EventType) -> NewEvent {
    NewEvent {
        ts: OffsetDateTime::UNIX_EPOCH,
        step: 1,
        agent: "main".into(),
        parent: None,
        body,
    }
}

/// A trace with two blobs it refers to, a message and a spilled tool
/// result, and one blob in its store that nothing refers to.
fn trace_with_blobs(dir: &Path) -> PathBuf {
    let path = dir.join("run_1.jsonl");
    let mut w = TraceWriter::create(&path, RunId("run_1".into())).expect("create");
    let message = w
        .blobs()
        .put(br#"{"role":"user","content":[]}"#)
        .expect("put");
    w.blobs().put(b"nothing refers to this").expect("put");
    w.append(new(EventType::RunStateChanged {
        state: RunState::Running,
        reason: "start".into(),
    }))
    .expect("append");
    w.append(new(EventType::ModelRequest(ModelRequest {
        provider: "fake".into(),
        model: "fake-1".into(),
        messages: vec![message],
        tools: vec![],
        params: json!({}),
    })))
    .expect("append");
    w.append(new(EventType::ToolResult(ToolResult {
        id: "call_1".into(),
        content: Payload::Inline(json!("x".repeat(INLINE_LIMIT + 1))),
        is_error: false,
        duration_ms: 1,
    })))
    .expect("append");
    path
}

/// Every file under `root`, by path relative to it, with its bytes.
fn files(root: &Path) -> Vec<(String, Vec<u8>)> {
    fn walk(root: &Path, at: &Path, out: &mut Vec<(String, Vec<u8>)>) {
        for entry in fs::read_dir(at).expect("read dir") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                let name = path
                    .strip_prefix(root)
                    .expect("prefix")
                    .to_string_lossy()
                    .into_owned();
                out.push((name, fs::read(&path).expect("read")));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

#[test]
fn a_packed_trace_unpacks_to_the_same_trace() {
    let source = dir("pack_roundtrip");
    let trace = trace_with_blobs(&source);
    let tar = source.join("run_1.tar");

    let report = pack(&trace, &tar).expect("pack");
    assert_eq!((report.events, report.blobs), (3, 2));

    let target = dir("pack_roundtrip_out");
    let unpacked = unpack(&tar, &target).expect("unpack");
    assert_eq!(unpacked, target.join("run_1.jsonl"));
    assert_eq!(verify(&unpacked).expect("verify"), report);

    // The trace byte for byte, and the two blobs it refers to. The blob
    // nothing refers to stayed behind.
    let unpacked_files = files(&target);
    assert_eq!(unpacked_files.len(), 3);
    assert!(unpacked_files[0].0.starts_with("run_1.blobs/blake3/"));
    for (name, bytes) in &unpacked_files {
        assert_eq!(
            *bytes,
            fs::read(source.join(name)).expect("source file"),
            "{name}"
        );
    }
    assert_eq!(files(&source.join("run_1.blobs")).len(), 3);
}

#[test]
fn packing_the_same_trace_twice_gives_the_same_bytes() {
    let source = dir("pack_deterministic");
    let trace = trace_with_blobs(&source);
    pack(&trace, source.join("a.tar")).expect("pack");
    pack(&trace, source.join("b.tar")).expect("pack");
    assert_eq!(
        fs::read(source.join("a.tar")).expect("a"),
        fs::read(source.join("b.tar")).expect("b")
    );
}

#[test]
fn a_trace_that_does_not_verify_is_not_packed() {
    let source = dir("pack_broken");
    let trace = trace_with_blobs(&source);
    let tar = source.join("run_1.tar");

    // An existing file is never overwritten.
    fs::write(&tar, "mine").expect("write");
    assert!(matches!(pack(&trace, &tar), Err(TraceError::Io(_))));
    assert_eq!(fs::read(&tar).expect("read"), b"mine");
    fs::remove_file(&tar).expect("remove");

    // A missing blob.
    let store = source.join("run_1.blobs");
    let blob = files(&store)
        .into_iter()
        .find(|(_, bytes)| bytes.starts_with(b"{\"role\""))
        .expect("blob");
    fs::remove_file(store.join(&blob.0)).expect("remove blob");
    assert!(matches!(
        pack(&trace, &tar),
        Err(TraceError::MissingBlob(_))
    ));
    assert!(!tar.exists());

    // Something that is not a trace by name.
    let other = source.join("notes.txt");
    fs::write(&other, "hi").expect("write");
    assert!(matches!(pack(&other, &tar), Err(TraceError::Pack(_))));
}

/// Write a tar holding `entries` as given, with no checks.
fn tar_of(path: &Path, entries: &[(&str, &[u8])]) {
    let mut tar = tar::Builder::new(fs::File::create(path).expect("create"));
    for (name, data) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        // Set the name on the header directly: the builder would refuse `..`.
        let bytes = name.as_bytes();
        header.as_old_mut().name[..bytes.len()].copy_from_slice(bytes);
        header.set_cksum();
        tar.append(&header, *data).expect("append");
    }
    tar.into_inner().expect("finish");
}

#[test]
fn unpack_refuses_anything_that_is_not_a_packed_trace() {
    let source = dir("unpack_hostile");
    let trace = trace_with_blobs(&source);
    let good = fs::read(&trace).expect("read");
    let hex = "ab".to_owned() + &"0".repeat(62);
    let outside = source.join("escaped.txt");

    type Entries = Vec<(String, Vec<u8>)>;
    let hostile: Vec<(&str, Entries)> = vec![
        (
            "a path that climbs out",
            vec![
                ("run_1.jsonl".into(), good.clone()),
                ("../escaped.txt".into(), b"out".to_vec()),
            ],
        ),
        (
            "a blob path that climbs out",
            vec![
                ("run_1.jsonl".into(), good.clone()),
                (format!("../run_1.blobs/blake3/ab/{hex}"), b"out".to_vec()),
            ],
        ),
        (
            "an absolute path",
            vec![
                ("run_1.jsonl".into(), good.clone()),
                (outside.to_string_lossy().into_owned(), b"out".to_vec()),
            ],
        ),
        (
            "a file that is neither trace nor blob",
            vec![
                ("run_1.jsonl".into(), good.clone()),
                ("run_1.blobs/readme.txt".into(), b"hi".to_vec()),
            ],
        ),
        (
            "a blob filed under another run",
            vec![
                ("run_1.jsonl".into(), good.clone()),
                (format!("run_2.blobs/blake3/ab/{hex}"), b"x".to_vec()),
            ],
        ),
        (
            "two traces",
            vec![
                ("run_1.jsonl".into(), good.clone()),
                ("run_2.jsonl".into(), good.clone()),
            ],
        ),
        (
            "no trace",
            vec![(format!("run_1.blobs/blake3/ab/{hex}"), b"x".to_vec())],
        ),
    ];
    for (what, entries) in hostile {
        let tar = source.join("hostile.tar");
        let borrowed: Vec<(&str, &[u8])> = entries
            .iter()
            .map(|(name, data)| (name.as_str(), data.as_slice()))
            .collect();
        tar_of(&tar, &borrowed);
        let target = source.join("target");
        let _ = fs::remove_dir_all(&target);

        let result = unpack(&tar, &target);
        assert!(
            matches!(result, Err(TraceError::Pack(_))),
            "{what}: {result:?}"
        );
        // Refused before anything was written, inside the target or out.
        assert!(!target.exists() || files(&target).is_empty(), "{what}");
        assert!(!outside.exists(), "{what}");
        assert!(!source.join("escaped.txt").exists(), "{what}");
    }
}

#[test]
fn unpack_leaves_nothing_behind_when_the_trace_does_not_verify() {
    let source = dir("unpack_unverified");
    let trace = trace_with_blobs(&source);
    let tar = source.join("run_1.tar");
    pack(&trace, &tar).expect("pack");

    // The same pack with one blob's content changed.
    let mut archive = tar::Archive::new(fs::File::open(&tar).expect("open"));
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    for entry in archive.entries().expect("entries") {
        let mut entry = entry.expect("entry");
        let name = entry.path().expect("path").to_string_lossy().into_owned();
        let mut data = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut data).expect("read");
        if name.contains(".blobs/") && data.starts_with(b"{\"role\"") {
            data = b"{\"role\":\"user\",\"content\":[\"edited\"]}".to_vec();
        }
        entries.push((name, data));
    }
    let tampered = source.join("tampered.tar");
    let borrowed: Vec<(&str, &[u8])> = entries
        .iter()
        .map(|(name, data)| (name.as_str(), data.as_slice()))
        .collect();
    tar_of(&tampered, &borrowed);

    let target = dir("unpack_unverified_out");
    assert!(matches!(
        unpack(&tampered, &target),
        Err(TraceError::BlobCorrupt { .. })
    ));
    assert!(files(&target).is_empty());

    // A good pack then unpacks into the same place, once.
    unpack(&tar, &target).expect("unpack");
    let again = unpack(&tar, &target);
    assert!(matches!(again, Err(TraceError::Pack(_))), "{again:?}");
}
