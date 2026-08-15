//! Record framing.
//!
//! Length-prefixed frames carrying the venue's bytes verbatim. The
//! layout is fixed by `docs/CAPTURE-FORMAT.md`; changing it means
//! changing the format version, never reinterpreting a field.

use oq_hash::crc32;

/// Bytes of frame header that follow the length prefix.
pub const HEADER_LEN: usize = 21;
/// Bytes of the length prefix itself.
pub const LEN_PREFIX: usize = 4;

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
    const fn as_u8(self) -> u8 {
        match self {
            Self::Payload => 0,
            Self::Control => 1,
        }
    }

    const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Payload),
            1 => Some(Self::Control),
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

    /// Encode into `out`.
    pub fn encode(&self, out: &mut Vec<u8>) {
        let len = (HEADER_LEN + self.payload.len()) as u32;
        out.extend_from_slice(&len.to_le_bytes());
        out.push(self.kind.as_u8());
        out.extend_from_slice(&self.local_ts.to_le_bytes());
        out.extend_from_slice(&self.exch_ts.to_le_bytes());
        out.extend_from_slice(&crc32(&self.payload).to_le_bytes());
        out.extend_from_slice(&self.payload);
    }

    /// Encoded size in bytes.
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        LEN_PREFIX + HEADER_LEN + self.payload.len()
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
    /// The length prefix is impossible.
    InvalidLength(u32),
}

impl core::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Truncated => f.write_str("frame is truncated"),
            Self::ChecksumMismatch => f.write_str("payload does not match its checksum"),
            Self::UnknownKind(k) => write!(f, "unknown record kind {k}"),
            Self::InvalidLength(l) => write!(f, "invalid frame length {l}"),
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
    if input.len() < LEN_PREFIX {
        return Err(DecodeError::Truncated);
    }
    let len = u32::from_le_bytes([input[0], input[1], input[2], input[3]]);
    if (len as usize) < HEADER_LEN {
        return Err(DecodeError::InvalidLength(len));
    }

    let total = LEN_PREFIX + len as usize;
    if input.len() < total {
        return Err(DecodeError::Truncated);
    }

    let kind = Kind::from_u8(input[4]).ok_or(DecodeError::UnknownKind(input[4]))?;
    let local_ts = i64::from_le_bytes(input[5..13].try_into().expect("8 bytes"));
    let exch_ts = i64::from_le_bytes(input[13..21].try_into().expect("8 bytes"));
    let expected_crc = u32::from_le_bytes(input[21..25].try_into().expect("4 bytes"));
    let payload = &input[LEN_PREFIX + HEADER_LEN..total];

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
        let payload_start = LEN_PREFIX + HEADER_LEN;
        buffer[payload_start + 2] ^= 0x01; // flip a bit in the first payload

        assert_eq!(decode_all(&buffer), Err(DecodeError::ChecksumMismatch));
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

        let mut short = buffer.clone();
        short[0..4].copy_from_slice(&3u32.to_le_bytes());
        assert_eq!(decode(&short), Err(DecodeError::InvalidLength(3)));

        let mut alien = buffer.clone();
        alien[4] = 9;
        assert_eq!(decode(&alien), Err(DecodeError::UnknownKind(9)));
    }
}
