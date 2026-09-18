//! Base64, RFC 4648, both directions.
//!
//! Hand-written for the reason the hashes are: this is in the path that
//! signs requests against an account, and every dependency there is one
//! more thing trusted with the secret.
//!
//! Decoding exists because one venue's API secret *is* base64 — Kraken
//! Futures hands out an encoded key and signs with the decoded bytes, so
//! a client that skipped the decode would sign with the wrong key and
//! be told only that the signature was invalid.

/// Encode, with padding.
#[must_use]
pub(crate) fn encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Decode, rejecting anything that is not valid base64.
///
/// `None` rather than a partial result: a key that decoded to the wrong
/// bytes would sign every request incorrectly, and the venue reports
/// that as an invalid signature — which sends the reader looking at the
/// signing algorithm rather than at the key.
#[must_use]
pub(crate) fn decode(text: &str) -> Option<Vec<u8>> {
    let mut accumulator: u32 = 0;
    let mut bits = 0u32;
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let mut padding = 0usize;
    for c in text.bytes() {
        if c == b'\n' || c == b'\r' {
            continue;
        }
        if c == b'=' {
            padding += 1;
            continue;
        }
        // Padding is only ever at the end.
        if padding > 0 {
            return None;
        }
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        accumulator = (accumulator << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((accumulator >> bits) & 0xff) as u8);
        }
    }
    if padding > 2 {
        return None;
    }
    // Whatever is left over must be zero: a stray sixth bit means the
    // text was truncated, and a truncated key is a wrong key.
    if accumulator & ((1 << bits) - 1) != 0 {
        return None;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rfc_4648_vectors_round_trip() {
        for (plain, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(encode(plain.as_bytes()), encoded, "encoding {plain:?}");
            assert_eq!(
                decode(encoded).as_deref(),
                Some(plain.as_bytes()),
                "decoding {encoded:?}"
            );
        }
    }

    #[test]
    fn a_key_that_is_not_base64_is_refused_rather_than_half_decoded() {
        // The failure this guards: a mistyped secret that decodes to
        // *something* signs every request wrongly, and the venue reports
        // an invalid signature — which sends the reader to the algorithm
        // instead of to the key.
        assert_eq!(decode("not base64!"), None);
        assert_eq!(decode("Zg==="), None, "too much padding");
        assert_eq!(decode("Zm9v=Yg"), None, "padding in the middle");
    }

    #[test]
    fn arbitrary_bytes_survive_a_round_trip() {
        let bytes: Vec<u8> = (0..=255u8).collect();
        assert_eq!(decode(&encode(&bytes)).as_deref(), Some(bytes.as_slice()));
    }
}
