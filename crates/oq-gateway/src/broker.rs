//! Broker and referral prefixes on client order ids.
//!
//! `FR-VENUE-4` asks that order identifiers support a broker/referral
//! prefix scheme **from day one, so integrators can attribute flow
//! without patching the adapter**. The "without patching" is the whole
//! requirement: a venue's broker programme pays on ids carrying a code
//! it issued, and an integrator who has to fork the adapter to add one
//! is an integrator who maintains a fork forever.
//!
//! # This is not the ownership prefix, and conflating them is the bug
//!
//! `oq-live` already has `--id-prefix`, and it answers a different
//! question: *is this order mine?* It exists because a shared account
//! delivers another system's orders on the same stream, and counting
//! them consumed this process's risk-gate limit.
//!
//! A broker code answers *who gets paid for this flow?* The two can
//! differ — one venue account can run several strategies under one
//! broker code, and one operator can run one strategy under different
//! codes per client. An adapter that used the ownership prefix as the
//! broker code would tie the two together for no reason either of them
//! implies, and the first time somebody needed them apart the fix would
//! be a fork.
//!
//! # Venues disagree about the shape, so the shape is checked per venue
//!
//! Binance's futures broker ids are `x-` followed by an alphanumeric
//! code and accept `[A-Za-z0-9._:/-]` after it, to 36 characters. OKX's
//! `clOrdId` is alphanumeric only, to 32. A composed id that is legal on
//! one venue and not the other fails at the venue with a message about
//! the id, which is a long way from the code that composed it — so it is
//! refused here instead.

use core::fmt;

/// A venue's rules for what a client order id may contain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdRules {
    /// Longest id the venue accepts.
    pub max_len: usize,
    /// Whether anything beyond letters and digits is allowed.
    ///
    /// Named as a flag rather than a character set because that is the
    /// distinction the two shipped venues actually differ on, and a
    /// character set nobody varies is a knob that only ever gets set
    /// wrong.
    pub punctuation_allowed: bool,
}

impl IdRules {
    /// Binance USDT-M futures: 36 characters of `[A-Za-z0-9._:/-]`.
    pub const BINANCE: Self = Self {
        max_len: 36,
        punctuation_allowed: true,
    };
    /// OKX: 32 alphanumeric characters.
    pub const OKX: Self = Self {
        max_len: 32,
        punctuation_allowed: false,
    };

    /// Whether `id` is usable as it stands.
    #[must_use]
    pub fn accepts(&self, id: &str) -> bool {
        !id.is_empty()
            && id.len() <= self.max_len
            && id.chars().all(|c| {
                c.is_ascii_alphanumeric()
                    || (self.punctuation_allowed && matches!(c, '.' | '_' | ':' | '/' | '-'))
            })
    }
}

/// Why an id could not be composed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdError {
    /// The finished id is longer than the venue accepts.
    ///
    /// Reported with both lengths, because the fix is to shorten
    /// whichever part the integrator controls and they need to know how
    /// much.
    TooLong {
        /// What was composed.
        len: usize,
        /// What the venue allows.
        max: usize,
    },
    /// A character the venue will not take.
    Character {
        /// The offending character.
        found: char,
        /// Where it came from.
        part: &'static str,
    },
    /// A part that had to be present was empty.
    Empty {
        /// Which one.
        part: &'static str,
    },
}

impl fmt::Display for IdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLong { len, max } => write!(
                f,
                "the composed client id is {len} characters and this venue accepts {max}; \
                 shorten the broker code or the sequence, not the venue"
            ),
            Self::Character { found, part } => write!(
                f,
                "the {part} contains {found:?}, which this venue does not accept in a \
                 client order id"
            ),
            Self::Empty { part } => write!(f, "the {part} is empty"),
        }
    }
}

impl core::error::Error for IdError {}

