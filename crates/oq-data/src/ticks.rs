//! Getting ticks into the engine.
//!
//! A backtest reads a very large number of records and does almost
//! nothing per record, so the cost is dominated by how many times each
//! one is copied and how much per-record work the decoder does. The
//! format here is fixed-width and little-endian: a record is decoded by
//! reading eight integers at known offsets, with no allocation and no
//! branching on schema.
//!
//! It exists because the archive format and the *read* format have
//! different jobs. Captured archives are verbatim venue bytes, and
//! columnar files are the right shape for analysis; neither is the
//! right shape for a replay loop that wants the next tick with as few
//! instructions as possible. Conversion happens once, at import.
//!
//! Both timestamps travel with every record. Dropping the arrival
//! timestamp to save eight bytes would make the file unusable for
//! latency-aware simulation later, and that decision cannot be undone
//! after the fact — which is why it is not offered as an option.

use crate::Error;
use oq_engine::Tick;
use oq_hash::crc32;
use oq_types::{PriceTicks, Stamp};

/// `OQTK`, little-endian.
pub const MAGIC: u32 = u32::from_le_bytes(*b"OQTK");
/// Bytes of file header.
pub const HEADER_LEN: usize = 32;
/// Bytes per record.
pub const RECORD_LEN: usize = 64;
/// The format version this build writes.
pub const VERSION: u16 = 2;

/// A tick file header.
///
/// ```text
/// offset size field
///      0    4 magic 'O','Q','T','K'
///      4    2 version
///      6    2 reserved
///      8    8 record count
///     16    8 instrument id (opaque to this crate)
///     24    4 crc32 of the record region
///     28    4 reserved
/// ```
///
/// A record is eight little-endian `i64`s: exchange time, arrival time,
/// last, high, low, bid, ask, volume.
///
/// The record count and checksum are in the header rather than a
/// trailer so a reader can validate before allocating, and so a
/// truncated file is detected as truncated rather than read as a
/// shorter one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub count: u64,
    pub instrument: u64,
    pub checksum: u32,
}

/// Encode ticks into the tick-file format.
#[must_use]
pub fn encode(instrument: u64, ticks: &[Tick]) -> Vec<u8> {
    let mut body = Vec::with_capacity(ticks.len() * RECORD_LEN);
    for t in ticks {
        body.extend_from_slice(&t.stamp.exch.0.to_le_bytes());
        body.extend_from_slice(&t.stamp.local.0.to_le_bytes());
        body.extend_from_slice(&t.last.0.to_le_bytes());
        body.extend_from_slice(&t.high.0.to_le_bytes());
        body.extend_from_slice(&t.low.0.to_le_bytes());
        body.extend_from_slice(&t.bid.0.to_le_bytes());
        body.extend_from_slice(&t.ask.0.to_le_bytes());
        body.extend_from_slice(&t.volume.0.to_le_bytes());
    }
    let checksum = crc32(&body);

    let mut out = Vec::with_capacity(HEADER_LEN + body.len());
    out.extend_from_slice(&MAGIC.to_le_bytes());
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&(ticks.len() as u64).to_le_bytes());
    out.extend_from_slice(&instrument.to_le_bytes());
    out.extend_from_slice(&checksum.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&body);
    out
}

/// Read the header without decoding records.
///
/// # Errors
/// [`Error::Truncated`], [`Error::BadMagic`], or [`Error::UnknownVersion`].
pub fn read_header(bytes: &[u8]) -> Result<Header, Error> {
    if bytes.len() < HEADER_LEN {
        return Err(Error::Truncated {
            needed: HEADER_LEN,
            available: bytes.len(),
        });
    }
    let magic = u32::from_le_bytes(bytes[0..4].try_into().expect("4 bytes"));
    if magic != MAGIC {
        return Err(Error::BadMagic { found: magic });
    }
    let version = u16::from_le_bytes(bytes[4..6].try_into().expect("2 bytes"));
    if version != VERSION {
        return Err(Error::UnknownVersion { found: version });
    }
    Ok(Header {
        count: u64::from_le_bytes(bytes[8..16].try_into().expect("8 bytes")),
        instrument: u64::from_le_bytes(bytes[16..24].try_into().expect("8 bytes")),
        checksum: u32::from_le_bytes(bytes[24..28].try_into().expect("4 bytes")),
    })
}

