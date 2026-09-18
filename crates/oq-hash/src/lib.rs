//! Content hashing primitives.
//!
//! Three consumers, three different needs. Archived data is identified by a
//! **SHA-256** content hash: it decides whether a parity baseline still
//! describes the same experiment, so collision resistance is the point.
//! Individual capture records carry a **CRC-32**: it decides whether the
//! last record of a file was torn by a crash, so speed is the point and
//! an adversary is not in the picture. And one venue signs with
//! **SHA-512**, which is here for the same reason the others are and
//! with the same limit: a hash is deterministic arithmetic against
//! published vectors, and curve arithmetic is not — see docs/VENUES.md
//! for where that line is drawn.
//!
//! Implemented here rather than taken as dependencies. These sit under
//! the verification chain, and a verification tool that cannot be built
//! from the workspace alone is a weak link.

pub mod crc32;
pub mod hmac;
pub mod sha256;
pub mod sha512;

pub use crc32::crc32;
pub use hmac::{hmac_sha256, hmac_sha512};
pub use sha256::{Sha256, sha256_hex, to_hex};
pub use sha512::{Sha512, sha512_hex};
