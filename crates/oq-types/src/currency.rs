//! What a number of [`Cash`](crate::Cash) units is denominated in.
//!
//! # Why this is not a field of `Cash`
//!
//! The obvious move is to put a currency inside every cash value and
//! check it on every addition. That is the wrong shape for the same
//! reason [`PriceTicks`](crate::PriceTicks) does not carry an
//! instrument: the unit belongs to the **slot**, not to each number that
//! passes through it. An account's USD balance is USD because of where
//! it is held, and a price is in an instrument's ticks because of which
//! book it came from — a per-value tag would be paid for on every
//! arithmetic operation in the engine to restate what the surrounding
//! type already knows.
//!
//! So a currency appears where a slot can hold more than one:
//! [`Balances`], where an account settling in several currencies keeps
//! one amount per currency and no addition crosses between them.
//!
//! # Why not an enum
//!
//! A closed set would have to name every currency a user might trade,
//! and be edited to add one. Venues list new settlement assets without
//! asking. The code is stored as it is written — up to eight ASCII
//! characters — so `Currency` is comparable, copyable, orderable and
//! printable without a registry to look anything up in, and a currency
//! nobody anticipated is representable the moment it is quoted.
//!
//! # What this deliberately does not do
//!
//! **It does not convert.** Adding USD to EUR is refused rather than
//! rated, because a rate is a time-varying external input and a total
//! computed with a stale one is wrong in a way that looks like a
//! number. Equity across currencies needs a rate source and a point in
//! time; until this project has one, an account can *hold* several
//! currencies and cannot *total* them, which is the honest half.

use core::fmt;

use crate::Cash;

/// A settlement currency, as its ticker.
///
/// Eight bytes because that covers every settlement asset in use and
/// keeps the type `Copy` and comparable without allocating. Longer
/// codes are refused rather than truncated: a truncated ticker collides
/// with a different currency and produces a balance filed under the
/// wrong one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Currency([u8; 8]);

impl Currency {
    /// A currency from its ticker.
    ///
    /// `None` for an empty code, one past eight bytes, or one holding
    /// anything but ASCII letters and digits. Venue tickers are
    /// upper-case alphanumerics; anything else here is a parsing
    /// mistake upstream, and accepting it would file a balance under a
    /// key nothing else can produce.
    #[must_use]
    pub const fn new(code: &str) -> Option<Self> {
        let bytes = code.as_bytes();
        if bytes.is_empty() || bytes.len() > 8 {
            return None;
        }
        let mut out = [0u8; 8];
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            let ok = b.is_ascii_uppercase() || b.is_ascii_digit();
            if !ok {
                return None;
            }
            out[i] = b;
            i += 1;
        }
        Some(Self(out))
    }

    /// The ticker this was built from.
    #[must_use]
    pub fn code(&self) -> &str {
        let end = self.0.iter().position(|b| *b == 0).unwrap_or(self.0.len());
        // Every byte was checked as ASCII on the way in.
        core::str::from_utf8(&self.0[..end]).unwrap_or("")
    }

    /// Tether, the settlement asset of the venues this project reads.
    ///
    /// A convenience for the common case and not a default: a run that
    /// never states its currency should be one that never needed to,
    /// not one that quietly assumed this.
    #[must_use]
    pub const fn usdt() -> Self {
        Self([b'U', b'S', b'D', b'T', 0, 0, 0, 0])
    }
}

impl fmt::Display for Currency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

/// What an account holds, by currency.
///
/// The thing `Cash` alone cannot express: a book settling in more than
/// one currency. Equities and FX require it; the perpetuals this project
/// reads today do not, which is why one currency has to stay as cheap as
/// it was.
///
/// # No total
///
/// There is deliberately no `equity()` here. Summing across currencies
/// needs a rate, a rate is a time-varying external input, and a total
/// computed from a stale one is wrong while looking like a number. An
/// account can hold several currencies and cannot total them until
/// something supplies rates and the instant they were true at.
///
/// One currency is the case that must not get slower for the sake of the
/// case that does not exist yet: a `Balances` holding a single currency
/// does no allocation and answers in a comparison.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Balances {
    /// Sorted by currency, so equality and iteration do not depend on
    /// the order amounts happened to arrive in — which would otherwise
    /// make two identical accounts compare unequal, and a fingerprint
    /// depend on history rather than state.
    held: Vec<(Currency, Cash)>,
}

impl Balances {
    /// An account holding nothing.
    #[must_use]
    pub const fn new() -> Self {
        Self { held: Vec::new() }
    }

    /// An account holding one amount in one currency.
    #[must_use]
    pub fn of(currency: Currency, amount: Cash) -> Self {
        Self {
            held: vec![(currency, amount)],
        }
    }

    /// The amount held in `currency`, zero when none is.
    ///
    /// Zero rather than `None`: an account that has never touched a
    /// currency holds none of it, which is a quantity and not an
    /// absence. A caller that needs to tell "never held" from "held
    /// nothing" is asking about history, which is the journal's
    /// question.
    #[must_use]
    pub fn get(&self, currency: Currency) -> Cash {
        self.held
            .binary_search_by_key(&currency, |(c, _)| *c)
            .map_or(Cash::ZERO, |i| self.held[i].1)
    }