/// Decode a tick file.
///
/// Verifies the checksum before returning: a backtest that silently
/// consumed corrupted prices would produce a plausible, wrong answer,
/// and there is no later stage at which that becomes visible.
///
/// # Errors
/// [`Error::Truncated`] when the file is shorter than its header
/// claims, [`Error::ChecksumMismatch`] when the records do not verify.
pub fn decode(bytes: &[u8]) -> Result<(Header, Vec<Tick>), Error> {
    let header = read_header(bytes)?;
    let expected_len = HEADER_LEN + header.count as usize * RECORD_LEN;
    if bytes.len() < expected_len {
        return Err(Error::Truncated {
            needed: expected_len,
            available: bytes.len(),
        });
    }
    let body = &bytes[HEADER_LEN..expected_len];
    let computed = crc32(body);
    if computed != header.checksum {
        return Err(Error::ChecksumMismatch {
            expected: header.checksum,
            computed,
        });
    }

    let mut ticks = Vec::with_capacity(header.count as usize);
    for chunk in body.chunks_exact(RECORD_LEN) {
        let at =
            |i: usize| i64::from_le_bytes(chunk[i * 8..i * 8 + 8].try_into().expect("8 bytes"));
        ticks.push(Tick {
            stamp: Stamp::new(at(0), at(1)),
            last: PriceTicks(at(2)),
            high: PriceTicks(at(3)),
            low: PriceTicks(at(4)),
            bid: PriceTicks(at(5)),
            ask: PriceTicks(at(6)),
            volume: oq_types::QtyLots(at(7)),
        });
    }
    Ok((header, ticks))
}

/// Read a tick file without holding its bytes and its ticks at once.
///
/// A multi-year window is tens of gigabytes on disk and several in
/// memory as decoded ticks. Reading the whole file into a buffer and
/// *then* decoding needs both at the same moment, which is where a long
/// run dies — and it dies at the end of a slow read, having wasted the
/// time. This decodes as it reads, so the peak is the ticks alone.
///
/// # Errors
/// I/O failures, or anything [`decode`] reports about the header.
/// Checksums are verified over the whole record region as it streams.
pub fn read_file(path: &std::path::Path) -> Result<(Header, Vec<Tick>), Error> {
    use std::io::Read;

    let mut file = std::fs::File::open(path).map_err(|_| Error::Truncated {
        needed: HEADER_LEN,
        available: 0,
    })?;
    let mut header_bytes = [0u8; HEADER_LEN];
    file.read_exact(&mut header_bytes)
        .map_err(|_| Error::Truncated {
            needed: HEADER_LEN,
            available: 0,
        })?;
    let header = read_header(&header_bytes)?;

    let mut ticks = Vec::with_capacity(header.count as usize);
    let mut crc = crc32_streaming::Accumulator::new();
    // A block of whole records, so a decode never straddles a read.
    const RECORDS_PER_BLOCK: usize = 8192;
    let mut buf = vec![0u8; RECORDS_PER_BLOCK * RECORD_LEN];
    let mut remaining = header.count as usize;

    while remaining > 0 {
        let want = remaining.min(RECORDS_PER_BLOCK) * RECORD_LEN;
        let block = &mut buf[..want];
        file.read_exact(block).map_err(|_| Error::Truncated {
            needed: HEADER_LEN + header.count as usize * RECORD_LEN,
            available: HEADER_LEN + (header.count as usize - remaining) * RECORD_LEN,
        })?;
        crc.update(block);
        for chunk in block.chunks_exact(RECORD_LEN) {
            let at =
                |i: usize| i64::from_le_bytes(chunk[i * 8..i * 8 + 8].try_into().expect("8 bytes"));
            ticks.push(Tick {
                stamp: Stamp::new(at(0), at(1)),
                last: PriceTicks(at(2)),
                high: PriceTicks(at(3)),
                low: PriceTicks(at(4)),
                bid: PriceTicks(at(5)),
                ask: PriceTicks(at(6)),
                volume: oq_types::QtyLots(at(7)),
            });
        }
        remaining -= want / RECORD_LEN;
    }

    let computed = crc.finish();
    if computed != header.checksum {
        return Err(Error::ChecksumMismatch {
            expected: header.checksum,
            computed,
        });
    }
    Ok((header, ticks))
}

