//! The manifest that seals a day.
//!
//! A sealed day has to be trustworthy years later, by someone who was
//! not there when it was captured. That means the file must say what it
//! contains, how complete it is, and what it hashes to — otherwise the
//! only way to know whether an archive is intact is to have kept a
//! separate record, and separate records get lost.

use oq_hash::sha256_hex;

use crate::day::Window;
use crate::frame::{Kind, Record};
use crate::stream::{Software, StreamId};

/// Clock offset estimates archived with the data.
///
/// Latency modeling reads `local_ts`, which is only as good as the
/// capture host's clock. Recording the offset makes that dependency
/// visible instead of assumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ClockOffset {
    /// Estimated offset at the start of the day, nanoseconds.
    pub at_start_ns: i64,
    /// Estimated offset at the end of the day, nanoseconds.
    pub at_end_ns: i64,
    /// Largest absolute offset seen during the day.
    pub max_abs_ns: i64,
}

/// What a sealed day contains.
#[derive(Debug, Clone, PartialEq)]
pub struct Manifest {
    /// Format version of the capture file.
    pub format_version: u32,
    /// Venue identifier.
    pub venue: String,
    /// Instrument symbol.
    pub symbol: String,
    /// Stream name.
    pub stream: String,
    /// The window this file covers: a UTC day, or an hour within one.
    pub window: Window,
    /// Records written, control records included.
    pub records: u64,
    /// Uncompressed size in bytes.
    pub bytes_raw: u64,
    /// First and last exchange timestamps seen, if any.
    pub exch_ts_range: Option<(i64, i64)>,
    /// First and last local timestamps seen, if any.
    pub local_ts_range: Option<(i64, i64)>,
    /// Number of feed gaps recorded.
    pub gaps: u64,
    /// Total outage duration across those gaps, nanoseconds.
    pub gap_ns_total: i64,
    /// Clock offset estimates.
    pub clock_offset: ClockOffset,
    /// Capture software version.
    pub capture_version: String,
    /// Commit the capture software was built from.
    pub capture_commit: String,
    /// SHA-256 of the uncompressed file. This is the content identity a
    /// parity baseline pins, so recompressing an archive does not
    /// invalidate every baseline that depends on it.
    pub sha256_raw: String,
}

/// Accumulates manifest fields as records are written.
#[derive(Debug, Clone, Default)]
pub struct ManifestBuilder {
    records: u64,
    bytes_raw: u64,
    exch_first: Option<i64>,
    exch_last: Option<i64>,
    local_first: Option<i64>,
    local_last: Option<i64>,
    gaps: u64,
    gap_ns_total: i64,
    clock_offset: ClockOffset,
    clock_seen: bool,
}

impl ManifestBuilder {
    /// A builder with nothing observed yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Account for one written record.
    pub fn observe(&mut self, record: &Record) {
        self.records += 1;
        self.bytes_raw += record.encoded_len() as u64;

        if record.exch_ts != crate::frame::NO_EXCH_TS {
            self.exch_first.get_or_insert(record.exch_ts);
            self.exch_last = Some(record.exch_ts);
        }
        self.local_first.get_or_insert(record.local_ts);
        self.local_last = Some(record.local_ts);
    }

    /// Account for a feed gap.
    pub fn observe_gap(&mut self, outage_ns: i64) {
        self.gaps += 1;
        self.gap_ns_total += outage_ns;
    }

    /// The `local_ts` of the last record observed, if any.
    ///
    /// Used to measure the silence between one capture session and the
    /// next when a process restarts into a window that already holds
    /// records.
    #[must_use]
    pub fn local_last(&self) -> Option<i64> {
        self.local_last
    }

    /// Account for a clock offset estimate.
    pub fn observe_clock_offset(&mut self, offset_ns: i64) {
        if !self.clock_seen {
            self.clock_offset.at_start_ns = offset_ns;
            self.clock_seen = true;
        }
        self.clock_offset.at_end_ns = offset_ns;
        self.clock_offset.max_abs_ns = self.clock_offset.max_abs_ns.max(offset_ns.abs());
    }

    /// Records observed so far.
    #[must_use]
    pub fn records(&self) -> u64 {
        self.records
    }

    /// Finish, hashing `raw` as the file content.
    #[must_use]
    pub fn build(
        self,
        stream: &StreamId,
        window: Window,
        software: &Software,
        raw: &[u8],
    ) -> Manifest {
        Manifest {
            format_version: crate::FORMAT_VERSION,
            venue: stream.venue.clone(),
            symbol: stream.symbol.clone(),
            stream: stream.stream.clone(),
            window,
            records: self.records,
            bytes_raw: self.bytes_raw,
            exch_ts_range: self.exch_first.zip(self.exch_last),
            local_ts_range: self.local_first.zip(self.local_last),
            gaps: self.gaps,
            gap_ns_total: self.gap_ns_total,
            clock_offset: self.clock_offset,
            capture_version: software.version.clone(),
            capture_commit: software.commit.clone(),
            sha256_raw: sha256_hex(raw),
        }
    }
}

