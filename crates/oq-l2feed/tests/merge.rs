//! Merging two overlapping capture trees loses nothing and duplicates
//! nothing.
//!
//! These are the only two properties that matter. An overlapping
//! upgrade exists to avoid a hole, so a merge that drops a message
//! defeats its purpose; and a merge that keeps a message twice corrupts
//! the sequence a replay depends on, which is worse than the hole would
//! have been.

use std::collections::HashSet;
use std::path::Path;

use oq_l2feed::day::Rotation;
use oq_l2feed::frame::{Kind, Record, decode_all};
use oq_l2feed::stream::{Software, StreamId};
use oq_l2feed::writer::CaptureWriter;

const SECOND: i64 = 1_000_000_000;
const T0: i64 = 1_786_000_000_000_000_000;

/// One market message as both generations would record it: the same
/// bytes and the same exchange timestamp, seen at slightly different
/// local times because two connections never arrive together.
fn message(seq: u64) -> Vec<u8> {
    format!(r#"{{"e":"depthUpdate","u":{seq}}}"#).into_bytes()
}

fn write_tree(root: &Path, first: u64, last: u64, local_skew: i64) {
    let stream = StreamId::new("binance-perp", "BTCUSDT", "depth");
    let mut w = CaptureWriter::new(root, stream, Software::new("test", "unknown"))
        .expect("writer")
        .with_rotation(Rotation::Daily);
    w.append_session_start(T0 + local_skew).expect("start");
    for seq in first..=last {
        let exch = T0 + (seq as i64) * SECOND;
        w.append(&Record {
            kind: Kind::Payload,
            local_ts: exch + local_skew,
            exch_ts: exch,
            payload: message(seq),
        })
        .expect("append");
    }
    w.seal().expect("seal");
}

fn records_of(root: &Path) -> Vec<Record> {
    let path = root
        .join("binance-perp")
        .join("BTCUSDT")
        .join("depth")
        .join("2026-08-06.oqcap");
    let bytes = std::fs::read(path).expect("read merged file");
    let (records, torn) = decode_all(&bytes).expect("decode");
    assert_eq!(torn, 0, "merged output must not end mid-record");
    records
}

#[test]
fn merge_covers_the_union_exactly_once() {
    let base = std::env::temp_dir().join(format!("oq-merge-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let primary = base.join("primary");
    let secondary = base.join("secondary");
    let out = base.join("merged");

    // Primary runs 1..=60. Secondary starts before the primary stops --
    // that is the whole point of an overlap -- and runs 41..=100, with
    // its own local clock offset because it is a different connection.
    write_tree(&primary, 1, 60, 0);
    write_tree(&secondary, 41, 100, 7_000_000);

    let status = std::process::Command::new(env!("CARGO_BIN_EXE_oq-merge"))
        .args([
            "--primary",
            primary.to_str().unwrap(),
            "--secondary",
            secondary.to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
        ])
        .status()
        .expect("run oq-merge");
    assert!(status.success(), "oq-merge should succeed");

    let merged = records_of(&out);
    let payloads: Vec<&Record> = merged.iter().filter(|r| r.kind == Kind::Payload).collect();

    // Nothing lost: every sequence from either tree is present.
    let present: HashSet<Vec<u8>> = payloads.iter().map(|r| r.payload.clone()).collect();
    for seq in 1..=100u64 {
        assert!(
            present.contains(&message(seq)),
            "sequence {seq} is missing from the merge"
        );
    }

    // Nothing duplicated: the overlap 41..=60 appears once, so the count
    // is the union, not the sum.
    assert_eq!(
        payloads.len(),
        100,
        "expected the union of 1..=100, got {} records",
        payloads.len()
    );

    // The primary's copy won in the overlap, so those records carry the
    // primary's clock, not the secondary's. Mixing the two per message
    // would bias any latency measured from this file.
    let overlap: Vec<&&Record> = payloads
        .iter()
        .filter(|r| (41..=60).contains(&((r.exch_ts - T0) / SECOND)))
        .collect();
    assert_eq!(overlap.len(), 20);
    for r in overlap {
        assert_eq!(
            r.local_ts - r.exch_ts,
            0,
            "overlap records must keep the primary's local_ts"
        );
    }

    // Arrival order is preserved rather than re-sorted.
    let seqs: Vec<i64> = payloads.iter().map(|r| (r.exch_ts - T0) / SECOND).collect();
    let mut sorted = seqs.clone();
    sorted.sort_unstable();
    assert_eq!(seqs, sorted, "merged output should be in time order");

    // The manifest describes the merged file, not one of its inputs.
    let manifest_path = out
        .join("binance-perp")
        .join("BTCUSDT")
        .join("depth")
        .join("2026-08-06.manifest.json");
    let text = std::fs::read_to_string(&manifest_path).expect("merged manifest");
    let records_field = text
        .split("\"records\"")
        .nth(1)
        .and_then(|s| {
            s.split(|c: char| !c.is_ascii_digit())
                .find(|s| !s.is_empty())
        })
        .expect("records field");
    assert_eq!(
        records_field.parse::<usize>().unwrap(),
        merged.len(),
        "manifest must count the merged records"
    );

    let _ = std::fs::remove_dir_all(&base);
}