/// Incremental CRC-32 over blocks.
///
/// The one-shot function in `oq-hash` needs the whole region at once,
/// which is exactly what streaming exists to avoid. This accumulates
/// the same checksum block by block, and a test pins it against the
/// one-shot result at many block boundaries — the seam where such an
/// implementation breaks.
mod crc32_streaming {
    pub struct Accumulator {
        state: u32,
    }

    const POLYNOMIAL: u32 = 0xEDB8_8320;

    const fn table() -> [u32; 256] {
        let mut t = [0u32; 256];
        let mut i = 0;
        while i < 256 {
            let mut crc = i as u32;
            let mut bit = 0;
            while bit < 8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ POLYNOMIAL
                } else {
                    crc >> 1
                };
                bit += 1;
            }
            t[i] = crc;
            i += 1;
        }
        t
    }

    static TABLE: [u32; 256] = table();

    impl Accumulator {
        pub const fn new() -> Self {
            Self { state: 0xFFFF_FFFF }
        }

        pub fn update(&mut self, bytes: &[u8]) {
            let mut crc = self.state;
            for &b in bytes {
                let idx = ((crc ^ u32::from(b)) & 0xFF) as usize;
                crc = (crc >> 8) ^ TABLE[idx];
            }
            self.state = crc;
        }

        pub const fn finish(self) -> u32 {
            self.state ^ 0xFFFF_FFFF
        }
    }
}

/// A tick stream with the checks a replay depends on.
///
/// Ordering is verified rather than assumed. Out-of-order ticks break
/// the gap-fill logic in a way that produces fills instead of an error,
/// so a stream that silently reorders would show up as profit rather
/// than as a fault.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickStream {
    ticks: Vec<Tick>,
    instrument: u64,
}

impl TickStream {
    /// Wrap ticks, verifying they are non-decreasing in exchange time.
    ///
    /// # Errors
    /// [`Error::OutOfOrder`] at the first offending index.
    pub fn new(instrument: u64, ticks: Vec<Tick>) -> Result<Self, Error> {
        for (i, w) in ticks.windows(2).enumerate() {
            if w[1].stamp.exch < w[0].stamp.exch {
                return Err(Error::OutOfOrder {
                    index: i + 1,
                    previous: w[0].stamp.exch.0,
                    found: w[1].stamp.exch.0,
                });
            }
        }
        Ok(Self { ticks, instrument })
    }

    /// Load from encoded bytes.
    ///
    /// # Errors
    /// Anything [`decode`] or [`TickStream::new`] reports.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        let (header, ticks) = decode(bytes)?;
        Self::new(header.instrument, ticks)
    }

    /// Load from a file without holding its bytes and its ticks at once.
    ///
    /// The right entry point for anything larger than a few hundred
    /// megabytes; see [`read_file`].
    ///
    /// # Errors
    /// Anything [`read_file`] or [`TickStream::new`] reports.
    pub fn from_file(path: impl AsRef<std::path::Path>) -> Result<Self, Error> {
        let (header, ticks) = read_file(path.as_ref())?;
        Self::new(header.instrument, ticks)
    }

    #[must_use]
    pub fn ticks(&self) -> &[Tick] {
        &self.ticks
    }

    #[must_use]
    pub const fn instrument(&self) -> u64 {
        self.instrument
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.ticks.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ticks.is_empty()
    }

    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        encode(self.instrument, &self.ticks)
    }

    /// Feed latency statistics, for judging whether a dataset can
    /// support latency-aware work at all.
    ///
    /// A dataset whose arrival timestamps equal its exchange timestamps
    /// carries no latency information — it was either captured without
    /// them or synthesized — and that is worth knowing *before* someone
    /// calibrates a latency model against it.
    #[must_use]
    pub fn feed_latency_summary(&self) -> FeedLatency {
        let mut min = i64::MAX;
        let mut max = i64::MIN;
        let mut sum: i128 = 0;
        let mut negative = 0usize;
        for t in &self.ticks {
            let l = t.stamp.feed_latency();
            min = min.min(l);
            max = max.max(l);
            sum += i128::from(l);
            if l < 0 {
                negative += 1;
            }
        }
        if self.ticks.is_empty() {
            return FeedLatency::default();
        }
        FeedLatency {
            min,
            max,
            mean: (sum / self.ticks.len() as i128) as i64,
            negative,
            carries_latency: min != 0 || max != 0,
        }
    }
}

