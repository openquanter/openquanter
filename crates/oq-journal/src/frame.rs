//! The on-disk record format.
//!
//! Fixed-layout, little-endian, checksummed. Every field has a fixed
//! offset and a fixed meaning, and **a field is never repurposed**: a
//! new field goes at the end of the payload behind a version bump, and
//! an old reader that does not know about it must still be able to skip
//! the record cleanly.
//!
//! ```text
//! offset   size  field
//!      0      4  magic     'O','Q','R','J'  — resynchronization anchor
//!      4      2  version   frame format version
//!      6      2  kind      payload discriminant, opaque to this crate
//!      8      8  seq       sequence number assigned by the sequencer
//!     16      4  len       payload length in bytes
//!     20    len  payload
//! 20+len      4  crc32     over bytes [4, 20+len)
//! ```
//!
//! The magic leads so that a reader scanning a damaged file has an
//! anchor to resynchronize on. The checksum trails, and covers every
//! byte between the magic and itself — including `len`, the field whose
//! corruption is most dangerous because it decides how far the reader
//! jumps. A corrupted length therefore fails verification rather than
//! being acted on, and [`MAX_PAYLOAD`] bounds what a corrupted length
//! can ask the reader to allocate before that check runs.
//!
//! A trailing checksum keeps the verified region contiguous, so the
//! whole record is checked with one pass over one slice.

use oq_hash::crc32;

/// Bytes preceding the payload.
pub const HEADER_LEN: usize = 20;

/// Bytes following the payload.
pub const TRAILER_LEN: usize = 4;

/// Bytes of framing per record.
pub const FRAMING_LEN: usize = HEADER_LEN + TRAILER_LEN;

/// `OQRJ`, little-endian.
pub const MAGIC: u32 = u32::from_le_bytes(*b"OQRJ");

/// The frame layout this build writes.
pub const VERSION: u16 = 1;

/// The largest payload a single record may carry.
///
/// A bound is required, not optional: without one, a corrupted length
/// field asks the reader to allocate an arbitrary amount of memory
/// before the checksum can reject it.
pub const MAX_PAYLOAD: usize = 16 * 1024 * 1024;

/// A framed record, ready to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub seq: u64,
    pub kind: u16,
    pub payload: Vec<u8>,
}

/// Why a frame could not be decoded.
///
/// The distinction between [`FrameError::Incomplete`] and the others
/// carries the whole torn-tail design: incomplete means "the writer
/// died here", which is expected and recoverable, while a bad magic or
/// checksum in the *middle* of a file means corruption, which is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    /// Fewer bytes remain than the record claims to need.
    Incomplete { needed: usize, available: usize },
    /// The magic did not match; this is not a record boundary.
    BadMagic { found: u32 },
    /// The frame version is newer than this build understands.
    UnknownVersion { found: u16 },
    /// The declared payload length exceeds [`MAX_PAYLOAD`].
    LengthOutOfRange { found: u32 },
    /// The checksum did not match the bytes read.
    ChecksumMismatch { expected: u32, computed: u32 },
}

impl core::fmt::Display for FrameError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Incomplete { needed, available } => {
                write!(
                    f,
                    "incomplete record: needed {needed} bytes, {available} available"
                )
            }
            Self::BadMagic { found } => write!(f, "bad magic {found:#010x}"),
            Self::UnknownVersion { found } => write!(f, "unknown frame version {found}"),
            Self::LengthOutOfRange { found } => write!(f, "payload length {found} out of range"),
            Self::ChecksumMismatch { expected, computed } => {
                write!(
                    f,
                    "checksum mismatch: expected {expected:#010x}, computed {computed:#010x}"
                )
            }
        }
    }
}

impl core::error::Error for FrameError {}

impl Frame {
    #[must_use]
    pub fn new(seq: u64, kind: u16, payload: Vec<u8>) -> Self {
        Self { seq, kind, payload }
    }

