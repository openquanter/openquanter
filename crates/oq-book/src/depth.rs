//! Parsing depth messages into exact numbers.
//!
//! Two jobs, both narrow and both easy to get subtly wrong:
//!
//! 1. **Decimal strings to fixed point.** The venue sends prices as
//!    text — `"63090.10"` — and the engine works in integer ticks.
//!    Going through `f64` would be the obvious shortcut and is wrong:
//!    the conversion is lossy, the loss is silent, and the number it
//!    corrupts is the one that later decides whether an order filled.
//!    Parsing digits directly is exact and not much code.
//! 2. **Locating the fields.** Sequence numbers and the bid and ask
//!    arrays, extracted by scanning rather than by building a document.
//!
//! The parser handles the specific shapes this venue sends. It is not a
//! JSON library and should not grow into one: a message it cannot parse
//! is reported as unparseable rather than guessed at, because a
//! silently misread depth update produces a book that is wrong in a way
//! nothing downstream can detect.

/// Why a message could not be turned into an update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// A required field was absent.
    MissingField(&'static str),
    /// A number was not in a form this parser accepts.
    BadNumber(String),
    /// More decimal places than the configured scale can represent.
    ///
    /// Rejected rather than rounded. Rounding a price silently changes
    /// which side of a limit it falls on.
    TooPrecise { text: String, scale: u32 },
    /// The message is not a depth update at all.
    NotDepth,
}

impl core::fmt::Display for ParseError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::MissingField(name) => write!(f, "missing field {name}"),
            Self::BadNumber(text) => write!(f, "malformed number {text:?}"),
            Self::TooPrecise { text, scale } => {
                write!(f, "{text:?} has more decimals than scale {scale} allows")
            }
            Self::NotDepth => f.write_str("not a depth update"),
        }
    }
}

impl core::error::Error for ParseError {}

/// Parse a decimal string into fixed point with `scale` decimal places.
///
/// `parse_fixed("63090.10", 2) == Ok(6_309_010)`
///
/// # Errors
///
/// [`ParseError::BadNumber`] for anything that is not a plain decimal,
/// and [`ParseError::TooPrecise`] when the text carries more decimals
/// than `scale` — a case that is refused rather than rounded, because
/// rounding a price changes which side of a limit it lands on and the
/// caller cannot tell it happened.
pub fn parse_fixed(text: &str, scale: u32) -> Result<i64, ParseError> {
    let bad = || ParseError::BadNumber(text.to_string());
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    if digits.is_empty() {
        return Err(bad());
    }

    let (int_part, frac_part) = match digits.split_once('.') {
        Some((i, f)) => (i, f),
        None => (digits, ""),
    };
    if int_part.is_empty() && frac_part.is_empty() {
        return Err(bad());
    }
    if !int_part.bytes().all(|b| b.is_ascii_digit())
        || !frac_part.bytes().all(|b| b.is_ascii_digit())
    {
        return Err(bad());
    }

    // Trailing zeros beyond the scale carry no value, so "1.500" at
    // scale 2 is fine while "1.505" is not.
    let significant = frac_part.trim_end_matches('0');
    if significant.len() > scale as usize {
        return Err(ParseError::TooPrecise {
            text: text.to_string(),
            scale,
        });
    }

    let mut value: i64 = if int_part.is_empty() {
        0
    } else {
        int_part.parse().map_err(|_| bad())?
    };

    for i in 0..scale as usize {
        value = value.checked_mul(10).ok_or_else(bad)?;
        if let Some(d) = frac_part.as_bytes().get(i) {
            value = value.checked_add(i64::from(d - b'0')).ok_or_else(bad)?;
        }
    }

    Ok(if negative { -value } else { value })
}

/// One side of a depth update: price and the new quantity at it.
///
/// A quantity of zero means the level is gone. That is the venue's
/// encoding and it is preserved rather than filtered, because "this
/// level was removed" and "this level was not mentioned" are different
/// facts and only one of them is an instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Level {
    /// Price in ticks.
    pub price: i64,
    /// Quantity in lots. Zero removes the level.
    pub qty: i64,
}

