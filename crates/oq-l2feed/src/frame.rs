//! Record framing.
//!
//! Length-prefixed frames carrying the venue's bytes verbatim. The
//! layout is fixed by `docs/CAPTURE-FORMAT.md`; changing it means
//! changing the format version, never reinterpreting a field.

use oq_hash::crc32;

/// Bytes of a version 1 frame header that follow the length prefix.
pub const HEADER_LEN: usize = 21;
/// Bytes of a version 2 frame header that follow the length prefix:
/// the version 1 fields plus a checksum over the header itself.
pub const HEADER_LEN_V2: usize = 25;
/// Bytes of the length prefix itself.
pub const LEN_PREFIX: usize = 4;
/// The largest frame either version may declare, header included.
///
/// Far above any payload a venue sends — the largest depth message seen
/// is a few hundred kilobytes — and far below what a flipped high bit in
/// the length makes of it. Past this a length is corruption, not a frame
/// still being written.
pub const MAX_FRAME_LEN: u32 = 64 * 1024 * 1024;

/// Timestamp sentinel for a payload that carries no exchange time.
pub const NO_EXCH_TS: i64 = i64::MIN;

/// What a frame carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The venue's bytes, exactly as received.
    Payload,
    /// A control record written by the capture process itself.
    Control,
}

impl Kind {
    /// The kind byte a version 2 frame is written with.
    const fn as_u8_v2(self) -> u8 {
        match self {
            Self::Payload => 2,
            Self::Control => 3,
        }
    }

    /// What a kind byte means: the record kind, and whether the frame
    /// has a version 2 header.
    const fn from_u8(value: u8) -> Option<(Self, bool)> {
        match value {
            0 => Some((Self::Payload, false)),
            1 => Some((Self::Control, false)),
            2 => Some((Self::Payload, true)),
            3 => Some((Self::Control, true)),
            _ => None,
        }
    }
}

/// One decoded record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// Payload or control.
    pub kind: Kind,
    /// Local receive time, nanoseconds since the Unix epoch.
    pub local_ts: i64,
    /// Exchange timestamp, or [`NO_EXCH_TS`].
    pub exch_ts: i64,
    /// The bytes.
    pub payload: Vec<u8>,
}

impl Record {
    /// A payload record.
    #[must_use]
    pub fn payload(local_ts: i64, exch_ts: i64, payload: Vec<u8>) -> Self {
        Self {
            kind: Kind::Payload,
            local_ts,
            exch_ts,
            payload,
        }
    }

    /// A control record.
    #[must_use]
    pub fn control(local_ts: i64, payload: Vec<u8>) -> Self {
        Self {
            kind: Kind::Control,
            local_ts,
            exch_ts: NO_EXCH_TS,
            payload,
        }
    }

    /// The timestamp that decides which UTC day this record belongs to.
    ///
    /// Exchange time when there is one: a file must hold exactly its own
    /// day even when the capture host's clock drifts. Control records
    /// and payloads without an exchange timestamp fall back to local
    /// time, which is the only thing available for them.
    #[must_use]
    pub fn day_ts(&self) -> i64 {
        if self.exch_ts == NO_EXCH_TS {
            self.local_ts
        } else {
            self.exch_ts
        }
    }

    /// Encode into `out`, as a version 2 frame.
    ///
    /// The header's own checksum covers the length, the kind and both
    /// timestamps, so a reader can trust the length before acting on it.
    pub fn encode(&self, out: &mut Vec<u8>) {
        let len = (HEADER_LEN_V2 + self.payload.len()) as u32;
        let start = out.len();
        out.extend_from_slice(&len.to_le_bytes());
        out.push(self.kind.as_u8_v2());
        out.extend_from_slice(&self.local_ts.to_le_bytes());
        out.extend_from_slice(&self.exch_ts.to_le_bytes());
        let header_crc = crc32(&out[start..start + LEN_PREFIX + 17]);
        out.extend_from_slice(&header_crc.to_le_bytes());
        out.extend_from_slice(&crc32(&self.payload).to_le_bytes());
        out.extend_from_slice(&self.payload);
    }

    /// Encode as a version 1 frame, which this crate no longer writes.
    ///
    /// For tests of the reader: archives written before version 2 must
    /// stay readable, and the only way to keep proving that is to keep
    /// producing them.
    #[cfg(test)]
    pub(crate) fn encode_v1(&self, out: &mut Vec<u8>) {
        let len = (HEADER_LEN + self.payload.len()) as u32;
        out.extend_from_slice(&len.to_le_bytes());
        out.push(match self.kind {
            Kind::Payload => 0,
            Kind::Control => 1,
        });
        out.extend_from_slice(&self.local_ts.to_le_bytes());
        out.extend_from_slice(&self.exch_ts.to_le_bytes());
        out.extend_from_slice(&crc32(&self.payload).to_le_bytes());
        out.extend_from_slice(&self.payload);
    }

    /// Encoded size in bytes.
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        LEN_PREFIX + HEADER_LEN_V2 + self.payload.len()
    }
}