impl Manifest {
    /// Render as JSON.
    ///
    /// Hand-written rather than pulled from a serialization crate: the
    /// schema is fixed by `docs/CAPTURE-FORMAT.md`, and a capture host
    /// should be able to build this crate with nothing else present.
    #[must_use]
    pub fn to_json(&self) -> String {
        // A file whose records carry no exchange timestamp -- a REST
        // poll before its reader knew the field, an hour that held
        // nothing but control records -- has no range to report. Zero
        // said that badly: it is a real instant (1970) and reads as one,
        // so a catalogue built on these manifests dated such a file to
        // the epoch instead of skipping it.
        let (exch_first, exch_last) = self
            .exch_ts_range
            .map_or((None, None), |(a, b)| (Some(a), Some(b)));
        let (local_first, local_last) = self
            .local_ts_range
            .map_or((None, None), |(a, b)| (Some(a), Some(b)));

        let mut out = String::with_capacity(768);
        out.push_str("{\n");
        push_num(&mut out, "format_version", i64::from(self.format_version));
        push_str_field(&mut out, "venue", &self.venue);
        push_str_field(&mut out, "symbol", &self.symbol);
        push_str_field(&mut out, "stream", &self.stream);
        push_str_field(&mut out, "utc_day", &self.window.day.to_string());
        if let Some(hour) = self.window.hour {
            push_num(&mut out, "utc_hour", i64::from(hour));
        }
        push_num(&mut out, "records", self.records as i64);
        push_num(&mut out, "bytes_raw", self.bytes_raw as i64);
        push_opt_num(&mut out, "first_exch_ts", exch_first);
        push_opt_num(&mut out, "last_exch_ts", exch_last);
        push_opt_num(&mut out, "first_local_ts", local_first);
        push_opt_num(&mut out, "last_local_ts", local_last);
        push_num(&mut out, "gaps", self.gaps as i64);
        push_num(&mut out, "gap_ns_total", self.gap_ns_total);
        out.push_str(&format!(
            "  \"clock_offset_ns\": {{\"at_start\": {}, \"at_end\": {}, \"max_abs\": {}}},\n",
            self.clock_offset.at_start_ns,
            self.clock_offset.at_end_ns,
            self.clock_offset.max_abs_ns
        ));
        push_str_field(&mut out, "capture_version", &self.capture_version);
        push_str_field(&mut out, "capture_commit", &self.capture_commit);
        out.push_str(&format!(
            "  \"sha256_raw\": \"{}\"\n}}\n",
            escape(&self.sha256_raw)
        ));
        out
    }
}

fn push_num(out: &mut String, key: &str, value: i64) {
    out.push_str(&format!("  \"{key}\": {value},\n"));
}

/// A number, or `null` where there is nothing to report.
fn push_opt_num(out: &mut String, key: &str, value: Option<i64>) {
    match value {
        Some(v) => push_num(out, key, v),
        None => out.push_str(&format!("  \"{key}\": null,\n")),
    }
}

fn push_str_field(out: &mut String, key: &str, value: &str) {
    out.push_str(&format!("  \"{key}\": \"{}\",\n", escape(value)));
}

/// Minimal JSON string escaping.
fn escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Control record payloads written by the capture process.
pub mod control {
    use super::escape;

    /// `session_start`.
    #[must_use]
    pub fn session_start(version: &str, venue: &str, symbol: &str, stream: &str) -> Vec<u8> {
        format!(
            "{{\"type\":\"session_start\",\"capture_version\":\"{}\",\"venue\":\"{}\",\"symbol\":\"{}\",\"stream\":\"{}\"}}",
            escape(version),
            escape(venue),
            escape(symbol),
            escape(stream)
        )
        .into_bytes()
    }

    /// `clock_offset`.
    #[must_use]
    pub fn clock_offset(offset_ns: i64, dispersion_ns: i64) -> Vec<u8> {
        format!(
            "{{\"type\":\"clock_offset\",\"offset_ns\":{offset_ns},\"dispersion_ns\":{dispersion_ns}}}"
        )
        .into_bytes()
    }

    /// `gap`.
    #[must_use]
    pub fn gap(reason: &str, last_seq: Option<u64>, outage_ns: i64) -> Vec<u8> {
        let seq = match last_seq {
            Some(s) => s.to_string(),
            None => "null".to_string(),
        };
        format!(
            "{{\"type\":\"gap\",\"reason\":\"{}\",\"last_seq\":{seq},\"outage_ns\":{outage_ns}}}",
            escape(reason)
        )
        .into_bytes()
    }

