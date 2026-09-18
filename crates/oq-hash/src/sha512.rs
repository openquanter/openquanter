//! SHA-512, for the one venue whose signature needs it.
//!
//! Implemented here for the same reason SHA-256 is: this crate sits
//! under the verification chain and under the path that signs requests
//! against an account, and a dependency in either is one more thing
//! trusted with a secret.
//!
//! That argument has a limit and it is worth stating where. A hash is
//! deterministic bit arithmetic with published vectors — it is either
//! right for every input or wrong for one this file can be made to try.
//! Elliptic-curve signing is not: it is constant-time field arithmetic
//! and nonce generation where a bias leaks the key, and it is not
//! hand-written here. See docs/VENUES.md.
//!
//! Checked against the NIST vectors and, through `hmac`, against
//! RFC 4231.

/// The first sixty-four bits of the fractional parts of the square roots
/// of the first eight primes.
const H0: [u64; 8] = [
    0x6a09_e667_f3bc_c908,
    0xbb67_ae85_84ca_a73b,
    0x3c6e_f372_fe94_f82b,
    0xa54f_f53a_5f1d_36f1,
    0x510e_527f_ade6_82d1,
    0x9b05_688c_2b3e_6c1f,
    0x1f83_d9ab_fb41_bd6b,
    0x5be0_cd19_137e_2179,
];

/// The first sixty-four bits of the fractional parts of the cube roots
/// of the first eighty primes.
const K: [u64; 80] = [
    0x428a_2f98_d728_ae22,
    0x7137_4491_23ef_65cd,
    0xb5c0_fbcf_ec4d_3b2f,
    0xe9b5_dba5_8189_dbbc,
    0x3956_c25b_f348_b538,
    0x59f1_11f1_b605_d019,
    0x923f_82a4_af19_4f9b,
    0xab1c_5ed5_da6d_8118,
    0xd807_aa98_a303_0242,
    0x1283_5b01_4570_6fbe,
    0x2431_85be_4ee4_b28c,
    0x550c_7dc3_d5ff_b4e2,
    0x72be_5d74_f27b_896f,
    0x80de_b1fe_3b16_96b1,
    0x9bdc_06a7_25c7_1235,
    0xc19b_f174_cf69_2694,
    0xe49b_69c1_9ef1_4ad2,
    0xefbe_4786_384f_25e3,
    0x0fc1_9dc6_8b8c_d5b5,
    0x240c_a1cc_77ac_9c65,
    0x2de9_2c6f_592b_0275,
    0x4a74_84aa_6ea6_e483,
    0x5cb0_a9dc_bd41_fbd4,
    0x76f9_88da_8311_53b5,
    0x983e_5152_ee66_dfab,
    0xa831_c66d_2db4_3210,
    0xb003_27c8_98fb_213f,
    0xbf59_7fc7_beef_0ee4,
    0xc6e0_0bf3_3da8_8fc2,
    0xd5a7_9147_930a_a725,
    0x06ca_6351_e003_826f,
    0x1429_2967_0a0e_6e70,
    0x27b7_0a85_46d2_2ffc,
    0x2e1b_2138_5c26_c926,
    0x4d2c_6dfc_5ac4_2aed,
    0x5338_0d13_9d95_b3df,
    0x650a_7354_8baf_63de,
    0x766a_0abb_3c77_b2a8,
    0x81c2_c92e_47ed_aee6,
    0x9272_2c85_1482_353b,
    0xa2bf_e8a1_4cf1_0364,
    0xa81a_664b_bc42_3001,
    0xc24b_8b70_d0f8_9791,
    0xc76c_51a3_0654_be30,
    0xd192_e819_d6ef_5218,
    0xd699_0624_5565_a910,
    0xf40e_3585_5771_202a,
    0x106a_a070_32bb_d1b8,
    0x19a4_c116_b8d2_d0c8,
    0x1e37_6c08_5141_ab53,
    0x2748_774c_df8e_eb99,
    0x34b0_bcb5_e19b_48a8,
    0x391c_0cb3_c5c9_5a63,
    0x4ed8_aa4a_e341_8acb,
    0x5b9c_ca4f_7763_e373,
    0x682e_6ff3_d6b2_b8a3,
    0x748f_82ee_5def_b2fc,
    0x78a5_636f_4317_2f60,
    0x84c8_7814_a1f0_ab72,
    0x8cc7_0208_1a64_39ec,
    0x90be_fffa_2363_1e28,
    0xa450_6ceb_de82_bde9,
    0xbef9_a3f7_b2c6_7915,
    0xc671_78f2_e372_532b,
    0xca27_3ece_ea26_619c,
    0xd186_b8c7_21c0_c207,
    0xeada_7dd6_cde0_eb1e,
    0xf57d_4f7f_ee6e_d178,
    0x06f0_67aa_7217_6fba,
    0x0a63_7dc5_a2c8_98a6,
    0x113f_9804_bef9_0dae,
    0x1b71_0b35_131c_471b,
    0x28db_77f5_2304_7d84,
    0x32ca_ab7b_40c7_2493,
    0x3c9e_be0a_15c9_bebc,
    0x431d_67c4_9c10_0d4c,
    0x4cc5_d4be_cb3e_42b6,
    0x597f_299c_fc65_7e2a,
    0x5fcb_6fab_3ad6_faec,
    0x6c44_198c_4a47_5817,
];

