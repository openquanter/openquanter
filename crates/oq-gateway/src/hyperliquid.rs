//! Hyperliquid.
//!
//! The seventh venue, and the one that signs like a wallet rather than
//! like an API client.
//!
//! # There is no API secret
//!
//! Every other venue here authenticates with a key the venue also
//! holds. This one authenticates with an **EIP-712 signature** from a
//! private key that only this side has: there is nothing to compare
//! against, the venue recovers the address from the signature and that
//! is the identity. A wallet key moves funds, where an API key places
//! orders, which is why `docs/VENUES.md` treats reaching this venue as
//! a decision rather than a task.
//!
//! # The signature is four steps and one of them is msgpack
//!
//! 1. The action is encoded with **MessagePack**, field order
//!    significant.
//! 2. The nonce and an optional vault address are appended.
//! 3. Keccak-256 of that is the `connectionId`.
//! 4. It goes into an EIP-712 `Agent` struct — `source` is `"a"` on
//!    mainnet and `"b"` on testnet — and the typed-data digest is
//!    signed with secp256k1.
//!
//! Four places to be wrong, and the venue reports all of them the same
//! way. **So this is checked against the venue's own published test
//! vectors**, from `hyperliquid-python-sdk`'s `signing_test.py`: a
//! known key, a known action, a known `r`, `s` and `v`. That makes this
//! the only adapter here whose signing is verified rather than merely
//! written — the same standard `oq-hash` holds itself to with RFC 4231.
//!
//! # An asset is a number
//!
//! Orders reference assets by their index in the `meta` response's
//! `universe`, not by symbol, with spot at `10000 + index`. The mapping
//! is fetched and kept; nothing in `Instrument` carries it.
//!
//! # Where this commit stops
//!
//! The signing layer and the answer reader, both verified. `Execution`
//! is not implemented yet and the reason is a third client-id
//! constraint: cancelling by the caller's own id needs a `cloid`, which
//! is a 128-bit hex string — narrower than Binance's 36 characters,
//! Kraken's 100, and different again from Backpack's `uint32`. That is
//! a fourth shape for `IdRules` to carry, and it belongs in the commit
//! that uses it rather than this one. Placing also needs the asset
//! *index* for a symbol, which is a `/info` call and a cache.
//!
//! # What this has not done
//!
//! **It has not been run against Hyperliquid.** The signing is verified
//! against published vectors, which is more than the other adapters
//! have, and it is still not a placed order. NautilusTrader's adapter
//! documents what a first run costs here: a price with more than five
//! significant figures fails as `user or API wallet does not exist`,
//! because over-precision breaks signature verification and the venue
//! reports it as a missing wallet. There is a guard for that below and
//! it is written from someone else's incident, not from one here.

use core::time::Duration;

use k256::ecdsa::{RecoveryId, SigningKey, signature::hazmat::PrehashSigner};
use sha3::{Digest, Keccak256};

use crate::VenueError;
use crate::creds::Credentials;
use crate::exec::{Endpoint, Placed, Reject, Unresolved};

/// Keccak-256, which is Ethereum's hash and not SHA3-256.
///
/// The two differ only in a padding byte and produce completely
/// different digests, which is the kind of mistake that looks like a
/// wrong key.
#[must_use]
pub fn keccak256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Keccak256::new();
    hasher.update(data);
    hasher.finalize().into()
}

// ---------------------------------------------------------------------
// MessagePack, enough of it.
// ---------------------------------------------------------------------

/// A value in an action, in the order the venue hashes it.
///
/// A list of pairs rather than a map, because **field order is part of
/// the signature**: a map that sorted its keys would produce a
/// different hash from the venue's and every request would be refused
/// for a bad signature.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Str(String),
    Bool(bool),
    Uint(u64),
    Int(i64),
    Map(Vec<(String, Value)>),
    Array(Vec<Value>),
    Nil,
}