/// A broker or referral code, issued by the venue.
///
/// Held as a type rather than a string so it cannot be swapped with the
/// ownership prefix by an assignment that type-checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrokerCode(String);

impl BrokerCode {
    /// Take a code the venue issued.
    ///
    /// # Errors
    /// When it is empty or carries a character no venue accepts.
    pub fn new(code: impl Into<String>) -> Result<Self, IdError> {
        let code = code.into();
        if code.is_empty() {
            return Err(IdError::Empty {
                part: "broker code",
            });
        }
        if let Some(c) = code.chars().find(|c| !c.is_ascii_alphanumeric()) {
            // Every venue's id alphabet contains letters and digits and
            // they disagree past that, so a code with punctuation in it
            // is one that will fail on some venue later. Refused here,
            // where the message can say so.
            return Err(IdError::Character {
                found: c,
                part: "broker code",
            });
        }
        Ok(Self(code))
    }

    /// The code, as the venue issued it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Composes client order ids carrying a broker code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdScheme {
    broker: Option<BrokerCode>,
    /// The ownership prefix — *is this order mine* — kept separate from
    /// the broker code on purpose.
    owner: String,
    rules: IdRules,
}

impl IdScheme {
    /// A scheme with no broker code: just ownership.
    ///
    /// # Errors
    /// When the owner prefix is empty or the venue will not take it.
    pub fn new(owner: impl Into<String>, rules: IdRules) -> Result<Self, IdError> {
        let owner = owner.into();
        if owner.is_empty() {
            return Err(IdError::Empty {
                part: "ownership prefix",
            });
        }
        if let Some(c) = owner.chars().find(|c| !rules.accepts(&c.to_string())) {
            return Err(IdError::Character {
                found: c,
                part: "ownership prefix",
            });
        }
        Ok(Self {
            broker: None,
            owner,
            rules,
        })
    }

    /// The same scheme, attributing flow to a broker code.
    #[must_use]
    pub fn with_broker(mut self, code: BrokerCode) -> Self {
        self.broker = Some(code);
        self
    }

    /// The prefix an ownership check matches on.
    ///
    /// Includes the broker code when there is one, because the venue
    /// echoes the whole id back and a `Book` matching only the owner
    /// segment would not recognise this process's own orders.
    #[must_use]
    pub fn owned_prefix(&self) -> String {
        match &self.broker {
            Some(b) => format!("{}{}", b.as_str(), self.owner),
            None => self.owner.clone(),
        }
    }

    /// Compose the id for one order.
    ///
    /// # Errors
    /// When the finished id does not fit the venue's rules.
    pub fn compose(&self, sequence: u64) -> Result<String, IdError> {
        let id = format!("{}{sequence}", self.owned_prefix());
        if id.len() > self.rules.max_len {
            return Err(IdError::TooLong {
                len: id.len(),
                max: self.rules.max_len,
            });
        }
        if let Some(c) = id.chars().find(|c| !self.rules.accepts(&c.to_string())) {
            return Err(IdError::Character {
                found: c,
                part: "composed id",
            });
        }
        Ok(id)
    }