/// A parsed incremental depth update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepthUpdate {
    /// Exchange event time, milliseconds.
    pub event_ms: i64,
    /// First update id covered by this message.
    pub first_id: u64,
    /// Final update id covered by this message.
    pub final_id: u64,
    /// Final update id of the *previous* message, when the venue sends
    /// one. Its absence is why gap detection needs a fallback.
    pub prev_final_id: Option<u64>,
    /// Bid side changes.
    pub bids: Vec<Level>,
    /// Ask side changes.
    pub asks: Vec<Level>,
}

/// Scales used to convert the venue's decimal strings to integers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scales {
    /// Decimal places in a price.
    pub price: u32,
    /// Decimal places in a quantity.
    pub qty: u32,
}

impl Default for Scales {
    fn default() -> Self {
        // Enough for the instruments this captures today. A venue with
        // finer ticks needs a different value, not a rounder one.
        Self { price: 2, qty: 3 }
    }
}

/// Parse an incremental depth message.
///
/// # Errors
///
/// See [`ParseError`].
pub fn parse_depth(payload: &[u8], scales: Scales) -> Result<DepthUpdate, ParseError> {
    let text = core::str::from_utf8(payload).map_err(|_| ParseError::NotDepth)?;
    if !text.contains("\"depthUpdate\"") {
        return Err(ParseError::NotDepth);
    }

    Ok(DepthUpdate {
        event_ms: int_field(text, "\"E\":").ok_or(ParseError::MissingField("E"))? as i64,
        first_id: int_field(text, "\"U\":").ok_or(ParseError::MissingField("U"))?,
        final_id: int_field(text, "\"u\":").ok_or(ParseError::MissingField("u"))?,
        prev_final_id: int_field(text, "\"pu\":"),
        bids: levels(text, "\"b\":", scales)?,
        asks: levels(text, "\"a\":", scales)?,
    })
}