impl Value {
    /// A map from pairs, keeping the order given.
    #[must_use]
    pub fn map(pairs: Vec<(&str, Self)>) -> Self {
        Self::Map(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }

    /// A string.
    #[must_use]
    pub fn str(text: impl Into<String>) -> Self {
        Self::Str(text.into())
    }
}

/// Encode a value as MessagePack.
///
/// Only the shapes an action uses, and each one in its shortest form,
/// because that is what the reference implementation emits and the
/// bytes are hashed.
pub fn pack(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Nil => out.push(0xc0),
        Value::Bool(false) => out.push(0xc2),
        Value::Bool(true) => out.push(0xc3),
        Value::Uint(n) => pack_uint(*n, out),
        Value::Int(n) => {
            if *n >= 0 {
                pack_uint(u64::try_from(*n).unwrap_or(0), out);
            } else if *n >= -32 {
                #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
                out.push((*n as i8) as u8);
            } else if *n >= i64::from(i8::MIN) {
                out.push(0xd0);
                #[allow(clippy::cast_possible_truncation)]
                out.push((*n as i8) as u8);
            } else if *n >= i64::from(i16::MIN) {
                out.push(0xd1);
                #[allow(clippy::cast_possible_truncation)]
                out.extend_from_slice(&(*n as i16).to_be_bytes());
            } else if *n >= i64::from(i32::MIN) {
                out.push(0xd2);
                #[allow(clippy::cast_possible_truncation)]
                out.extend_from_slice(&(*n as i32).to_be_bytes());
            } else {
                out.push(0xd3);
                out.extend_from_slice(&n.to_be_bytes());
            }
        }
        Value::Str(s) => {
            let bytes = s.as_bytes();
            let len = bytes.len();
            if len < 32 {
                #[allow(clippy::cast_possible_truncation)]
                out.push(0xa0 | (len as u8));
            } else if len < 256 {
                out.push(0xd9);
                #[allow(clippy::cast_possible_truncation)]
                out.push(len as u8);
            } else {
                out.push(0xda);
                #[allow(clippy::cast_possible_truncation)]
                out.extend_from_slice(&(len as u16).to_be_bytes());
            }
            out.extend_from_slice(bytes);
        }
        Value::Array(items) => {
            let len = items.len();
            if len < 16 {
                #[allow(clippy::cast_possible_truncation)]
                out.push(0x90 | (len as u8));
            } else {
                out.push(0xdc);
                #[allow(clippy::cast_possible_truncation)]
                out.extend_from_slice(&(len as u16).to_be_bytes());
            }
            for item in items {
                pack(item, out);
            }
        }
        Value::Map(pairs) => {
            let len = pairs.len();
            if len < 16 {
                #[allow(clippy::cast_possible_truncation)]
                out.push(0x80 | (len as u8));
            } else {
                out.push(0xde);
                #[allow(clippy::cast_possible_truncation)]
                out.extend_from_slice(&(len as u16).to_be_bytes());
            }
            for (k, v) in pairs {
                pack(&Value::Str(k.clone()), out);
                pack(v, out);
            }
        }
    }
}

fn pack_uint(n: u64, out: &mut Vec<u8>) {
    if n < 128 {
        #[allow(clippy::cast_possible_truncation)]
        out.push(n as u8);
    } else if n <= u64::from(u8::MAX) {
        out.push(0xcc);
        #[allow(clippy::cast_possible_truncation)]
        out.push(n as u8);
    } else if n <= u64::from(u16::MAX) {
        out.push(0xcd);
        #[allow(clippy::cast_possible_truncation)]
        out.extend_from_slice(&(n as u16).to_be_bytes());
    } else if n <= u64::from(u32::MAX) {
        out.push(0xce);
        #[allow(clippy::cast_possible_truncation)]
        out.extend_from_slice(&(n as u32).to_be_bytes());
    } else {
        out.push(0xcf);
        out.extend_from_slice(&n.to_be_bytes());
    }
}

// ---------------------------------------------------------------------
// The signature.
// ---------------------------------------------------------------------

/// The `connectionId`: what the EIP-712 struct actually commits to.
///
/// MessagePack of the action, then the nonce as eight big-endian bytes,
/// then one byte saying whether a vault address follows. An absent
/// `expiresAfter` adds nothing, which is why it is not a parameter here.
#[must_use]
pub fn action_hash(action: &Value, nonce: u64, vault: Option<[u8; 20]>) -> [u8; 32] {
    let mut data = Vec::with_capacity(128);
    pack(action, &mut data);
    data.extend_from_slice(&nonce.to_be_bytes());
    match vault {
        None => data.push(0x00),
        Some(address) => {
            data.push(0x01);
            data.extend_from_slice(&address);
        }
    }
    keccak256(&data)
}