/// Streaming SHA-512 state.
///
/// The same shape as [`crate::sha256::Sha256`], with the sizes the
/// standard doubles: a 128-byte block, 64-bit words, eighty rounds, and
/// a 128-bit length field.
#[derive(Debug, Clone)]
pub struct Sha512 {
    state: [u64; 8],
    buffer: [u8; 128],
    buffered: usize,
    /// The message length in bits. The standard reserves 128 bits for
    /// this; `u128` is that field rather than a generous `u64`, so a
    /// length that overflows 64 bits is padded correctly instead of
    /// wrapping into a different message's digest.
    length_bits: u128,
}

impl Default for Sha512 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha512 {
    /// A fresh hasher.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: H0,
            buffer: [0u8; 128],
            buffered: 0,
            length_bits: 0,
        }
    }

    /// Add bytes.
    pub fn update(&mut self, mut data: &[u8]) {
        self.length_bits = self.length_bits.wrapping_add((data.len() as u128) * 8);
        if self.buffered > 0 {
            let want = 128 - self.buffered;
            let take = want.min(data.len());
            self.buffer[self.buffered..self.buffered + take].copy_from_slice(&data[..take]);
            self.buffered += take;
            data = &data[take..];
            if self.buffered == 128 {
                let block = self.buffer;
                self.compress(&block);
                self.buffered = 0;
            }
        }
        while data.len() >= 128 {
            let mut block = [0u8; 128];
            block.copy_from_slice(&data[..128]);
            self.compress(&block);
            data = &data[128..];
        }
        if !data.is_empty() {
            self.buffer[..data.len()].copy_from_slice(data);
            self.buffered = data.len();
        }
    }

    /// Finish and return the digest.
    #[must_use]
    pub fn finalize(mut self) -> [u8; 64] {
        let bits = self.length_bits;
        // A single one bit, zeroes, then the length in the last sixteen
        // bytes. The pad is at least one byte and the length needs
        // sixteen, so a block with more than 111 bytes in it spills into
        // another one.
        let mut pad = [0u8; 256];
        pad[0] = 0x80;
        let rest = (self.buffered + 1) % 128;
        let zeros = if rest <= 112 { 112 - rest } else { 240 - rest };
        let total = 1 + zeros;
        let mut tail = [0u8; 16];
        tail.copy_from_slice(&bits.to_be_bytes());
        let mut with_length = Vec::with_capacity(total + 16);
        with_length.extend_from_slice(&pad[..total]);
        with_length.extend_from_slice(&tail);
        self.update_no_length(&with_length);

        let mut out = [0u8; 64];
        for (i, word) in self.state.iter().enumerate() {
            out[i * 8..i * 8 + 8].copy_from_slice(&word.to_be_bytes());
        }
        out
    }

    /// Feed bytes without counting them, for the padding itself.
    fn update_no_length(&mut self, data: &[u8]) {
        let before = self.length_bits;
        self.update(data);
        self.length_bits = before;
    }

    fn compress(&mut self, block: &[u8; 128]) {
        let mut w = [0u64; 80];
        for i in 0..16 {
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&block[i * 8..i * 8 + 8]);
            w[i] = u64::from_be_bytes(bytes);
        }
        for i in 16..80 {
            let s0 = w[i - 15].rotate_right(1) ^ w[i - 15].rotate_right(8) ^ (w[i - 15] >> 7);
            let s1 = w[i - 2].rotate_right(19) ^ w[i - 2].rotate_right(61) ^ (w[i - 2] >> 6);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for i in 0..80 {
            let s1 = e.rotate_right(14) ^ e.rotate_right(18) ^ e.rotate_right(41);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(28) ^ a.rotate_right(34) ^ a.rotate_right(39);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (slot, value) in self.state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(value);
        }
    }
}

/// SHA-512 of `data`, as lowercase hex.
#[must_use]
pub fn sha512_hex(data: &[u8]) -> String {
    let mut h = Sha512::new();
    h.update(data);
    crate::sha256::to_hex(&h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_standard_vectors() {
        assert_eq!(
            sha512_hex(b""),
            "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce\
             47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e"
        );
        assert_eq!(
            sha512_hex(b"abc"),
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a\
             2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
        );
        // 112 bytes: the case that lands exactly on the boundary where
        // the length no longer fits beside the padding.
        assert_eq!(
            sha512_hex(
                b"abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmn\
                  hijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu"
                    .split(|b: &u8| b.is_ascii_whitespace())
                    .flatten()
                    .copied()
                    .collect::<Vec<u8>>()
                    .as_slice()
            ),
            "8e959b75dae313da8cf4f72814fc143f8f7779c6eb9f7fa17299aeadb6889018\
             501d289e4900f7e4331b99dec4b5433ac7d329eeb6dd26545e96e55b874be909"
        );
    }

    #[test]
    fn a_message_that_spills_the_length_into_another_block() {
        // 111, 112 and 113 bytes: one below the boundary, on it, and one
        // above. The middle one is where a naive pad writes the length
        // over the message.
        for n in [111usize, 112, 113, 127, 128, 129] {
            let message = vec![b'a'; n];
            let mut streamed = Sha512::new();
            for chunk in message.chunks(7) {
                streamed.update(chunk);
            }
            let mut at_once = Sha512::new();
            at_once.update(&message);
            assert_eq!(
                streamed.finalize(),
                at_once.finalize(),
                "a {n}-byte message hashed in pieces must equal the whole"
            );
        }
    }
}