    /// Whether an id the venue reported belongs to this process.
    #[must_use]
    pub fn owns(&self, id: &str) -> bool {
        id.starts_with(&self.owned_prefix())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The requirement's own words: an integrator adds a code **without
    /// patching the adapter**. If this needed a fork, the requirement is
    /// unmet however good the code is.
    #[test]
    fn a_broker_code_is_added_without_touching_the_adapter() {
        let plain = IdScheme::new("oq", IdRules::BINANCE).expect("valid");
        assert_eq!(plain.compose(7).as_deref(), Ok("oq7"));

        let attributed = IdScheme::new("oq", IdRules::BINANCE)
            .expect("valid")
            .with_broker(BrokerCode::new("xABC123").expect("valid"));
        assert_eq!(attributed.compose(7).as_deref(), Ok("xABC123oq7"));
    }

    /// The two prefixes are different questions and must be separable.
    /// One venue account can run several strategies under one broker
    /// code, and one operator can run one strategy under different codes
    /// per client.
    #[test]
    fn ownership_and_attribution_vary_independently() {
        let code = BrokerCode::new("xABC").expect("valid");
        let a = IdScheme::new("alpha", IdRules::BINANCE)
            .expect("valid")
            .with_broker(code.clone());
        let b = IdScheme::new("beta", IdRules::BINANCE)
            .expect("valid")
            .with_broker(code);
        assert_ne!(
            a.owned_prefix(),
            b.owned_prefix(),
            "same code, different owners"
        );

        let one = IdScheme::new("oq", IdRules::BINANCE)
            .expect("valid")
            .with_broker(BrokerCode::new("xONE").expect("valid"));
        let two = IdScheme::new("oq", IdRules::BINANCE)
            .expect("valid")
            .with_broker(BrokerCode::new("xTWO").expect("valid"));
        assert_ne!(
            one.owned_prefix(),
            two.owned_prefix(),
            "same owner, different codes"
        );
    }

    /// The venue echoes the whole id back, so an ownership check that
    /// matched only the owner segment would fail to recognise this
    /// process's own orders — and a risk gate counting resting orders
    /// would stop counting them.
    #[test]
    fn ownership_matches_the_whole_composed_prefix() {
        let s = IdScheme::new("oq", IdRules::BINANCE)
            .expect("valid")
            .with_broker(BrokerCode::new("xABC").expect("valid"));
        let id = s.compose(42).expect("valid");
        assert!(s.owns(&id));
        assert!(
            !s.owns("oq42"),
            "the same sequence without the code is not ours"
        );
        assert!(!s.owns("xDEFoq42"), "another broker's is not ours either");
    }

    /// The venues disagree, and an id legal on one and not the other
    /// fails at the venue with a message about the id — a long way from
    /// the code that composed it.
    #[test]
    fn an_id_that_the_venue_will_not_take_is_refused_here() {
        // OKX takes no punctuation. Binance does.
        assert!(IdScheme::new("oq-live", IdRules::BINANCE).is_ok());
        assert!(matches!(
            IdScheme::new("oq-live", IdRules::OKX),
            Err(IdError::Character { found: '-', .. })
        ));
    }

    /// Length is checked against the finished id, not the parts. A code
    /// and an owner that each fit can compose something that does not.
    #[test]
    fn the_length_limit_applies_to_the_composed_id() {
        let s = IdScheme::new("o".repeat(20), IdRules::OKX)
            .expect("twenty fits")
            .with_broker(BrokerCode::new("b".repeat(20)).expect("twenty fits"));
        match s.compose(1) {
            Err(IdError::TooLong { len, max }) => {
                assert_eq!(max, 32);
                assert!(len > 32);
            }
            other => panic!("expected a length refusal, got {other:?}"),
        }
    }

    /// A broker code with punctuation is refused whatever the venue
    /// allows, because a code accepted here and rejected on the next
    /// venue is a code that fails after an integration is written.
    #[test]
    fn a_broker_code_is_alphanumeric_on_every_venue() {
        assert!(BrokerCode::new("xABC123").is_ok());
        assert!(matches!(
            BrokerCode::new("x-ABC"),
            Err(IdError::Character { found: '-', .. })
        ));
        assert!(matches!(BrokerCode::new(""), Err(IdError::Empty { .. })));
    }

    /// Both shipped venues accept an id composed for them.
    #[test]
    fn both_shipped_venues_take_a_composed_id() {
        for rules in [IdRules::BINANCE, IdRules::OKX] {
            let s = IdScheme::new("oq", rules)
                .expect("valid")
                .with_broker(BrokerCode::new("xREF01").expect("valid"));
            let id = s.compose(123).expect("composable");
            assert!(rules.accepts(&id), "{id} rejected by its own venue's rules");
        }
    }
}