/// The EIP-712 digest for an L1 action.
///
/// The domain is fixed by the venue: name `Exchange`, version `1`,
/// chain id 1337, and the zero address. Those are not this chain's real
/// values — the signature is a proof of key ownership rather than a
/// transaction — and copying a real chain id here would produce a
/// signature the venue rejects.
#[must_use]
pub fn l1_digest(connection_id: &[u8; 32], mainnet: bool) -> [u8; 32] {
    let domain_typehash = keccak256(
        b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)",
    );
    let mut domain = Vec::with_capacity(160);
    domain.extend_from_slice(&domain_typehash);
    domain.extend_from_slice(&keccak256(b"Exchange"));
    domain.extend_from_slice(&keccak256(b"1"));
    domain.extend_from_slice(&u256(1337));
    domain.extend_from_slice(&[0u8; 32]);
    let domain_separator = keccak256(&domain);

    // `a` on mainnet, `b` on testnet. One character, and the wrong one
    // signs a valid message for the other deployment.
    let source: &[u8] = if mainnet { b"a" } else { b"b" };
    let agent_typehash = keccak256(b"Agent(string source,bytes32 connectionId)");
    let mut agent = Vec::with_capacity(96);
    agent.extend_from_slice(&agent_typehash);
    agent.extend_from_slice(&keccak256(source));
    agent.extend_from_slice(connection_id);
    let struct_hash = keccak256(&agent);

    let mut prefixed = Vec::with_capacity(66);
    prefixed.extend_from_slice(&[0x19, 0x01]);
    prefixed.extend_from_slice(&domain_separator);
    prefixed.extend_from_slice(&struct_hash);
    keccak256(&prefixed)
}

fn u256(n: u64) -> [u8; 32] {
    let mut out = [0u8; 32];
    out[24..].copy_from_slice(&n.to_be_bytes());
    out
}

/// An EIP-712 signature, in the shape the venue's payload wants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    pub r: [u8; 32],
    pub s: [u8; 32],
    /// 27 or 28, which is Ethereum's recovery id rather than 0 or 1.
    pub v: u8,
}

impl Signature {
    /// `0x`-prefixed hex with leading zeroes removed, which is how the
    /// venue's own clients render `r` and `s`.
    #[must_use]
    pub fn r_hex(&self) -> String {
        trimmed_hex(&self.r)
    }

    /// The same for `s`.
    #[must_use]
    pub fn s_hex(&self) -> String {
        trimmed_hex(&self.s)
    }
}

fn trimmed_hex(bytes: &[u8; 32]) -> String {
    let full: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("0x{}", full.trim_start_matches('0'))
}

/// Sign an L1 action with a wallet key.
///
/// # Errors
/// When the key is not a valid secp256k1 scalar, or the curve
/// implementation declines to sign.
pub fn sign_l1_action(
    key: &SigningKey,
    action: &Value,
    nonce: u64,
    vault: Option<[u8; 20]>,
    mainnet: bool,
) -> Result<Signature, String> {
    let connection_id = action_hash(action, nonce, vault);
    let digest = l1_digest(&connection_id, mainnet);
    let (signature, recovery): (k256::ecdsa::Signature, RecoveryId) = key
        .sign_prehash(&digest)
        .map_err(|e| format!("this key could not sign: {e}"))?;
    // Ethereum rejects the high half of the curve order, and `k256`
    // normalises to the low one — but the recovery id has to follow it,
    // so it is taken from the same call rather than assumed.
    let r: [u8; 32] = signature.r().to_bytes().into();
    let s: [u8; 32] = signature.s().to_bytes().into();
    Ok(Signature {
        r,
        s,
        v: 27 + recovery.to_byte(),
    })
}