    /// `snapshot`.
    #[must_use]
    pub fn snapshot(seq: u64) -> Vec<u8> {
        format!("{{\"type\":\"snapshot\",\"seq\":{seq}}}").into_bytes()
    }

    /// `session_end`.
    #[must_use]
    pub fn session_end(records: u64, bytes: u64) -> Vec<u8> {
        format!("{{\"type\":\"session_end\",\"records\":{records},\"bytes\":{bytes}}}").into_bytes()
    }
}

/// Whether a control payload is a gap marker, used by readers that must
/// distinguish "nothing happened" from "we were not listening".
#[must_use]
pub fn is_gap(record: &Record) -> bool {
    record.kind == Kind::Control
        && core::str::from_utf8(&record.payload).is_ok_and(|s| s.contains("\"type\":\"gap\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::day::UtcDay;
    use crate::frame::Record;

    #[test]
    fn builder_tracks_ranges_and_gaps() {
        let mut b = ManifestBuilder::new();
        b.observe(&Record::payload(100, 10, b"a".to_vec()));
        b.observe(&Record::payload(200, 20, b"bb".to_vec()));
        b.observe(&Record::control(
            150,
            control::gap("disconnect", Some(7), 5_000),
        ));
        b.observe_gap(5_000);
        b.observe_clock_offset(-400);
        b.observe_clock_offset(1_200);
        b.observe_clock_offset(-30);

        let m = b.build(
            &StreamId::new("venue", "SYM", "depth"),
            Window {
                day: UtcDay(20_000),
                hour: None,
            },
            &Software::new("test 0.1", "abc"),
            b"raw bytes",
        );
        assert_eq!(m.records, 3);
        assert_eq!(
            m.exch_ts_range,
            Some((10, 20)),
            "control records carry no exchange time"
        );
        assert_eq!(m.local_ts_range, Some((100, 150)));
        assert_eq!(m.gaps, 1);
        assert_eq!(m.gap_ns_total, 5_000);
        assert_eq!(m.clock_offset.at_start_ns, -400);
        assert_eq!(m.clock_offset.at_end_ns, -30);
        assert_eq!(m.clock_offset.max_abs_ns, 1_200);
        assert_eq!(m.sha256_raw, oq_hash::sha256_hex(b"raw bytes"));
    }

    #[test]
    fn json_has_the_documented_shape() {
        let m = ManifestBuilder::new().build(
            &StreamId::new("venue", "SYM", "depth"),
            Window {
                day: UtcDay(20_000),
                hour: None,
            },
            &Software::new("test 0.1", "abc"),
            b"",
        );
        let json = m.to_json();
        for key in [
            "format_version",
            "venue",
            "symbol",
            "stream",
            "utc_day",
            "records",
            "bytes_raw",
            "first_exch_ts",
            "gaps",
            "clock_offset_ns",
            "capture_version",
            "sha256_raw",
        ] {
            assert!(
                json.contains(&format!("\"{key}\"")),
                "missing {key} in {json}"
            );
        }
        assert!(json.starts_with('{') && json.trim_end().ends_with('}'));
        assert!(json.contains("\"utc_day\": \"2024-10-04\""), "{json}");
    }

    #[test]
    fn an_absent_range_is_null_rather_than_the_epoch() {
        let m = ManifestBuilder::new().build(
            &StreamId::new("venue", "SYM", "forceOrder"),
            Window {
                day: UtcDay(20_000),
                hour: None,
            },
            &Software::new("test 0.1", "abc"),
            b"",
        );
        let json = m.to_json();
        assert!(
            json.contains("\"first_exch_ts\": null"),
            "a file with no exchange timestamps must not claim 1970: {json}"
        );
        assert!(json.contains("\"last_local_ts\": null"), "{json}");
    }

    #[test]
    fn json_strings_are_escaped() {
        let m = ManifestBuilder::new().build(
            &StreamId::new("ven\"ue", "SY\\M", "de\npth"),
            Window {
                day: UtcDay(0),
                hour: None,
            },
            &Software::new("v", "c"),
            b"",
        );
        let json = m.to_json();
        assert!(json.contains(r#"ven\"ue"#));
        assert!(json.contains(r"SY\\M"));
        assert!(json.contains(r"de\npth"));
    }

    #[test]
    fn gap_records_are_recognizable() {
        let gap = Record::control(1, control::gap("reconnect", None, 12));
        assert!(is_gap(&gap));
        assert!(!is_gap(&Record::control(1, control::snapshot(5))));
        assert!(!is_gap(&Record::payload(1, 2, b"{}".to_vec())));
    }
}