/// Why a frame could not be decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// The buffer ends mid-frame. At the end of a file this is a torn
    /// final record — the normal result of a crash — and means "stop
    /// reading here", not "this file is damaged".
    Truncated,
    /// The payload does not match its checksum. Anywhere but the final
    /// record, this is corruption and must not be passed over silently.
    ChecksumMismatch,
    /// The frame declares a kind this format version does not define.
    UnknownKind(u8),
    /// The length prefix is impossible: shorter than a header, or longer
    /// than [`MAX_FRAME_LEN`].
    InvalidLength(u32),
    /// A version 2 header does not match its own checksum.
    ///
    /// Always corruption, never a torn tail: the header is only checked
    /// once all of it has been read. It is what version 1 could not
    /// report — a damaged length there reads as a frame running past the
    /// end of the file, and every record after it was dropped as a torn
    /// tail without a word.
    HeaderChecksumMismatch,
}

impl core::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Truncated => f.write_str("frame is truncated"),
            Self::ChecksumMismatch => f.write_str("payload does not match its checksum"),
            Self::UnknownKind(k) => write!(f, "unknown record kind {k}"),
            Self::InvalidLength(l) => write!(f, "invalid frame length {l}"),
            Self::HeaderChecksumMismatch => f.write_str("frame header does not match its checksum"),
        }
    }
}

impl core::error::Error for DecodeError {}

/// Decode one frame from the front of `input`.
///
/// Returns the record and the number of bytes consumed.
///
/// # Errors
///
/// See [`DecodeError`].
pub fn decode(input: &[u8]) -> Result<(Record, usize), DecodeError> {
    if input.len() < LEN_PREFIX + 1 {
        return Err(DecodeError::Truncated);
    }
    let len = u32::from_le_bytes([input[0], input[1], input[2], input[3]]);
    let (kind, v2) = Kind::from_u8(input[4]).ok_or(DecodeError::UnknownKind(input[4]))?;
    let header_len = if v2 { HEADER_LEN_V2 } else { HEADER_LEN };

    // Version 2: the whole header first, and its checksum, before the
    // length is believed. A header cut short is a torn tail; one that is
    // all there and wrong is damage.
    if v2 {
        if input.len() < LEN_PREFIX + header_len {
            return Err(DecodeError::Truncated);
        }
        let stored = u32::from_le_bytes(input[21..25].try_into().expect("4 bytes"));
        if crc32(&input[..21]) != stored {
            return Err(DecodeError::HeaderChecksumMismatch);
        }
    }
    if (len as usize) < header_len || len > MAX_FRAME_LEN {
        return Err(DecodeError::InvalidLength(len));
    }

    let total = LEN_PREFIX + len as usize;
    if input.len() < total {
        return Err(DecodeError::Truncated);
    }

    let local_ts = i64::from_le_bytes(input[5..13].try_into().expect("8 bytes"));
    let exch_ts = i64::from_le_bytes(input[13..21].try_into().expect("8 bytes"));
    let crc_at = LEN_PREFIX + header_len - 4;
    let expected_crc = u32::from_le_bytes(input[crc_at..crc_at + 4].try_into().expect("4 bytes"));
    let payload = &input[LEN_PREFIX + header_len..total];

    if crc32(payload) != expected_crc {
        return Err(DecodeError::ChecksumMismatch);
    }

    Ok((
        Record {
            kind,
            local_ts,
            exch_ts,
            payload: payload.to_vec(),
        },
        total,
    ))
}

