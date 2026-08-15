//! CRC-32 (IEEE 802.3), for detecting a torn or corrupted record.
//!
//! Implemented here rather than taken as a dependency, for the same
//! reason the parity crate implements its own hash: the tool that
//! decides whether a journal is trustworthy should be buildable from
//! the workspace alone.
//!
//! This is an integrity check, not a security primitive. It catches
//! the failure this crate actually faces — a process that died halfway
//! through a write, leaving a partial record and a checksum that no
//! longer matches. It would not stop a deliberate forgery, and nothing
//! here claims otherwise.

const POLYNOMIAL: u32 = 0xEDB8_8320;

const fn build_table() -> [u32; 256] {
    let mut table = [0u32; 256];
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
        table[i] = crc;
        i += 1;
    }
    table
}

static TABLE: [u32; 256] = build_table();

/// CRC-32 of a byte slice.
#[must_use]
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in bytes {
        let idx = ((crc ^ u32::from(b)) & 0xFF) as usize;
        crc = (crc >> 8) ^ TABLE[idx];
    }
    crc ^ 0xFFFF_FFFF
}

/// Incremental CRC-32, for checksumming a header and payload without
/// first joining them into one buffer.
#[derive(Debug, Clone, Copy)]
pub struct Crc32 {
    state: u32,
}

impl Default for Crc32 {
    fn default() -> Self {
        Self::new()
    }
}

impl Crc32 {
    #[must_use]
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

    #[must_use]
    pub const fn finish(self) -> u32 {
        self.state ^ 0xFFFF_FFFF
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        // The standard check values for CRC-32/ISO-HDLC.
        assert_eq!(crc32(b""), 0x0000_0000);
        assert_eq!(crc32(b"a"), 0xE8B7_BE43);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b"The quick brown fox jumps over the lazy dog"), 0x414F_A339);
    }

    #[test]
    fn incremental_matches_one_shot_at_every_split() {
        let data = b"openquanter journal record payload, long enough to split";
        let expected = crc32(data);
        for split in 0..=data.len() {
            let mut crc = Crc32::new();
            crc.update(&data[..split]);
            crc.update(&data[split..]);
            assert_eq!(crc.finish(), expected, "split at {split}");
        }
    }

    #[test]
    fn a_single_flipped_bit_changes_the_checksum() {
        let mut data = *b"record";
        let before = crc32(&data);
        data[3] ^= 0b0000_0001;
        assert_ne!(crc32(&data), before);
    }
}