/// Read a wallet key out of credentials.
///
/// # Errors
/// When the secret is not 32 bytes of hex, with or without `0x`.
pub fn signing_key(creds: &Credentials) -> Result<SigningKey, String> {
    let text = core::str::from_utf8(creds.secret_bytes())
        .map_err(|_| "a wallet key is hex text".to_string())?;
    let text = text.trim().trim_start_matches("0x");
    if text.len() != 64 {
        return Err("a wallet key is 32 bytes of hex".to_string());
    }
    let mut bytes = [0u8; 32];
    for (i, pair) in text.as_bytes().chunks(2).enumerate() {
        let hex = core::str::from_utf8(pair).map_err(|_| "a wallet key is hex".to_string())?;
        bytes[i] = u8::from_str_radix(hex, 16).map_err(|_| "a wallet key is hex".to_string())?;
    }
    SigningKey::from_bytes(&bytes.into()).map_err(|e| format!("not a usable wallet key: {e}"))
}

// ---------------------------------------------------------------------
// Orders.
// ---------------------------------------------------------------------

/// Five significant figures, which is what this venue accepts.
///
/// Over-precision does not come back as a price error. It breaks
/// signature verification, and the venue answers `user or API wallet
/// does not exist` — an error that sends the reader to the credentials.
/// Written from NautilusTrader's account of its own first run.
#[must_use]
pub fn within_five_significant_figures(price: &str) -> bool {
    let digits: String = price
        .chars()
        .filter(char::is_ascii_digit)
        .collect::<String>()
        .trim_start_matches('0')
        .to_string();
    digits.trim_end_matches('0').len() <= 5
}

/// One order, as the wire wants it.
///
/// Keys are one letter and the order of them is part of the hash.
#[must_use]
pub fn order_wire(
    asset: u32,
    is_buy: bool,
    price: &str,
    size: &str,
    reduce_only: bool,
    tif: &str,
) -> Value {
    Value::map(vec![
        ("a", Value::Uint(u64::from(asset))),
        ("b", Value::Bool(is_buy)),
        ("p", Value::str(price)),
        ("s", Value::str(size)),
        ("r", Value::Bool(reduce_only)),
        (
            "t",
            Value::map(vec![("limit", Value::map(vec![("tif", Value::str(tif))]))]),
        ),
    ])
}

/// The action that places orders.
#[must_use]
pub fn order_action(orders: Vec<Value>) -> Value {
    Value::map(vec![
        ("type", Value::str("order")),
        ("orders", Value::Array(orders)),
        ("grouping", Value::str("na")),
    ])
}

/// A client for one Hyperliquid deployment.
pub struct Hyperliquid {
    base: String,
    key: SigningKey,
    mainnet: bool,
    agent: ureq::Agent,
}

impl Hyperliquid {
    /// Production.
    pub const MAINNET: &'static str = "https://api.hyperliquid.xyz";
    /// The test deployment.
    pub const TESTNET: &'static str = "https://api.hyperliquid-testnet.xyz";

    /// Build a client.
    ///
    /// # Errors
    /// When the secret is not a wallet key.
    pub fn at(endpoint: Endpoint, creds: &Credentials) -> Result<Self, String> {
        let mainnet = matches!(endpoint, Endpoint::Live);
        Ok(Self {
            base: if mainnet {
                Self::MAINNET.to_string()
            } else {
                Self::TESTNET.to_string()
            },
            key: signing_key(creds)?,
            mainnet,
            agent: ureq::Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(45)))
                .http_status_as_error(false)
                .build()
                .into(),
        })
    }

    /// Post a signed action to `/exchange`.
    ///
    /// # Errors
    /// Whatever the transport reports, or a key that cannot sign.
    pub fn post_action(&self, action: &Value, nonce: u64) -> Result<String, VenueError> {
        let signature = sign_l1_action(&self.key, action, nonce, None, self.mainnet)
            .map_err(VenueError::Transport)?;
        let mut body = String::from("{\"action\":");
        body.push_str(&to_json(action));
        body.push_str(&format!(
            ",\"nonce\":{nonce},\"signature\":{{\"r\":\"{}\",\"s\":\"{}\",\"v\":{}}}}}",
            signature.r_hex(),
            signature.s_hex(),
            signature.v
        ));
        let mut response = self
            .agent
            .post(format!("{}/exchange", self.base))
            .header("Content-Type", "application/json")
            .send(&body)
            .map_err(|e| VenueError::Transport(e.to_string()))?;
        let status = response.status().as_u16();
        let text = response
            .body_mut()
            .read_to_string()
            .map_err(|e| VenueError::Transport(e.to_string()))?;
        if (200..300).contains(&status) {
            Ok(text)
        } else {
            Err(VenueError::Venue { status, body: text })
        }
    }
}

