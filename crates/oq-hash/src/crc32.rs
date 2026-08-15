//! CRC-32 (IEEE 802.3), used per capture record.
//!
//! Its job is narrow: tell a torn final record apart from corruption in
//! the middle of a file. Those are different problems — one is a normal
//! consequence of a crash and means "stop reading here", the other means
//! "this archive is damaged, do not proceed silently".

/// Reflected IEEE 802.3 polynomial.
const POLY: u32 = 0xEDB8_8320;

const fn build_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ POLY
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

/// CRC-32 of `data`.
#[must_use]
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for byte in data {
        crc = TABLE[((crc ^ u32::from(*byte)) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_standard_check_value() {
        // The CRC-32 check value defined for this polynomial.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn empty_input_is_zero() {
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn detects_single_bit_flips() {
        let base = b"depth update payload".to_vec();
        let original = crc32(&base);
        for byte_index in 0..base.len() {
            for bit in 0..8 {
                let mut flipped = base.clone();
                flipped[byte_index] ^= 1 << bit;
                assert_ne!(
                    crc32(&flipped),
                    original,
                    "flip at byte {byte_index} bit {bit} went undetected"
                );
            }
        }
    }
}