/// What a dataset's timestamps say about its capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FeedLatency {
    pub min: i64,
    pub max: i64,
    pub mean: i64,
    /// Records whose arrival precedes their exchange timestamp. Not an
    /// error — it means the capture host's clock ran behind the venue's
    /// — but a number that has to be visible, because it bounds how
    /// much the latency figures can be trusted.
    pub negative: usize,
    /// Whether arrival and exchange timestamps ever differ.
    pub carries_latency: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(n: usize) -> Vec<Tick> {
        (0..n)
            .map(|i| {
                let t = i as i64;
                Tick {
                    stamp: Stamp::new(t * 1_000, t * 1_000 + 250),
                    last: PriceTicks(1_000_000 + t),
                    high: PriceTicks(1_000_010 + t),
                    low: PriceTicks(999_990 + t),
                    bid: PriceTicks(999_999 + t),
                    ask: PriceTicks(1_000_001 + t),
                    volume: oq_types::QtyLots(t * 7),
                }
            })
            .collect()
    }

    #[test]
    fn round_trip_preserves_every_field() {
        let ticks = sample(500);
        let bytes = encode(7, &ticks);
        let (header, decoded) = decode(&bytes).expect("decodes");
        assert_eq!(header.count, 500);
        assert_eq!(header.instrument, 7);
        assert_eq!(decoded, ticks);
    }

    #[test]
    fn an_empty_file_round_trips() {
        let bytes = encode(1, &[]);
        let (header, decoded) = decode(&bytes).expect("decodes");
        assert_eq!(header.count, 0);
        assert!(decoded.is_empty());
    }

    #[test]
    fn truncation_is_detected_rather_than_read_as_a_shorter_file() {
        let bytes = encode(1, &sample(10));
        for cut in 0..bytes.len() {
            assert!(
                decode(&bytes[..cut]).is_err(),
                "a {cut}-byte prefix must not decode"
            );
        }
    }

    #[test]
    fn corrupted_prices_are_caught() {
        // The failure this checksum exists for: a corrupted price does
        // not crash a backtest, it changes the answer.
        let mut bytes = encode(1, &sample(10));
        bytes[HEADER_LEN + 20] ^= 0xFF;
        assert!(matches!(
            decode(&bytes),
            Err(Error::ChecksumMismatch { .. })
        ));
    }

    #[test]
    fn a_foreign_file_is_refused() {
        let mut bytes = encode(1, &sample(2));
        bytes[0] = b'X';
        assert!(matches!(decode(&bytes), Err(Error::BadMagic { .. })));
    }

    #[test]
    fn out_of_order_ticks_are_refused_with_their_index() {
        let mut ticks = sample(10);
        ticks[5].stamp = Stamp::new(0, 0);
        match TickStream::new(1, ticks) {
            Err(Error::OutOfOrder { index, .. }) => assert_eq!(index, 5),
            other => panic!("expected OutOfOrder at 5, got {other:?}"),
        }
    }

    #[test]
    fn equal_timestamps_are_allowed() {
        // Venues do publish multiple events in the same nanosecond;
        // refusing ties would reject valid data.
        let mut ticks = sample(4);
        ticks[2].stamp = ticks[1].stamp;
        assert!(TickStream::new(1, ticks).is_ok());
    }

    #[test]
    fn feed_latency_reports_a_real_capture() {
        let stream = TickStream::new(1, sample(100)).expect("ordered");
        let fl = stream.feed_latency_summary();
        assert!(fl.carries_latency);
        assert_eq!(fl.min, 250);
        assert_eq!(fl.max, 250);
        assert_eq!(fl.negative, 0);
    }

    #[test]
    fn a_dataset_without_arrival_times_says_so() {
        // The check that stops someone calibrating a latency model
        // against data that cannot support one.
        let ticks: Vec<Tick> = (0..10)
            .map(|i| Tick::trades_only(Stamp::synthetic(i * 1_000), 100, 100, 100))
            .collect();
        let stream = TickStream::new(1, ticks).expect("ordered");
        assert!(!stream.feed_latency_summary().carries_latency);
    }

    #[test]
    fn a_clock_that_ran_backwards_is_counted_not_hidden() {
        let mut ticks = sample(10);
        ticks[3].stamp = Stamp::new(3_000, 2_900);
        let stream = TickStream::new(1, ticks).expect("ordered");
        assert_eq!(stream.feed_latency_summary().negative, 1);
    }

    #[test]
    fn stream_bytes_round_trip() {
        let stream = TickStream::new(9, sample(64)).expect("ordered");
        let restored = TickStream::from_bytes(&stream.encode()).expect("round trip");
        assert_eq!(restored, stream);
    }
}