/// The same value as JSON, for the request body.
///
/// The body is JSON and the *signature* is over MessagePack of the same
/// value, which is the venue's design and not a choice here. Both
/// encoders walk one `Value`, so the two cannot describe different
/// orders.
#[must_use]
pub fn to_json(value: &Value) -> String {
    match value {
        Value::Nil => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Uint(n) => n.to_string(),
        Value::Int(n) => n.to_string(),
        Value::Str(s) => format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")),
        Value::Array(items) => {
            let inner: Vec<String> = items.iter().map(to_json).collect();
            format!("[{}]", inner.join(","))
        }
        Value::Map(pairs) => {
            let inner: Vec<String> = pairs
                .iter()
                .map(|(k, v)| format!("\"{k}\":{}", to_json(v)))
                .collect();
            format!("{{{}}}", inner.join(","))
        }
    }
}

/// What an `/exchange` answer meant.
///
/// Rejections arrive inside a 200: `status` is `ok` and the refusal is
/// an `error` entry in `response.data.statuses`. The fourth venue here
/// to answer that way.
#[must_use]
pub fn classify(status: u16, body: &str, client_id: &str) -> Placed {
    if !(200..300).contains(&status) {
        return Placed::Unknown(Unresolved {
            client_id: client_id.to_string(),
            reason: format!("HTTP {status}: {}", truncate(body)),
        });
    }
    if !body.contains("\"status\"") {
        return Placed::Unknown(Unresolved {
            client_id: client_id.to_string(),
            reason: format!("unreadable answer: {}", truncate(body)),
        });
    }
    // An `err` member at the envelope level is the action failing.
    if let Some(err) = crate::json::field_str(body, "err") {
        return Placed::Rejected(Reject {
            code: None,
            message: err,
        });
    }
    // One status per order in the batch; this adapter sends one.
    if let Some(entry) = crate::json::object_containing(body, "\"error\"") {
        return Placed::Rejected(Reject {
            code: None,
            message: crate::json::field_str(&entry, "error").unwrap_or_else(|| truncate(&entry)),
        });
    }
    let resting = crate::json::object_containing(body, "\"resting\"");
    let filled = crate::json::object_containing(body, "\"filled\"");
    let Some(entry) = resting.or(filled) else {
        return Placed::Unknown(Unresolved {
            client_id: client_id.to_string(),
            reason: format!("a status this build does not know: {}", truncate(body)),
        });
    };
    let venue_id = crate::json::raw_field(&entry, "oid").unwrap_or_default();
    Placed::Accepted(crate::exec::OrderAck {
        venue_id,
        client_id: client_id.to_string(),
        status: if entry.contains("\"filled\"") {
            "filled".to_string()
        } else {
            "resting".to_string()
        },
        executed_qty: crate::json::field_str(&entry, "totalSz").unwrap_or_else(|| "0".to_string()),
    })
}