    /// Add to one currency's balance, leaving the others alone.
    pub fn add(&mut self, currency: Currency, amount: Cash) {
        match self.held.binary_search_by_key(&currency, |(c, _)| *c) {
            Ok(i) => self.held[i].1 = self.held[i].1.add(amount),
            Err(i) => self.held.insert(i, (currency, amount)),
        }
    }

    /// Every currency held, in a stable order.
    ///
    /// Includes currencies whose balance has fallen to zero. They are
    /// part of the account's shape — a currency traded and closed out is
    /// not the same as one never touched, and dropping it here would
    /// make a fingerprint depend on whether a position happened to end
    /// flat.
    pub fn iter(&self) -> impl Iterator<Item = (Currency, Cash)> + '_ {
        self.held.iter().copied()
    }

    /// How many currencies this account has touched.
    #[must_use]
    pub fn len(&self) -> usize {
        self.held.len()
    }

    /// Whether the account has touched no currency at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.held.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ticker_survives_the_round_trip() {
        for code in ["USDT", "USD", "BTC", "EUR", "USDC", "BUSD", "1INCH"] {
            let c = Currency::new(code).expect("valid");
            assert_eq!(c.code(), code);
            assert_eq!(c.to_string(), code);
        }
    }

    /// A long code is refused, never truncated.
    ///
    /// Truncation is the failure that matters: `SUPERLONG1` and
    /// `SUPERLONG2` would become the same key, and a balance would be
    /// filed under a currency that is not the one it is in.
    #[test]
    fn a_code_past_eight_bytes_is_refused() {
        assert!(Currency::new("ABCDEFGH").is_some(), "eight fits");
        assert!(Currency::new("ABCDEFGHI").is_none(), "nine does not");
    }

    #[test]
    fn only_upper_case_alphanumerics_are_accepted() {
        for bad in ["", "usdt", "US-D", "US D", "US.D", "US\u{00e9}"] {
            assert!(Currency::new(bad).is_none(), "{bad:?} must be refused");
        }
    }

    /// Distinct tickers are distinct keys, including where one is a
    /// prefix of another.
    #[test]
    fn a_prefix_is_not_the_same_currency() {
        let usd = Currency::new("USD").expect("valid");
        let usdt = Currency::new("USDT").expect("valid");
        assert_ne!(usd, usdt);
        assert_eq!(usdt, Currency::usdt());
    }

    // ---- Balances ----

    fn c(code: &str) -> Currency {
        Currency::new(code).expect("valid")
    }

    /// A currency never touched holds nothing, and that is a quantity.
    #[test]
    fn an_untouched_currency_holds_zero() {
        let b = Balances::new();
        assert_eq!(b.get(c("USD")), Cash::ZERO);
        assert!(b.is_empty());
        assert_eq!(b.len(), 0);
    }

    /// The thing `Cash` alone could not express.
    #[test]
    fn an_account_holds_several_currencies_at_once() {
        let mut b = Balances::new();
        b.add(c("USD"), Cash(100));
        b.add(c("EUR"), Cash(250));
        b.add(c("USD"), Cash(5));

        assert_eq!(b.get(c("USD")), Cash(105));
        assert_eq!(b.get(c("EUR")), Cash(250));
        assert_eq!(b.len(), 2);
    }

    /// Adding to one currency does not reach another.
    ///
    /// The failure this type exists to prevent: a EUR credit landing in
    /// the USD balance is not a rounding difference, it is an account
    /// that reports money it does not have in a currency it cannot
    /// spend.
    #[test]
    fn currencies_do_not_leak_into_each_other() {
        let mut b = Balances::of(c("USD"), Cash(1_000));
        b.add(c("JPY"), Cash(999));
        assert_eq!(b.get(c("USD")), Cash(1_000), "USD is untouched");
        assert_eq!(b.get(c("JPY")), Cash(999));
    }

    /// Two accounts holding the same amounts are equal however the
    /// amounts arrived.
    ///
    /// Insertion order is history, not state. A fingerprint that
    /// depended on it would make two identical accounts diverge on
    /// replay, which is the one thing a fingerprint exists to catch.
    #[test]
    fn equality_does_not_depend_on_the_order_amounts_arrived_in() {
        let mut a = Balances::new();
        a.add(c("USD"), Cash(1));
        a.add(c("BTC"), Cash(2));
        a.add(c("EUR"), Cash(3));

        let mut b = Balances::new();
        b.add(c("EUR"), Cash(3));
        b.add(c("USD"), Cash(1));
        b.add(c("BTC"), Cash(2));

        assert_eq!(a, b);
        assert_eq!(
            a.iter().collect::<Vec<_>>(),
            b.iter().collect::<Vec<_>>(),
            "and iterate identically"
        );
    }

    /// A currency traded and closed out is not one never touched.
    #[test]
    fn a_currency_that_fell_to_zero_is_still_held() {
        let mut b = Balances::of(c("USD"), Cash(10));
        b.add(c("USD"), Cash(-10));
        assert_eq!(b.get(c("USD")), Cash::ZERO);
        assert_eq!(b.len(), 1, "the account has touched USD");
        assert!(!b.is_empty());
    }
}
