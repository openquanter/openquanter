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
//! # A client id is a cloid
//!
//! Cancelling by the caller's own id needs a `cloid`, a 128-bit hex
//! string — narrower than Binance's 36 characters, Kraken's 100, and
//! different again from Backpack's `uint32`: the fourth shape
//! `IdRules` carries. Placing also needs the asset *index* for a
//! symbol, which is a `/info` call and a cache.
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

use k256::ecdsa::{RecoveryId, SigningKey, signature::hazmat::PrehashSigner};
use sha3::{Digest, Keccak256};

use crate::VenueError;
use crate::creds::Credentials;
use oq_types::Instrument;

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
    Map(Vec<(String, Value)>),
    Array(Vec<Value>),
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
///
/// Narrower than MessagePack: no signed integers and no nil. An action
/// has neither — every number in one is a positive integer, and an
/// absent field is omitted rather than nulled. The variants were
/// written anyway and the reachability check refused them, correctly:
/// an encoding nothing produces is an encoding nothing has tested.
pub fn pack(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Bool(false) => out.push(0xc2),
        Value::Bool(true) => out.push(0xc3),
        Value::Uint(n) => pack_uint(*n, out),
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
    if !price.contains('.') {
        return true;
    }
    let digits: String = price
        .chars()
        .filter(char::is_ascii_digit)
        .collect::<String>()
        .trim_start_matches('0')
        .to_string();
    digits.trim_end_matches('0').len() <= 5
}

/// Validate a perpetual price against both limits published in `meta`.
#[must_use]
pub fn valid_perp_price(price: &str, sz_decimals: u8) -> bool {
    let decimals = price
        .split_once('.')
        .map_or(0, |(_, fraction)| fraction.trim_end_matches('0').len());
    within_five_significant_figures(price)
        && decimals <= usize::from(6_u8.saturating_sub(sz_decimals))
}