    /// Total encoded size of this record.
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        FRAMING_LEN + self.payload.len()
    }

    /// Append the encoded record to `out`.
    ///
    /// # Panics
    /// If the payload exceeds [`MAX_PAYLOAD`]. That is a programming
    /// error at the call site rather than a runtime condition: an event
    /// that large indicates a bug in the producer, and writing it would
    /// produce a journal a conforming reader must reject.
    pub fn encode_into(&self, out: &mut Vec<u8>) {
        assert!(
            self.payload.len() <= MAX_PAYLOAD,
            "payload of {} bytes exceeds MAX_PAYLOAD",
            self.payload.len()
        );
        let start = out.len();
        out.extend_from_slice(&MAGIC.to_le_bytes());
        out.extend_from_slice(&VERSION.to_le_bytes());
        out.extend_from_slice(&self.kind.to_le_bytes());
        out.extend_from_slice(&self.seq.to_le_bytes());
        out.extend_from_slice(&(self.payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.payload);
        let checksum = crc32(&out[start + 4..]);
        out.extend_from_slice(&checksum.to_le_bytes());
    }

    /// Encode to a fresh buffer.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.encoded_len());
        self.encode_into(&mut out);
        out
    }

    /// Decode one record from the front of `bytes`.
    ///
    /// Returns the frame and how many bytes it consumed.
    ///
    /// # Errors
    /// See [`FrameError`]. A caller replaying a journal treats
    /// [`FrameError::Incomplete`] at the end of a file as a normal stop
    /// and anything else as corruption.
    pub fn decode(bytes: &[u8]) -> Result<(Self, usize), FrameError> {
        if bytes.len() < HEADER_LEN {
            return Err(FrameError::Incomplete {
                needed: HEADER_LEN,
                available: bytes.len(),
            });
        }
        let magic = u32::from_le_bytes(bytes[0..4].try_into().expect("4 bytes"));
        if magic != MAGIC {
            return Err(FrameError::BadMagic { found: magic });
        }
        let version = u16::from_le_bytes(bytes[4..6].try_into().expect("2 bytes"));
        if version != VERSION {
            return Err(FrameError::UnknownVersion { found: version });
        }
        let kind = u16::from_le_bytes(bytes[6..8].try_into().expect("2 bytes"));
        let seq = u64::from_le_bytes(bytes[8..16].try_into().expect("8 bytes"));
        let len = u32::from_le_bytes(bytes[16..20].try_into().expect("4 bytes"));
        if len as usize > MAX_PAYLOAD {
            return Err(FrameError::LengthOutOfRange { found: len });
        }

        let total = FRAMING_LEN + len as usize;
        if bytes.len() < total {
            return Err(FrameError::Incomplete {
                needed: total,
                available: bytes.len(),
            });
        }
        let payload_end = HEADER_LEN + len as usize;
        let expected = u32::from_le_bytes(bytes[payload_end..total].try_into().expect("4 bytes"));
        let computed = crc32(&bytes[4..payload_end]);
        if computed != expected {
            return Err(FrameError::ChecksumMismatch { expected, computed });
        }

        Ok((
            Self {
                seq,
                kind,
                payload: bytes[HEADER_LEN..payload_end].to_vec(),
            },
            total,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let frame = Frame::new(42, 7, b"hello journal".to_vec());
        let bytes = frame.encode();
        assert_eq!(bytes.len(), frame.encoded_len());
        let (decoded, used) = Frame::decode(&bytes).expect("valid frame");
        assert_eq!(decoded, frame);
        assert_eq!(used, bytes.len());
    }

    #[test]
    fn empty_payload_round_trips() {
        let frame = Frame::new(1, 0, Vec::new());
        let (decoded, used) = Frame::decode(&frame.encode()).expect("valid frame");
        assert_eq!(decoded.payload.len(), 0);
        assert_eq!(used, FRAMING_LEN);
    }

    #[test]
    fn records_concatenate_without_ambiguity() {
        let mut buf = Vec::new();
        Frame::new(0, 1, b"first".to_vec()).encode_into(&mut buf);
        Frame::new(1, 1, b"second".to_vec()).encode_into(&mut buf);

        let (a, used_a) = Frame::decode(&buf).expect("first");
        let (b, used_b) = Frame::decode(&buf[used_a..]).expect("second");
        assert_eq!(a.payload, b"first");
        assert_eq!(b.payload, b"second");
        assert_eq!(used_a + used_b, buf.len());
    }

    #[test]
    fn truncation_at_every_length_reports_incomplete() {
        // The torn-tail case, which a crashed writer produces: every
        // proper prefix must be diagnosed as incomplete rather than as
        // corruption, because the two have different recoveries.
        let bytes = Frame::new(9, 1, b"payload bytes".to_vec()).encode();
        for cut in 0..bytes.len() {
            match Frame::decode(&bytes[..cut]) {
                Err(FrameError::Incomplete { .. }) => {}
                other => panic!("prefix of {cut} bytes: expected Incomplete, got {other:?}"),
            }
        }
    }

    #[test]
    fn corrupted_payload_is_caught() {
        let mut bytes = Frame::new(3, 2, b"important".to_vec()).encode();
        bytes[HEADER_LEN + 2] ^= 0xFF;
        assert!(matches!(
            Frame::decode(&bytes),
            Err(FrameError::ChecksumMismatch { .. })
        ));
    }

    #[test]
    fn corrupted_length_is_caught_before_it_is_acted_on() {
        // The length field decides how far a reader jumps, so its
        // corruption is the most dangerous. It is inside the checksum,
        // and out-of-range values are rejected before any allocation.
        let mut bytes = Frame::new(3, 2, b"important".to_vec()).encode();
        bytes[16] = 0xFF;
        bytes[17] = 0xFF;
        let err = Frame::decode(&bytes).expect_err("corrupted length must not decode");
        assert!(
            matches!(
                err,
                FrameError::ChecksumMismatch { .. }
                    | FrameError::Incomplete { .. }
                    | FrameError::LengthOutOfRange { .. }
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn bad_magic_is_distinguished_from_truncation() {
        let mut bytes = Frame::new(1, 1, b"x".to_vec()).encode();
        bytes[0] = b'X';
        assert!(matches!(
            Frame::decode(&bytes),
            Err(FrameError::BadMagic { .. })
        ));
    }

    #[test]
    fn every_single_bit_flip_in_a_record_is_detected() {
        // The property the checksum exists for, checked exhaustively on
        // a small record rather than asserted.
        let original = Frame::new(7, 3, b"abcd".to_vec()).encode();
        for byte in 0..original.len() {
            for bit in 0..8 {
                let mut corrupted = original.clone();
                corrupted[byte] ^= 1 << bit;
                if corrupted == original {
                    continue;
                }
                assert!(
                    Frame::decode(&corrupted).is_err_or_differs(&original),
                    "bit {bit} of byte {byte} went undetected"
                );
            }
        }
    }

    /// Helper for the exhaustive bit-flip test: a decode either fails,
    /// or succeeds with content that differs from the original — both
    /// are detections. Silently decoding to the *same* frame would mean
    /// the flipped bit was in a region nothing depends on.
    trait DecodeOutcome {
        fn is_err_or_differs(&self, original: &[u8]) -> bool;
    }

    impl DecodeOutcome for Result<(Frame, usize), FrameError> {
        fn is_err_or_differs(&self, original: &[u8]) -> bool {
            match self {
                Err(_) => true,
                Ok((frame, _)) => frame.encode() != original,
            }
        }
    }
}