fn truncate(body: &str) -> String {
    body.chars().take(200).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The key the venue's own test suite signs with.
    const TEST_KEY: &str = "0123456789012345678901234567890123456789012345678901234567890123";

    fn key() -> SigningKey {
        let creds = Credentials::new("k", TEST_KEY);
        signing_key(&creds).expect("a valid wallet key")
    }

    #[test]
    fn the_published_l1_signing_vector_on_mainnet() {
        // hyperliquid-python-sdk, tests/signing_test.py,
        // `test_l1_action_signing_matches`. Four steps — msgpack, the
        // nonce and vault byte, Keccak-256, EIP-712 — and every one of
        // them has to be right for this to hold. The venue reports all
        // four failures identically.
        let action = Value::map(vec![
            ("type", Value::str("dummy")),
            // `float_to_int_for_hashing(1000)` in the reference: a
            // price or size is multiplied by 1e8 before hashing.
            ("num", Value::Uint(100_000_000_000)),
        ]);
        let signature = sign_l1_action(&key(), &action, 0, None, true).expect("signed");
        assert_eq!(
            signature.r_hex(),
            "0x53749d5b30552aeb2fca34b530185976545bb22d0b3ce6f62e31be961a59298"
        );
        assert_eq!(
            signature.s_hex(),
            "0x755c40ba9bf05223521753995abb2f73ab3229be8ec921f350cb447e384d8ed8"
        );
        assert_eq!(signature.v, 27);
    }

    #[test]
    fn the_published_l1_signing_vector_on_testnet() {
        // The same action and the same key, one character different in
        // the signed struct — `source` is `b` rather than `a`. A client
        // that got this backwards would sign perfectly valid requests
        // for the wrong deployment.
        let action = Value::map(vec![
            ("type", Value::str("dummy")),
            ("num", Value::Uint(100_000_000_000)),
        ]);
        let signature = sign_l1_action(&key(), &action, 0, None, false).expect("signed");
        assert_eq!(
            signature.r_hex(),
            "0x542af61ef1f429707e3c76c5293c80d01f74ef853e34b76efffcb57e574f9510"
        );
        assert_eq!(
            signature.s_hex(),
            "0x17b8b32f086e8cdede991f1e2c529f5dd5297cbe8128500e00cbaf766204a613"
        );
        assert_eq!(signature.v, 28);
    }

    #[test]
    fn the_published_order_action_hash() {
        // `test_phantom_agent_creation_matches_production`: a real
        // order wire, hashed. This is the msgpack encoding under test —
        // field order, the short forms, the string lengths.
        let wire = order_wire(4, true, "1670.1", "0.0147", false, "Ioc");
        let action = order_action(vec![wire]);
        let hash = action_hash(&action, 1_677_777_606_040, None);
        let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex, "0fcbeda5ae3c4950a548021552a4fea2226858c4453571bf3f24ba017eac2908",
            "the connectionId the venue's own suite pins"
        );
    }

    #[test]
    fn keccak_is_not_sha3() {
        // They differ in one padding byte and share nothing else. A
        // client that used SHA3-256 here would produce a signature over
        // a digest the venue never computes.
        let hex: String = keccak256(b"").iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex, "c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470",
            "Keccak-256 of the empty string"
        );
    }

    #[test]
    fn a_map_keeps_the_order_it_was_given() {
        // Sorting would produce a different hash from the venue's, and
        // every request would come back as a bad signature.
        let mut sorted = Vec::new();
        pack(
            &Value::map(vec![("a", Value::Uint(1)), ("b", Value::Uint(2))]),
            &mut sorted,
        );
        let mut reversed = Vec::new();
        pack(
            &Value::map(vec![("b", Value::Uint(2)), ("a", Value::Uint(1))]),
            &mut reversed,
        );
        assert_ne!(sorted, reversed);
    }

    #[test]
    fn an_over_precise_price_is_caught_here_rather_than_by_the_venue() {
        // The venue answers over-precision with `user or API wallet does
        // not exist`, because it breaks signature verification. Written
        // from another implementation's first-run report.
        assert!(within_five_significant_figures("1670.1"));
        assert!(within_five_significant_figures("78313"));
        assert!(within_five_significant_figures("0.0147"));
        assert!(!within_five_significant_figures("78313.4"));
        assert!(!within_five_significant_figures("1234567"));
    }

    #[test]
    fn the_body_and_the_signature_describe_one_order() {
        // The request is JSON and the signature is over MessagePack of
        // the same value. Both encoders walk the same `Value`, so they
        // cannot disagree about what was sent.
        let wire = order_wire(4, true, "1670.1", "0.0147", false, "Ioc");
        let json = to_json(&order_action(vec![wire]));
        assert!(
            json.starts_with(r#"{"type":"order","orders":[{"a":4,"b":true"#),
            "{json}"
        );
        assert!(json.ends_with(r#""grouping":"na"}"#), "{json}");
    }
}