fn canonical_decimal(value: i64, scale: u8) -> String {
    let decimal = crate::exec::decimal(value, scale);
    if !decimal.contains('.') {
        return decimal;
    }
    let trimmed = decimal.trim_end_matches('0').trim_end_matches('.');
    if trimmed.is_empty() || trimmed == "-" {
        "0".to_string()
    } else {
        trimmed.to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AssetMeta {
    name: String,
    sz_decimals: u8,
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

/// Whose account an action is placed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActingFor {
    /// Not yet asked. Nothing is signed in this state: signing without a
    /// vault places the order in the signer's own master account, which
    /// for a subaccount is a different account from the one this client
    /// reads.
    Unresolved,
    /// The account is a master; actions carry no vault address.
    Master,
    /// The account is a subaccount or a vault, and every action names it.
    Vault([u8; 20]),
}

/// What `userRole` says an account address is, as it bears on signing.
///
/// # Errors
/// An address the venue does not know, one that is an agent (an API
/// wallet, whose address is the signer and never the account), or an
/// answer this build cannot read.
pub fn acting_for(body: &str, account: &str) -> Result<ActingFor, String> {
    match crate::json::field_str(body, "role").as_deref() {
        Some("user") => Ok(ActingFor::Master),
        Some("subAccount" | "vault") => Ok(ActingFor::Vault(address_bytes(account)?)),
        Some("agent") => Err(format!(
            "{account} is an API wallet; OQ_VENUE_KEY must be the account it trades for"
        )),
        Some("missing") => Err(format!("{account} has no account on this deployment")),
        _ => Err(format!("unreadable userRole answer: {}", truncate(body))),
    }
}

fn address_bytes(address: &str) -> Result<[u8; 20], String> {
    let hex = address.trim_start_matches("0x");
    let mut out = [0u8; 20];
    if hex.len() != 40 {
        return Err(format!("not a 20-byte address: {address}"));
    }
    for (i, pair) in hex.as_bytes().chunks(2).enumerate() {
        let text = core::str::from_utf8(pair).map_err(|_| format!("not hex: {address}"))?;
        out[i] = u8::from_str_radix(text, 16).map_err(|_| format!("not hex: {address}"))?;
    }
    Ok(out)
}

/// A client for one Hyperliquid deployment.
pub struct Hyperliquid {
    base: String,
    key: SigningKey,
    /// Master or subaccount whose state the signer is authorised to trade.
    account_address: String,
    /// Whom an action is signed on behalf of, once the venue has said
    /// what `account_address` is.
    acting_for: ActingFor,
    mainnet: bool,
    agent: ureq::Agent,
    /// Asset metadata in index order. Empty until `load_universe`.
    universe: Vec<AssetMeta>,
}

impl Hyperliquid {
    /// Production.
    pub const MAINNET: &'static str = "https://api.hyperliquid.xyz";
    /// The test deployment.
    pub const TESTNET: &'static str = "https://api.hyperliquid-testnet.xyz";

    /// Build a client.
    ///
    /// # Errors
    /// When the key is not the master/subaccount address, or the secret
    /// is not the API wallet's signing key.
    pub fn at(endpoint: Endpoint, creds: &Credentials) -> Result<Self, String> {
        let mainnet = matches!(endpoint, Endpoint::Live);
        let account_address = normalize_address(creds.key())?;
        Ok(Self {
            base: if mainnet {
                Self::MAINNET.to_string()
            } else {
                Self::TESTNET.to_string()
            },
            key: signing_key(creds)?,
            account_address,
            acting_for: ActingFor::Unresolved,
            mainnet,
            agent: crate::http::venue_agent(),
            universe: Vec::new(),
        })
    }

    /// Post a signed action to `/exchange`.
    ///
    /// # Errors
    /// Whatever the transport reports, or a key that cannot sign.
    pub fn post_action(&self, action: &Value, nonce: u64) -> Result<String, VenueError> {
        let vault = match self.acting_for {
            ActingFor::Unresolved => {
                return Err(VenueError::Transport(
                    "the account's role is not resolved; call load_universe first".to_string(),
                ));
            }
            ActingFor::Master => None,
            ActingFor::Vault(address) => Some(address),
        };
        let body = action_body(action, nonce, vault, &self.key, self.mainnet)
            .map_err(VenueError::Transport)?;
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

/// The `/exchange` request body for a signed action.
///
/// The vault address goes into the signature *and* the body, as the
/// venue's own SDK sends it; one without the other is refused, or worse,
/// accepted on the signer's master account.
///
/// # Errors
/// A key that cannot sign.
pub fn action_body(
    action: &Value,
    nonce: u64,
    vault: Option<[u8; 20]>,
    key: &SigningKey,
    mainnet: bool,
) -> Result<String, String> {
    let signature = sign_l1_action(key, action, nonce, vault, mainnet)?;
    let mut body = String::from("{\"action\":");
    body.push_str(&to_json(action));
    body.push_str(&format!(
        ",\"nonce\":{nonce},\"signature\":{{\"r\":\"{}\",\"s\":\"{}\",\"v\":{}}}",
        signature.r_hex(),
        signature.s_hex(),
        signature.v
    ));
    if let Some(address) = vault {
        body.push_str(&format!(",\"vaultAddress\":\"{}\"", address_hex(&address)));
    }
    body.push('}');
    Ok(body)
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
        Value::Bool(b) => b.to_string(),
        Value::Uint(n) => n.to_string(),
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
    // The action failing as a whole: `"status":"err"`, with the reason
    // as the `response` string. This read a key named `err`, which the
    // venue never sends, so a refused action fell through to "a status
    // this build does not know".
    if let Some(reason) = envelope_error(body) {
        return Placed::Rejected(Reject {
            code: None,
            message: reason,
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

/// The reason an action failed as a whole, when it did.
fn envelope_error(body: &str) -> Option<String> {
    (crate::json::field_str(body, "status").as_deref() == Some("err"))
        .then(|| crate::json::field_str(body, "response").unwrap_or_else(|| truncate(body)))
}

/// What a cancel answer meant.
///
/// Not [`classify`]: a placement succeeds as `resting` or `filled`, and a
/// cancel succeeds as the bare string `"success"` — which the placement
/// reader did not recognise, so every successful cancel came back
/// unknown. A refusal is an `error` entry, as for a placement.
#[must_use]
pub fn classify_cancel(status: u16, body: &str, client_id: &str) -> Placed {
    if !(200..300).contains(&status) {
        return Placed::Unknown(Unresolved {
            client_id: client_id.to_string(),
            reason: format!("HTTP {status}: {}", truncate(body)),
        });
    }
    if let Some(reason) = envelope_error(body) {
        return Placed::Rejected(Reject {
            code: None,
            message: reason,
        });
    }
    let Some(statuses) = crate::json::array_field(body, "statuses") else {
        return Placed::Unknown(Unresolved {
            client_id: client_id.to_string(),
            reason: format!("unreadable answer: {}", truncate(body)),
        });
    };
    if let Some(entry) = crate::json::object_containing(&statuses, "\"error\"") {
        return Placed::Rejected(Reject {
            code: None,
            message: crate::json::field_str(&entry, "error").unwrap_or_else(|| truncate(&entry)),
        });
    }
    if statuses.contains("\"success\"") {
        return Placed::Accepted(crate::exec::OrderAck {
            venue_id: String::new(),
            client_id: client_id.to_string(),
            status: "cancelled".to_string(),
            executed_qty: "0".to_string(),
        });
    }
    Placed::Unknown(Unresolved {
        client_id: client_id.to_string(),
        reason: format!(
            "a cancel status this build does not know: {}",
            truncate(body)
        ),
    })
}

fn truncate(body: &str) -> String {
    body.chars().take(200).collect()
}

// ---------------------------------------------------------------------
// Assets, identity and orders.
// ---------------------------------------------------------------------

/// Asset names in index order, from `/info` with `{"type":"meta"}`.
///
/// The index *is* the position in this list — orders reference assets
/// by number, not by name — so the order of the response is load-
/// bearing and a reordering on the venue's side silently repoints every
/// symbol. It is read fresh at startup rather than cached to disk for
/// that reason.
#[must_use]
pub fn parse_universe(body: &str) -> Vec<String> {
    parse_universe_meta(body)
        .into_iter()
        .map(|asset| asset.name)
        .collect()
}

fn parse_universe_meta(body: &str) -> Vec<AssetMeta> {
    let Some(list) = crate::json::array_field(body, "universe") else {
        return Vec::new();
    };
    crate::json::objects(&list)
        .iter()
        .filter_map(|o| {
            Some(AssetMeta {
                name: crate::json::field_str(o, "name")?,
                sz_decimals: crate::json::raw_field(o, "szDecimals")?.parse().ok()?,
            })
        })
        .collect()
}

fn normalize_address(address: &str) -> Result<String, String> {
    let address = address.trim();
    let Some(hex) = address
        .strip_prefix("0x")
        .or_else(|| address.strip_prefix("0X"))
    else {
        return Err("OQ_VENUE_KEY must be the master or subaccount 0x address".to_string());
    };
    if hex.len() != 40 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("OQ_VENUE_KEY must be the master or subaccount 0x address".to_string());
    }
    Ok(format!("0x{}", hex.to_ascii_lowercase()))
}

/// The Ethereum address a wallet key signs as.
///
/// Keccak-256 of the uncompressed public key without its `0x04` tag,
/// last twenty bytes. The venue recovers this from the signature, so a
/// client that computed it differently would query one account and
/// trade another — which is the shape of the agent-wallet trap
/// NautilusTrader documents.
#[must_use]
pub fn address_of(key: &SigningKey) -> [u8; 20] {
    let point = key.verifying_key().to_encoded_point(false);
    let hash = keccak256(&point.as_bytes()[1..]);
    let mut address = [0u8; 20];
    address.copy_from_slice(&hash[12..]);
    address
}

/// That address as the venue writes it.
#[must_use]
pub fn address_hex(address: &[u8; 20]) -> String {
    let body: String = address.iter().map(|b| format!("{b:02x}")).collect();
    format!("0x{body}")
}

/// The action that cancels by the caller's own id.
#[must_use]
pub fn cancel_by_cloid_action(asset: u32, cloid: &str) -> Value {
    Value::map(vec![
        ("type", Value::str("cancelByCloid")),
        (
            "cancels",
            Value::Array(vec![Value::map(vec![
                ("asset", Value::Uint(u64::from(asset))),
                ("cloid", Value::str(cloid)),
            ])]),
        ),
    ])
}

impl Hyperliquid {
    /// Load the asset universe, which every order needs.
    ///
    /// # Errors
    /// Whatever the transport reports, or a meta response with no
    /// universe in it.
    ///
    /// Also asks what the account address is — master, subaccount or
    /// vault — which decides whose account every action is signed for.
    /// Until it has, nothing is sent.
    pub fn load_universe(&mut self) -> Result<usize, VenueError> {
        let role = self.info(&format!(
            r#"{{"type":"userRole","user":"{}"}}"#,
            self.account_address
        ))?;
        self.acting_for =
            acting_for(&role, &self.account_address).map_err(|reason| VenueError::Malformed {
                what: "the account's role",
                body: reason,
            })?;
        let body = self.info(r#"{"type":"meta"}"#)?;
        let assets = parse_universe_meta(&body);
        if assets.is_empty() {
            return Err(VenueError::Malformed {
                what: "the asset universe",
                body: body.chars().take(200).collect(),
            });
        }
        let count = assets.len();
        self.universe = assets;
        Ok(count)
    }

    /// The master or subaccount whose state this client trades.
    #[must_use]
    pub fn address(&self) -> String {
        self.account_address.clone()
    }

    /// The index an order must reference for this symbol.
    #[must_use]
    pub fn asset_of(&self, symbol: &str) -> Option<u32> {
        self.universe
            .iter()
            .position(|asset| asset.name == symbol)
            .and_then(|i| u32::try_from(i).ok())
    }

    fn asset_meta(&self, symbol: &str) -> Option<&AssetMeta> {
        self.universe.iter().find(|asset| asset.name == symbol)
    }

    /// Post to `/info`, which takes no signature.
    ///
    /// # Errors
    /// Whatever the transport reports.
    pub fn info(&self, body: &str) -> Result<String, VenueError> {
        let mut response = self
            .agent
            .post(format!("{}/info", self.base))
            .header("Content-Type", "application/json")
            .send(body)
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

// ---------------------------------------------------------------------
// Account reads.
// ---------------------------------------------------------------------

/// Read a `clearinghouseState` response.
///
/// # What is checked, and what is defined
///
/// Two different things, and the difference matters. The venue's own
/// numbers satisfy `accountValue = totalRawUsd + totalNtlPos`, so that
/// is *checked* — it is what confirms this build reads `marginSummary`
/// the way the venue writes it, and a response that fails it means the
/// fields are not what they are taken to be.
///
/// The wallet balance is then *defined*: `AccountSnapshot` means
/// wallet plus unrealized to equal the margin balance, so it is
/// `accountValue` minus the summed unrealized P&L. That is arithmetic
/// rather than evidence, and is not dressed up as a second check.
///
/// # Errors
/// When a field is missing, or the identity does not hold.
pub fn parse_clearinghouse(
    body: &str,
    read_at_ms: i64,
) -> Result<crate::binance::AccountSnapshot, VenueError> {
    // By name, not by field. `crossMarginSummary` carries the same
    // fields and is serialised first, so searching for `totalRawUsd`
    // finds it — and it satisfies the identity below too, which is
    // why this was invisible until a test read the value.
    let Some(summary) = crate::json::object_field(body, "marginSummary") else {
        return Err(VenueError::Malformed {
            what: "marginSummary",
            body: body.chars().take(200).collect(),
        });
    };
    let read = |key: &'static str| -> Result<f64, VenueError> {
        crate::json::field_str(&summary, key)
            .and_then(|v| v.parse::<f64>().ok())
            .ok_or(VenueError::Malformed {
                what: key,
                body: summary.chars().take(200).collect(),
            })
    };
    let account_value = read("accountValue")?;
    let raw_usd = read("totalRawUsd")?;
    let notional = read("totalNtlPos")?;
    let tolerance = 1e-6_f64.mul_add(account_value.abs().max(1.0), 1e-8);
    if (account_value - (raw_usd + notional)).abs() > tolerance {
        return Err(VenueError::Malformed {
            what: "the margin summary does not mean what this build reads it to mean",
            body: format!(
                "accountValue {account_value} is not totalRawUsd {raw_usd} plus \
                 totalNtlPos {notional}"
            ),
        });
    }
    // Summed from the legs: the summary carries no unrealized total.
    let unrealized: f64 = parse_asset_positions(body)
        .iter()
        .map(|p| p.unrealized)
        .sum();
    Ok(crate::binance::AccountSnapshot {
        wallet_balance: account_value - unrealized,
        unrealized,
        margin_balance: account_value,
        read_at_ms,
    })
}

/// Read the `assetPositions` of a `clearinghouseState` response.
///
/// `szi` is signed and there is no leg name — `type` is `oneWay` —
/// so the sign comes from the number, as it does on Backpack and for
/// the same reason.
#[must_use]
pub fn parse_asset_positions(body: &str) -> Vec<crate::binance::PositionSnapshot> {
    let Some(list) = crate::json::array_field(body, "assetPositions") else {
        return Vec::new();
    };
    crate::json::objects(&list)
        .iter()
        .filter_map(|entry| {
            let coin = crate::json::field_str(entry, "coin")?;
            let szi = crate::json::field_str(entry, "szi")?;
            let entry_text = crate::json::field_str(entry, "entryPx").unwrap_or_default();
            Some(crate::binance::PositionSnapshot {
                symbol: coin,
                position_side: "BOTH".to_string(),
                amount: szi.parse::<f64>().ok()?,
                amount_text: szi,
                entry_price: entry_text.parse::<f64>().unwrap_or_default(),
                entry_text,
                unrealized: crate::json::field_str(entry, "unrealizedPnl")
                    .and_then(|v| v.parse::<f64>().ok())
                    .unwrap_or_default(),
            })
        })
        .collect()
}

impl Hyperliquid {
    /// The account's balances.
    ///
    /// # Errors
    /// Whatever the transport reports, or a summary this build cannot
    /// confirm.
    pub fn balances(&self) -> Result<crate::binance::AccountSnapshot, VenueError> {
        let read_at = crate::binance::now_ms();
        let body = self.clearinghouse_state()?;
        parse_clearinghouse(&body, read_at)
    }

    /// Open positions.
    ///
    /// # Errors
    /// Whatever the transport reports.
    pub fn positions(&self) -> Result<Vec<crate::binance::PositionSnapshot>, VenueError> {
        let body = self.clearinghouse_state()?;
        Ok(parse_asset_positions(&body))
    }

    /// The account's state, named by address.
    ///
    /// The configured address is the master or subaccount. Deriving it
    /// from an API wallet's signing key would query an empty account.
    ///
    /// # Errors
    /// Whatever the transport reports.
    pub fn clearinghouse_state(&self) -> Result<String, VenueError> {
        self.info(&format!(
            r#"{{"type":"clearinghouseState","user":"{}"}}"#,
            self.address()
        ))
    }
}

impl crate::exec::Execution for Hyperliquid {
    fn place(&self, order: &crate::exec::NewOrder, instrument: &Instrument) -> Placed {
        let Some(asset) = self.asset_of(&order.symbol) else {
            return Placed::Rejected(Reject {
                code: None,
                message: format!(
                    "{} is not in this venue's universe; load it before trading, \
                     because an order references an asset by index and a missing \
                     one would otherwise be sent as index zero",
                    order.symbol
                ),
            });
        };
        if !crate::broker::IdRules::HYPERLIQUID.accepts(&order.client_id) {
            return Placed::Rejected(Reject {
                code: None,
                message: format!(
                    "client id {:?} is not usable here: a cloid is 0x and 32 hex digits",
                    order.client_id
                ),
            });
        }
        let Some(limit) = order.limit_price else {
            return Placed::Rejected(Reject {
                code: None,
                message: "this venue has no market order; send an IOC limit at a \
                          slippage-adjusted price, which needs a quote this adapter \
                          does not carry"
                    .to_string(),
            });
        };
        let price = canonical_decimal(limit.0, instrument.price_scale);
        let sz_decimals = self
            .asset_meta(&order.symbol)
            .map_or(instrument.qty_scale, |asset| asset.sz_decimals);
        if !valid_perp_price(&price, sz_decimals) {
            return Placed::Rejected(Reject {
                code: None,
                message: format!(
                    "price {price} exceeds this asset's five-significant-figure or \
                     decimal-place limit (szDecimals={sz_decimals})"
                ),
            });
        }
        let size = canonical_decimal(order.qty.0, instrument.qty_scale);
        // There is no fill-or-kill here; refused rather than sent as
        // something weaker.
        let tif = match order.tif {
            oq_types::TimeInForce::GoodTilCancel => "Gtc",
            oq_types::TimeInForce::ImmediateOrCancel => "Ioc",
            oq_types::TimeInForce::FillOrKill => {
                return Placed::Rejected(Reject {
                    code: None,
                    message: "this venue has no fill-or-kill order".to_string(),
                });
            }
        };
        let mut wire = match order_wire(
            asset,
            matches!(order.side, oq_types::Side::Buy),
            &price,
            &size,
            order.reduce_only,
            tif,
        ) {
            Value::Map(pairs) => pairs,
            other => return unreadable(order, other),
        };
        wire.push(("c".to_string(), Value::str(&order.client_id)));
        let action = order_action(vec![Value::Map(wire)]);
        let nonce = u64::try_from(crate::binance::now_ms()).unwrap_or_default();
        match self.post_action(&action, nonce) {
            Ok(text) => classify(200, &text, &order.client_id),
            Err(VenueError::Venue { status, body }) => classify(status, &body, &order.client_id),
            Err(e) => Placed::Unknown(Unresolved {
                client_id: order.client_id.clone(),
                reason: e.to_string(),
            }),
        }
    }

    fn cancel(&self, symbol: &str, client_id: &str) -> Placed {
        let Some(asset) = self.asset_of(symbol) else {
            return Placed::Rejected(Reject {
                code: None,
                message: format!("{symbol} is not in this venue's universe"),
            });
        };
        let action = cancel_by_cloid_action(asset, client_id);
        let nonce = u64::try_from(crate::binance::now_ms()).unwrap_or_default();
        match self.post_action(&action, nonce) {
            Ok(text) => classify_cancel(200, &text, client_id),
            Err(VenueError::Venue { status, body }) => classify_cancel(status, &body, client_id),
            Err(e) => Placed::Unknown(Unresolved {
                client_id: client_id.to_string(),
                reason: e.to_string(),
            }),
        }
    }

    fn order_status(
        &self,
        _symbol: &str,
        client_id: &str,
    ) -> Result<Option<crate::exec::OrderAck>, VenueError> {
        // The query names the account, because the venue indexes orders
        // by address and an agent wallet's own address holds nothing —
        // the trap NautilusTrader reports as "everything connects and
        // nothing matches".
        let body = self.info(&format!(
            r#"{{"type":"orderStatus","user":"{}","oid":"{client_id}"}}"#,
            self.address()
        ))?;
        // Only `unknownOid` is "no such order", the answer that licenses a
        // resend after an unknown placement. Anything else this cannot
        // read is the venue failing to answer, and is an error: "no such
        // order" and "could not tell" lead to opposite actions.
        match crate::json::field_str(&body, "status").as_deref() {
            Some("unknownOid") => Ok(None),
            Some("order") => order_from_query(&body, client_id)
                .map(Some)
                .ok_or_else(|| crate::json::malformed("order status", &body)),
            _ => Err(crate::json::malformed("order status", &body)),
        }
    }
}

/// What an `orderStatus` answer says about one order; `None` when the
/// venue does not have it, or when the answer does not read — which
/// [`Execution::order_status`](crate::exec::Execution::order_status)
/// tells apart before trusting a `None`.
///
/// The answer nests twice, and both mistakes this used to make came
/// from reading it flat. Its first `status` is the envelope's — `order`
/// for a known order, `unknownOid` for none — and the order's own state
/// is the `status` beside the inner `order` object, so reading the first
/// one reported every order as being in the state `order`. And `sz` is
/// what is still open, not what has filled: a filled order reads
/// `"sz":"0.0"` beside `"origSz":"0.0076"`, so taking `sz` as the
/// executed quantity reported a full fill as nothing filled.
#[must_use]
pub fn order_from_query(body: &str, client_id: &str) -> Option<crate::exec::OrderAck> {
    if crate::json::field_str(body, "status").as_deref() != Some("order") {
        return None;
    }
    let wrapper = crate::json::object_field(body, "order")?;
    let order = crate::json::object_field(&wrapper, "order")?;
    // The wrapper's own fields, with the order taken out so none of its
    // keys can answer for the wrapper's.
    let own = wrapper.replacen(&order, "{}", 1);
    let status = crate::json::field_str(&own, "status").unwrap_or_default();
    let executed_qty = match (
        crate::json::field_str(&order, "origSz"),
        crate::json::field_str(&order, "sz"),
    ) {
        (Some(original), Some(open)) => filled_size(&original, &open)?,
        _ => return None,
    };
    Some(crate::exec::OrderAck {
        venue_id: crate::json::raw_field(&order, "oid").unwrap_or_default(),
        client_id: client_id.to_string(),
        status,
        executed_qty,
    })
}

/// `original - open`, exactly, at the finer of the two precisions.
fn filled_size(original: &str, open: &str) -> Option<String> {
    let places = |t: &str| t.split_once('.').map_or(0, |(_, f)| f.len());
    let scale = u8::try_from(places(original).max(places(open))).ok()?;
    let filled =
        crate::klines::scaled(original, scale)?.checked_sub(crate::klines::scaled(open, scale)?)?;
    (filled >= 0).then(|| canonical_decimal(filled, scale))
}

fn unreadable(order: &crate::exec::NewOrder, _value: Value) -> Placed {
    Placed::Rejected(Reject {
        code: None,
        message: format!("could not build a wire for {}", order.client_id),
    })
}

#[cfg(test)]
mod account_reads {
    use super::*;

    /// The venue's own documented response, numbers included.
    const STATE: &str = r#"{"assetPositions":[{"position":{"coin":"ETH",
        "entryPx":"2986.3","leverage":{"rawUsd":"-95.059824","type":"isolated","value":20},
        "liquidationPx":"2866.26936529","marginUsed":"4.967826","maxLeverage":50,
        "positionValue":"100.02765","returnOnEquity":"-0.0026789","szi":"0.0335",
        "unrealizedPnl":"-0.0134"},"type":"oneWay"}],
        "crossMaintenanceMarginUsed":"0.0",
        "crossMarginSummary":{"accountValue":"13104.514502","totalMarginUsed":"0.0",
        "totalNtlPos":"0.0","totalRawUsd":"13104.514502"},
        "marginSummary":{"accountValue":"13109.482328","totalMarginUsed":"4.967826",
        "totalNtlPos":"100.02765","totalRawUsd":"13009.454678"},
        "time":1708622398623,"withdrawable":"13104.514502"}"#;

    #[test]
    fn the_margin_summary_satisfies_the_identity_this_build_reads_it_by() {
        // 13009.454678 + 100.02765 = 13109.482328, in the venue's own
        // published example. That is what confirms the fields are what
        // they are taken to be.
        let snap = parse_clearinghouse(STATE, 9).expect("a readable state");
        assert!((snap.margin_balance - 13_109.482_328).abs() < 1e-6);
        // Summed from the legs, because the summary carries no total.
        assert!((snap.unrealized - -0.0134).abs() < 1e-9);
        // Defined, not measured: wallet plus unrealized is the margin
        // balance, by what `AccountSnapshot` means.
        assert!((snap.wallet_balance - (13_109.482_328 + 0.0134)).abs() < 1e-6);
        assert_eq!(snap.read_at_ms, 9);
    }

    #[test]
    fn a_summary_that_fails_the_identity_is_refused() {
        // If `accountValue` is not `totalRawUsd` plus `totalNtlPos`,
        // these are not the fields this build thinks they are, and
        // three numbers derived from that reading would all be wrong
        // together.
        let wrong = STATE.replace(
            r#""accountValue":"13109.482328","totalMarginUsed":"4.967826""#,
            r#""accountValue":"99999.0","totalMarginUsed":"4.967826""#,
        );
        let e = parse_clearinghouse(&wrong, 1).expect_err("the identity must hold");
        assert!(
            format!("{e}").contains("does not mean what this build reads it to mean"),
            "{e}"
        );
    }

    #[test]
    fn a_position_takes_its_sign_from_the_number() {
        // `type` is `oneWay` and there is no leg name, as on Backpack.
        let legs = parse_asset_positions(STATE);
        assert_eq!(legs.len(), 1);
        assert_eq!(legs[0].symbol, "ETH");
        assert_eq!(legs[0].amount_text, "0.0335");
        assert_eq!(legs[0].position_side, "BOTH");
        assert!((legs[0].entry_price - 2986.3).abs() < 1e-9);
        assert!((legs[0].unrealized - -0.0134).abs() < 1e-9);
    }

    #[test]
    fn an_account_holding_nothing_reads_as_flat() {
        let body = r#"{"assetPositions":[],"marginSummary":{"accountValue":"100.0",
            "totalMarginUsed":"0.0","totalNtlPos":"0.0","totalRawUsd":"100.0"},
            "withdrawable":"100.0","time":1}"#;
        assert!(parse_asset_positions(body).is_empty());
        let snap = parse_clearinghouse(body, 1).expect("a flat account is readable");
        assert!((snap.unrealized).abs() < 1e-12);
        assert!((snap.wallet_balance - 100.0).abs() < 1e-9);
    }
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
    fn an_asset_is_a_position_in_the_universe() {
        // Orders reference assets by number. The order of this list is
        // load-bearing: a venue that reordered it would silently
        // repoint every symbol, which is why it is read at startup
        // rather than cached.
        let body = r#"{"universe":[{"name":"BTC","szDecimals":5,"maxLeverage":50},
            {"name":"ETH","szDecimals":4,"maxLeverage":50},
            {"name":"SOL","szDecimals":2,"maxLeverage":20}]}"#;
        let names = parse_universe(body);
        assert_eq!(names, vec!["BTC", "ETH", "SOL"]);
        assert_eq!(names.iter().position(|n| n == "ETH"), Some(1));
        let assets = parse_universe_meta(body);
        assert_eq!(assets[1].sz_decimals, 4);
    }

    #[test]
    fn a_cloid_is_the_fourth_shape_a_client_id_takes() {
        // 36 characters with punctuation, 100 characters, a uint32 —
        // and now 0x and exactly 32 hex digits.
        assert!(crate::broker::IdRules::HYPERLIQUID.accepts("0x1234567890abcdef1234567890abcdef"));
        assert!(
            !crate::broker::IdRules::HYPERLIQUID.accepts("0x1234"),
            "too short"
        );
        assert!(
            !crate::broker::IdRules::HYPERLIQUID.accepts("1234567890abcdef1234567890abcdef"),
            "no prefix"
        );
        assert!(
            !crate::broker::IdRules::HYPERLIQUID.accepts("0xghijklmnopqrstuvwxyz1234567890ab"),
            "not hex"
        );
        assert!(
            !crate::broker::IdRules::HYPERLIQUID.accepts("oq1"),
            "what every other venue would take"
        );
    }

    #[test]
    fn an_address_is_derived_the_way_the_venue_recovers_it() {
        // The venue recovers this from the signature. A client that
        // computed it differently would query one account and trade
        // another, which is the agent-wallet trap in a different form.
        let address = address_hex(&address_of(&key()));
        assert!(address.starts_with("0x"), "{address}");
        assert_eq!(address.len(), 42, "twenty bytes as hex");
        // Deterministic from the key, so the same key is the same
        // account every run.
        assert_eq!(address, address_hex(&address_of(&key())));
    }

    #[test]
    fn account_queries_use_the_configured_master_not_the_signer() {
        let master = "0xABCDEFabcdefABCDEFabcdefABCDEFabcdefABCD";
        let creds = Credentials::new(master, TEST_KEY);
        let client = Hyperliquid::at(Endpoint::Testnet, &creds).expect("valid client");
        assert_eq!(
            client.address(),
            "0xabcdefabcdefabcdefabcdefabcdefabcdefabcd"
        );
        assert_ne!(client.address(), address_hex(&address_of(&client.key)));
    }

    #[test]
    fn an_account_query_address_must_be_an_ethereum_address() {
        let creds = Credentials::new("agent-name", TEST_KEY);
        assert!(Hyperliquid::at(Endpoint::Testnet, &creds).is_err());
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

    /// hyperliquid-python-sdk, tests/signing_test.py,
    /// `test_l1_action_signing_matches_with_vault`: the vault address is
    /// part of what is signed, so an action for a subaccount signed
    /// without it is an action for another account.
    #[test]
    fn the_published_vault_signing_vector() {
        let action = Value::map(vec![
            ("type", Value::str("dummy")),
            ("num", Value::Uint(100_000_000_000)),
        ]);
        let vault = address_bytes("0x1719884eb866cb12b2287399b15f7db5e7d775ea").expect("address");
        let mainnet = sign_l1_action(&key(), &action, 0, Some(vault), true).expect("signed");
        assert_eq!(
            mainnet.r_hex(),
            "0x3c548db75e479f8012acf3000ca3a6b05606bc2ec0c29c50c515066a326239"
        );
        assert_eq!(
            mainnet.s_hex(),
            "0x4d402be7396ce74fbba3795769cda45aec00dc3125a984f2a9f23177b190da2c"
        );
        assert_eq!(mainnet.v, 28);
        let testnet = sign_l1_action(&key(), &action, 0, Some(vault), false).expect("signed");
        assert_eq!(
            testnet.r_hex(),
            "0xe281d2fb5c6e25ca01601f878e4d69c965bb598b88fac58e475dd1f5e56c362b"
        );
        assert_eq!(
            testnet.s_hex(),
            "0x7ddad27e9a238d045c035bc606349d075d5c5cd00a6cd1da23ab5c39d4ef0f60"
        );
        assert_eq!(testnet.v, 27);
    }

    /// hyperliquid-docs, exchange endpoint, "Cancel order(s)": the two
    /// documented answers. The placement reader knew neither, so every
    /// successful cancel came back unknown.
    #[test]
    fn a_cancel_is_read_as_a_cancel() {
        let ok = r#"{"status":"ok","response":{"type":"cancel","data":{"statuses":["success"]}}}"#;
        let refused = r#"{"status":"ok","response":{"type":"cancel","data":{"statuses":[{"error":"Order was never placed, already canceled, or filled."}]}}}"#;
        assert!(
            matches!(classify(200, ok, "c"), Placed::Unknown(_)),
            "the old reading"
        );
        assert!(matches!(classify_cancel(200, ok, "c"), Placed::Accepted(_)));
        match classify_cancel(200, refused, "c") {
            Placed::Rejected(r) => assert!(r.message.contains("never placed"), "{r:?}"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(classify_cancel(500, ok, "c"), Placed::Unknown(_)));
    }

    /// hyperliquid-docs, info endpoint, "Query order status by oid or
    /// cloid", with the documented `<status>` placeholder set to one of
    /// its listed values. Both fields this used to misread are here: the
    /// envelope's `status` comes first, and `sz` is what is still open.
    const ORDER_STATUS: &str = r#"{"status":"order","order":{"order":{"coin":"ETH","side":"A","limitPx":"2412.7","sz":"0.0","oid":1,"timestamp":1724361546645,"triggerCondition":"N/A","isTrigger":false,"triggerPx":"0.0","children":[],"isPositionTpsl":false,"reduceOnly":true,"orderType":"Market","origSz":"0.0076","tif":"FrontendMarket","cloid":null},"status":"filled","statusTimestamp":1724361546645}}"#;

    #[test]
    fn an_order_status_is_the_orders_and_its_fill_is_what_left_the_book() {
        let a = order_from_query(ORDER_STATUS, "c").expect("a known order");
        assert_eq!(a.status, "filled", "not the envelope's `order`");
        assert_eq!(
            a.executed_qty, "0.0076",
            "origSz less the open sz, not the open sz"
        );
        assert_eq!(a.venue_id, "1");
        // The old reading, for the record of what it said.
        assert_eq!(
            crate::json::field_str(ORDER_STATUS, "status").as_deref(),
            Some("order")
        );
        assert_eq!(
            crate::json::field_str(ORDER_STATUS, "sz").as_deref(),
            Some("0.0")
        );

        let partly = ORDER_STATUS
            .replace(r#""sz":"0.0""#, r#""sz":"0.0026""#)
            .replace(r#""status":"filled""#, r#""status":"open""#);
        let a = order_from_query(&partly, "c").expect("a known order");
        assert_eq!(
            (a.status.as_str(), a.executed_qty.as_str()),
            ("open", "0.005")
        );

        assert_eq!(order_from_query(r#"{"status":"unknownOid"}"#, "c"), None);
        // More open than was ever ordered is not an answer to trust.
        let impossible = ORDER_STATUS.replace(r#""sz":"0.0""#, r#""sz":"1""#);
        assert_eq!(order_from_query(&impossible, "c"), None);
    }

    /// An action refused as a whole is `"status":"err"` with the reason
    /// as `response`. The placement reader looked for a key named `err`.
    #[test]
    fn an_action_refused_as_a_whole_is_a_rejection() {
        let body = r#"{"status":"err","response":"User or API Wallet 0x0 does not exist."}"#;
        for placed in [classify(200, body, "c"), classify_cancel(200, body, "c")] {
            match placed {
                Placed::Rejected(r) => assert!(r.message.contains("does not exist"), "{r:?}"),
                other => panic!("{other:?}"),
            }
        }
    }

    /// There is no fill-or-kill on this venue, and one is refused before
    /// anything is signed rather than sent as a resting `Gtc`.
    #[test]
    fn a_fill_or_kill_order_is_refused() {
        let creds = Credentials::new("0xabcdefabcdefabcdefabcdefabcdefabcdefabcd", TEST_KEY);
        let mut client = Hyperliquid::at(Endpoint::Testnet, &creds).expect("valid client");
        client.universe = vec![AssetMeta {
            name: "BTC".to_string(),
            sz_decimals: 5,
        }];
        let order = crate::exec::NewOrder {
            symbol: "BTC".to_string(),
            side: oq_types::Side::Buy,
            limit_price: Some(oq_types::PriceTicks(6_000_000)),
            qty: oq_types::QtyLots(100),
            tif: oq_types::TimeInForce::FillOrKill,
            client_id: "0x0123456789abcdef0123456789abcdef".to_string(),
            reduce_only: false,
            position_side: crate::exec::PositionSide::OneWay,
        };
        match crate::exec::Execution::place(&client, &order, &Instrument::linear(1, 5)) {
            Placed::Rejected(r) => assert!(r.message.contains("fill-or-kill"), "{r:?}"),
            other => panic!("{other:?}"),
        }
    }

    /// The five answers `userRole` documents, and what each means for
    /// signing.
    #[test]
    fn the_account_role_decides_whose_account_is_traded() {
        let account = "0x1719884eb866cb12b2287399b15f7db5e7d775ea";
        let vault = address_bytes(account).expect("address");
        assert_eq!(
            acting_for(r#"{"role":"user"}"#, account),
            Ok(ActingFor::Master)
        );
        assert_eq!(
            acting_for(
                r#"{"role":"subAccount","data":{"master":"0xabc"}}"#,
                account
            ),
            Ok(ActingFor::Vault(vault))
        );
        assert_eq!(
            acting_for(r#"{"role":"vault"}"#, account),
            Ok(ActingFor::Vault(vault))
        );
        assert!(acting_for(r#"{"role":"agent","data":{"user":"0xabc"}}"#, account).is_err());
        assert!(acting_for(r#"{"role":"missing"}"#, account).is_err());
        assert!(acting_for("<html>", account).is_err());
    }

    /// The vault goes into the body as well as the signature, as the
    /// venue's SDK sends it.
    #[test]
    fn a_vault_action_names_the_vault_in_its_body() {
        let action = Value::map(vec![("type", Value::str("dummy"))]);
        let vault = address_bytes("0x1719884eb866cb12b2287399b15f7db5e7d775ea").expect("address");
        let with = action_body(&action, 1, Some(vault), &key(), true).expect("body");
        assert!(
            with.ends_with(r#","vaultAddress":"0x1719884eb866cb12b2287399b15f7db5e7d775ea"}"#),
            "{with}"
        );
        let without = action_body(&action, 1, None, &key(), true).expect("body");
        assert!(!without.contains("vaultAddress"), "{without}");
        assert!(without.ends_with("}}"), "{without}");
    }

    /// Nothing is signed before the role is known: signing without a
    /// vault would place a subaccount's order in its master.
    #[test]
    fn nothing_is_sent_before_the_account_role_is_known() {
        let creds = Credentials::new("0xabcdefabcdefabcdefabcdefabcdefabcdefabcd", TEST_KEY);
        let client = Hyperliquid::at(Endpoint::Testnet, &creds).expect("valid client");
        let action = Value::map(vec![("type", Value::str("dummy"))]);
        let err = client
            .post_action(&action, 1)
            .expect_err("refused before the network");
        assert!(err.to_string().contains("not resolved"), "{err}");
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
        assert!(within_five_significant_figures("1234567"));
        assert!(valid_perp_price("1234567", 5));
        assert!(valid_perp_price("0.1", 5));
        assert!(!valid_perp_price("0.01", 5));
        assert!(!valid_perp_price("12345.6", 0));
    }

    #[test]
    fn hyperliquid_numbers_drop_trailing_zeroes_before_signing() {
        assert_eq!(canonical_decimal(1_200_000, 1), "120000");
        assert_eq!(canonical_decimal(1_470, 5), "0.0147");
        assert_eq!(canonical_decimal(0, 4), "0");
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
