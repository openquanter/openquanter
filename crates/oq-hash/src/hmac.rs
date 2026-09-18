//! HMAC-SHA256, as venues require for signed requests.
//!
//! Written here rather than pulled in because the rest of this crate is,
//! and because the alternative is a dependency in the path that signs
//! orders. What that path does is small and fully specified by RFC 2104;
//! what a dependency does is whatever it does.
//!
//! The test vectors are the RFC's own, including the over-long-key case
//! that a naive implementation gets wrong by using the key as given
//! instead of hashing it first.

use crate::sha256::Sha256;

/// SHA-256's block size. Keys are padded to it, or hashed down to fit.
const BLOCK: usize = 64;
/// SHA-512 hashes in 128-byte blocks, so its HMAC pads to 128.
///
/// Stated separately rather than derived, because using 64 here is a
/// mistake that produces a plausible-looking MAC which no venue will
/// accept and no local test would catch without a published vector.
const BLOCK_512: usize = 128;

/// Authenticate `message` with `key`.
///
/// # Panics
/// Never: every path here is fixed-size arithmetic.
#[must_use]
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    // A key longer than the block is replaced by its hash. Skipping this
    // is the classic mistake: the result still looks like a MAC and is
    // wrong for exactly the keys that were chosen to be strong.
    let mut padded = [0u8; BLOCK];
    if key.len() > BLOCK {
        let digest = {
            let mut h = Sha256::new();
            h.update(key);
            h.finalize()
        };
        padded[..32].copy_from_slice(&digest);
    } else {
        padded[..key.len()].copy_from_slice(key);
    }

    let mut inner_pad = [0x36u8; BLOCK];
    let mut outer_pad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        inner_pad[i] ^= padded[i];
        outer_pad[i] ^= padded[i];
    }

    let inner = {
        let mut h = Sha256::new();
        h.update(&inner_pad);
        h.update(message);
        h.finalize()
    };

    let mut h = Sha256::new();
    h.update(&outer_pad);
    h.update(&inner);
    h.finalize()
}

/// Authenticate `message` with `key`, over SHA-512.
///
/// The same construction as [`hmac_sha256`] at the sizes SHA-512 uses.
/// One venue's signature is built on this: Kraken Futures hashes
/// `postData + nonce + path` with SHA-256, then runs *this* over the
/// result under a base64-decoded secret.
///
/// # Panics
/// Never: every path here is fixed-size arithmetic.
#[must_use]
pub fn hmac_sha512(key: &[u8], message: &[u8]) -> [u8; 64] {
    // A key longer than the block is replaced by its hash, for the same
    // reason as above: skipping it is wrong for exactly the keys chosen
    // to be strong.
    let mut padded = [0u8; BLOCK_512];
    if key.len() > BLOCK_512 {
        let digest = {
            let mut h = crate::sha512::Sha512::new();
            h.update(key);
            h.finalize()
        };
        padded[..64].copy_from_slice(&digest);
    } else {
        padded[..key.len()].copy_from_slice(key);
    }

    let mut inner_pad = [0x36u8; BLOCK_512];
    let mut outer_pad = [0x5cu8; BLOCK_512];
    for i in 0..BLOCK_512 {
        inner_pad[i] ^= padded[i];
        outer_pad[i] ^= padded[i];
    }

    let inner = {
        let mut h = crate::sha512::Sha512::new();
        h.update(&inner_pad);
        h.update(message);
        h.finalize()
    };

    let mut h = crate::sha512::Sha512::new();
    h.update(&outer_pad);
    h.update(&inner);
    h.finalize()
}

/// Authenticate `message` and render it as lowercase hex, which is the
/// form every venue's `signature` parameter takes.
#[must_use]
pub fn hmac_sha256_hex(key: &[u8], message: &[u8]) -> String {
    crate::sha256::to_hex(&hmac_sha256(key, message))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
            .collect()
    }

    /// RFC 4231 test case 1.
    #[test]
    fn rfc_4231_sha512_case_one() {
        // The published vector. A block size of 64 here produces a MAC
        // that looks fine and fails this.
        let mac = hmac_sha512(&[0x0b; 20], b"Hi There");
        assert_eq!(
            crate::sha256::to_hex(&mac),
            "87aa7cdea5ef619d4ff0b4241a1d6cb02379f4e2ce4ec2787ad0b30545e17cde\
             daa833b7d6b8a702038b274eaea3f4e4be9d914eeb61f1702e696c203a126854"
        );
    }

    #[test]
    fn rfc_4231_sha512_case_two() {
        let mac = hmac_sha512(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            crate::sha256::to_hex(&mac),
            "164b7a7bfcf819e2e395fbe73b56e0a387bd64222e831fd610270cd7ea250554\
             9758bf75c05a994a6d034f65f8f0e6fdcaeab1a34d4a6b4b636e070a38bce737"
        );
    }

    #[test]
    fn rfc_4231_sha512_a_key_longer_than_its_block() {
        // 131 bytes, which is past SHA-512's 128-byte block and so the
        // case that exercises the hash-the-key branch.
        let mac = hmac_sha512(
            &[0xaa; 131],
            b"Test Using Larger Than Block-Size Key - Hash Key First",
        );
        assert_eq!(
            crate::sha256::to_hex(&mac),
            "80b24263c7c1a3ebb71493c1dd7be8b49b46d1f41b4aeec1121b013783f8f352\
             6b56d037e05f2598bd0fd2215d6a1e5295e64f73f63f0aec8b915a985d786598"
        );
    }

    #[test]
    fn rfc_case_one() {
        let key = hex("0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b");
        assert_eq!(
            hmac_sha256_hex(&key, b"Hi There"),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    /// RFC 4231 test case 2: a key shorter than the block.
    #[test]
    fn rfc_case_two() {
        assert_eq!(
            hmac_sha256_hex(b"Jefe", b"what do ya want for nothing?"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    /// RFC 4231 test case 6: a key longer than the block, which must be
    /// hashed down first. An implementation that skips that step passes
    /// every other case and fails only for strong keys.
    #[test]
    fn a_key_longer_than_the_block_is_hashed_first() {
        let key = vec![0xaa; 131];
        assert_eq!(
            hmac_sha256_hex(
                &key,
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            ),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    /// RFC 4231 test case 7.
    #[test]
    fn a_long_key_and_a_long_message() {
        let key = vec![0xaa; 131];
        let msg = b"This is a test using a larger than block-size key and a larger \
                    than block-size data. The key needs to be hashed before being used \
                    by the HMAC algorithm.";
        assert_eq!(
            hmac_sha256_hex(&key, msg),
            "9b09ffa71b942fcb27635fbcd5b0e944bfdc63644f0713938a7f51535c3a35e2"
        );
    }

    /// A different key must give a different tag. Trivial, and it catches
    /// the implementation that ignores the key entirely — which passes
    /// nothing else here but would be caught late and expensively.
    #[test]
    fn the_key_participates() {
        assert_ne!(hmac_sha256(b"a", b"msg"), hmac_sha256(b"b", b"msg"));
    }
}