/// Decode a whole buffer, tolerating a torn final record.
///
/// Returns the records and the number of trailing bytes that formed an
/// incomplete frame. A non-zero remainder at the end of a file is the
/// expected signature of a crash during capture.
///
/// # Errors
///
/// Propagates corruption found before the final frame.
pub fn decode_all(input: &[u8]) -> Result<(Vec<Record>, usize), DecodeError> {
    let mut records = Vec::new();
    let mut offset = 0usize;

    while offset < input.len() {
        match decode(&input[offset..]) {
            Ok((record, consumed)) => {
                records.push(record);
                offset += consumed;
            }
            Err(DecodeError::Truncated) => return Ok((records, input.len() - offset)),
            Err(other) => return Err(other),
        }
    }

    Ok((records, 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Record {
        Record::payload(
            1_786_780_800_000_141_000,
            1_786_780_800_000_000_000,
            b"{\"e\":\"depthUpdate\",\"U\":1}".to_vec(),
        )
    }

    #[test]
    fn round_trips() {
        let record = sample();
        let mut buffer = Vec::new();
        record.encode(&mut buffer);
        assert_eq!(buffer.len(), record.encoded_len());

        let (decoded, consumed) = decode(&buffer).unwrap();
        assert_eq!(decoded, record);
        assert_eq!(consumed, buffer.len());
    }

    #[test]
    fn payload_bytes_survive_verbatim() {
        // Newlines, invalid UTF-8, and NUL bytes must all round trip:
        // this is the property that framing exists to provide.
        let hostile = vec![b'{', b'\n', 0x00, 0xFF, 0xFE, b'}', b'\r'];
        let record = Record::payload(1, 2, hostile.clone());
        let mut buffer = Vec::new();
        record.encode(&mut buffer);
        assert_eq!(decode(&buffer).unwrap().0.payload, hostile);
    }

    #[test]
    fn a_torn_final_record_stops_reading_without_erroring() {
        let mut buffer = Vec::new();
        sample().encode(&mut buffer);
        sample().encode(&mut buffer);
        let full_len = buffer.len();
        buffer.truncate(full_len - 7); // crash mid-write

        let (records, remainder) = decode_all(&buffer).unwrap();
        assert_eq!(records.len(), 1, "the intact record is still readable");
        assert!(remainder > 0, "the torn tail is reported, not hidden");
    }

    #[test]
    fn corruption_before_the_end_is_an_error() {
        let mut buffer = Vec::new();
        sample().encode(&mut buffer);
        sample().encode(&mut buffer);
        let payload_start = LEN_PREFIX + HEADER_LEN_V2;
        buffer[payload_start + 2] ^= 0x01; // flip a bit in the first payload

        assert_eq!(decode_all(&buffer), Err(DecodeError::ChecksumMismatch));
    }

    /// The failure version 2 exists for. In version 1 a damaged length
    /// read as a frame running past the end of the file, and the reader
    /// stopped there as if at a torn tail: reproduced, 990 of 1000
    /// records dropped without an error.
    #[test]
    fn any_flipped_bit_in_a_header_is_reported_not_read_past() {
        let mut clean = Vec::new();
        for _ in 0..1000 {
            sample().encode(&mut clean);
        }
        for bit in 0..(21 * 8) {
            let mut damaged = clean.clone();
            damaged[bit / 8] ^= 1 << (bit % 8);
            let result = decode_all(&damaged);
            assert!(
                result.is_err(),
                "bit {bit} of the first header flipped and the file read as {:?}",
                result.map(|(r, rest)| (r.len(), rest))
            );
        }
    }

    #[test]
    fn a_header_cut_short_is_a_torn_tail() {
        let mut buffer = Vec::new();
        sample().encode(&mut buffer);
        sample().encode(&mut buffer);
        let one = sample().encoded_len();
        buffer.truncate(one + LEN_PREFIX + 10);
        let (records, rest) = decode_all(&buffer).expect("a torn tail is not damage");
        assert_eq!(records.len(), 1);
        assert_eq!(rest, LEN_PREFIX + 10);
    }

    /// Archives written before version 2 read as they always did, and a
    /// file whose writer was upgraded mid-day reads across the seam.
    #[test]
    fn version_1_frames_still_read_alone_and_mixed() {
        let control = Record::control(7, b"{\"type\":\"session_start\"}".to_vec());
        let mut old = Vec::new();
        sample().encode_v1(&mut old);
        control.encode_v1(&mut old);
        let (records, rest) = decode_all(&old).expect("version 1 reads");
        assert_eq!(records, vec![sample(), control.clone()]);
        assert_eq!(rest, 0);

        let mut mixed = old.clone();
        control.encode(&mut mixed);
        sample().encode(&mut mixed);
        let (records, rest) = decode_all(&mixed).expect("the seam reads");
        assert_eq!(records, vec![sample(), control.clone(), control, sample()]);
        assert_eq!(rest, 0);
    }

    #[test]
    fn frames_are_written_as_version_2() {
        let mut buffer = Vec::new();
        sample().encode(&mut buffer);
        Record::control(1, b"{}".to_vec()).encode(&mut buffer);
        assert_eq!(buffer[4], 2);
        assert_eq!(buffer[sample().encoded_len() + 4], 3);
    }

    #[test]
    fn control_records_have_no_exchange_timestamp() {
        let record = Record::control(42, b"{\"type\":\"gap\"}".to_vec());
        assert_eq!(record.exch_ts, NO_EXCH_TS);
        assert_eq!(record.day_ts(), 42, "falls back to local time");
    }

    #[test]
    fn day_ts_prefers_exchange_time() {
        let record = Record::payload(999, 111, b"x".to_vec());
        assert_eq!(record.day_ts(), 111);
    }

    #[test]
    fn rejects_impossible_lengths_and_unknown_kinds() {
        let mut buffer = Vec::new();
        sample().encode(&mut buffer);

        let mut v1 = Vec::new();
        sample().encode_v1(&mut v1);
        let mut short = v1.clone();
        short[0..4].copy_from_slice(&3u32.to_le_bytes());
        assert_eq!(decode(&short), Err(DecodeError::InvalidLength(3)));

        let mut huge = v1.clone();
        huge[0..4].copy_from_slice(&(MAX_FRAME_LEN + 1).to_le_bytes());
        assert_eq!(
            decode(&huge),
            Err(DecodeError::InvalidLength(MAX_FRAME_LEN + 1)),
            "a length past the cap is damage, not a frame still being written"
        );

        let mut alien = buffer.clone();
        alien[4] = 9;
        assert_eq!(decode(&alien), Err(DecodeError::UnknownKind(9)));
    }
}