/// Read an unsigned integer that follows `key`.
fn int_field(text: &str, key: &str) -> Option<u64> {
    let mut from = 0usize;
    while let Some(pos) = text[from..].find(key) {
        let start = from + pos + key.len();
        let digits: String = text[start..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        if !digits.is_empty() {
            return digits.parse().ok();
        }
        from = start;
    }
    None
}

/// Read an array of `["price","qty"]` pairs that follows `key`.
fn levels(text: &str, key: &str, scales: Scales) -> Result<Vec<Level>, ParseError> {
    let Some(pos) = text.find(key) else {
        // An absent side means no changes on it, which is common and
        // not an error.
        return Ok(Vec::new());
    };
    let rest = &text[pos + key.len()..];
    let Some(open) = rest.find('[') else {
        return Err(ParseError::MissingField("array"));
    };

    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut pair: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_string = false;

    for ch in rest[open..].chars() {
        match ch {
            '"' => {
                if in_string {
                    pair.push(core::mem::take(&mut current));
                }
                in_string = !in_string;
            }
            _ if in_string => current.push(ch),
            '[' => {
                depth += 1;
                pair.clear();
            }
            ']' => {
                depth -= 1;
                if depth == 1 {
                    if pair.len() != 2 {
                        return Err(ParseError::MissingField("price/qty pair"));
                    }
                    out.push(Level {
                        price: parse_fixed(&pair[0], scales.price)?,
                        qty: parse_fixed(&pair[1], scales.qty)?,
                    });
                }
                if depth == 0 {
                    break;
                }
            }
            _ => {}
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_decimals_exactly() {
        assert_eq!(parse_fixed("63090.10", 2), Ok(6_309_010));
        assert_eq!(parse_fixed("63090", 2), Ok(6_309_000));
        assert_eq!(parse_fixed("0.01", 2), Ok(1));
        assert_eq!(parse_fixed("-1.25", 2), Ok(-125));
        assert_eq!(parse_fixed("1.5", 2), Ok(150));
        // Trailing zeros are not extra precision.
        assert_eq!(parse_fixed("1.500", 2), Ok(150));
        assert_eq!(parse_fixed("0", 3), Ok(0));
    }

    #[test]
    fn a_price_that_does_not_fit_the_scale_is_refused_not_rounded() {
        // The whole reason this returns an error: rounding changes
        // which side of a limit a price falls on, and the caller cannot
        // tell it happened.
        assert_eq!(
            parse_fixed("1.005", 2),
            Err(ParseError::TooPrecise {
                text: "1.005".to_string(),
                scale: 2
            })
        );
    }

    #[test]
    fn rejects_what_it_cannot_read_rather_than_guessing() {
        for bad in ["", "abc", "1.2.3", "1e5", " 1.0", "1,5", "-"] {
            assert!(
                matches!(parse_fixed(bad, 2), Err(ParseError::BadNumber(_))),
                "{bad:?} should not parse"
            );
        }
    }

    #[test]
    fn float_would_have_lost_these() {
        // Not every decimal survives a round trip through f64. "0.29"
        // becomes 28 cents, "0.57" becomes 56. The loss is a single
        // tick, it is silent, and it lands on the number that decides
        // whether a limit order filled.
        //
        // The first draft of this test picked a price that happened to
        // be exact in binary and asserted the opposite, which is a good
        // illustration of why this is checked rather than reasoned
        // about: the failures are not where intuition puts them.
        for (text, expected) in [("0.29", 29_i64), ("0.57", 57), ("1.13", 113)] {
            assert_eq!(parse_fixed(text, 2), Ok(expected));

            #[allow(clippy::cast_possible_truncation)]
            let via_float = (text.parse::<f64>().expect("float") * 100.0) as i64;
            assert_ne!(
                via_float, expected,
                "{text} happens to survive f64; pick another for this test"
            );
        }
    }

    const SAMPLE: &[u8] = br#"{"e":"depthUpdate","E":1786780800123,"T":1786780800120,"s":"BTCUSDT","U":100,"u":110,"pu":99,"b":[["63090.10","1.500"],["63089.00","0"]],"a":[["63091.20","0.250"]]}"#;

    #[test]
    fn parses_a_depth_update() {
        let u = parse_depth(SAMPLE, Scales::default()).expect("parses");
        assert_eq!(u.event_ms, 1_786_780_800_123);
        assert_eq!(
            (u.first_id, u.final_id, u.prev_final_id),
            (100, 110, Some(99))
        );
        assert_eq!(
            u.bids,
            vec![
                Level {
                    price: 6_309_010,
                    qty: 1_500
                },
                Level {
                    price: 6_308_900,
                    qty: 0
                },
            ]
        );
        assert_eq!(
            u.asks,
            vec![Level {
                price: 6_309_120,
                qty: 250
            }]
        );
    }

    #[test]
    fn a_zero_quantity_survives_parsing() {
        // It is a removal instruction, not noise. Filtering it here
        // would leave stale levels in every reconstructed book.
        let u = parse_depth(SAMPLE, Scales::default()).expect("parses");
        assert!(u.bids.iter().any(|l| l.qty == 0));
    }

    #[test]
    fn an_empty_side_is_not_an_error() {
        let payload =
            br#"{"e":"depthUpdate","E":1,"U":1,"u":2,"pu":0,"b":[],"a":[["1.00","1.000"]]}"#;
        let u = parse_depth(payload, Scales::default()).expect("parses");
        assert!(u.bids.is_empty());
        assert_eq!(u.asks.len(), 1);
    }

    #[test]
    fn rejects_messages_that_are_not_depth_updates() {
        assert_eq!(
            parse_depth(br#"{"e":"trade","E":1}"#, Scales::default()),
            Err(ParseError::NotDepth)
        );
    }
}