#[cfg(test)]
mod streaming_tests {
    use super::*;
    use oq_types::Stamp;

    fn sample(n: usize) -> Vec<Tick> {
        (0..n)
            .map(|i| {
                let t = i as i64;
                Tick::quoted(
                    Stamp::new(t * 1_000, t * 1_000 + 7),
                    1_000 + t,
                    1_010,
                    990,
                    999,
                    1_001,
                )
                .with_volume(t * 3)
            })
            .collect()
    }

    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn temp_file(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "oq-ticks-{}-{}-{}.oqtk",
            name,
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        p
    }

    #[test]
    fn streaming_and_whole_file_decoding_agree() {
        // Sizes chosen to straddle the block boundary in both
        // directions, because that seam is where a streaming decoder
        // and a streaming checksum break.
        for n in [0usize, 1, 8_191, 8_192, 8_193, 20_000] {
            let path = temp_file("agree");
            let ticks = sample(n);
            std::fs::write(&path, encode(3, &ticks)).expect("write");

            let (h_stream, t_stream) = read_file(&path).expect("stream");
            let (h_whole, t_whole) = decode(&std::fs::read(&path).expect("read")).expect("whole");

            assert_eq!(h_stream, h_whole, "headers differ at n={n}");
            assert_eq!(t_stream, t_whole, "ticks differ at n={n}");
            assert_eq!(t_stream, ticks, "round trip differs at n={n}");
            std::fs::remove_file(&path).ok();
        }
    }

    #[test]
    fn a_corrupted_record_is_caught_while_streaming() {
        let path = temp_file("corrupt");
        let mut bytes = encode(1, &sample(10_000));
        // Past the first block, so the corruption is only reachable
        // after several updates to the streaming checksum.
        let at = HEADER_LEN + 9_000 * RECORD_LEN + 3;
        bytes[at] ^= 0xFF;
        std::fs::write(&path, &bytes).expect("write");

        assert!(matches!(
            read_file(&path),
            Err(Error::ChecksumMismatch { .. })
        ));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_truncated_file_is_reported_as_truncated() {
        let path = temp_file("short");
        let bytes = encode(1, &sample(1_000));
        std::fs::write(&path, &bytes[..bytes.len() - 100]).expect("write");
        assert!(matches!(read_file(&path), Err(Error::Truncated { .. })));
        std::fs::remove_file(&path).ok();
    }
}
