//! Reading the flat JSON a venue sends back.
//!
//! Hand-written rather than a dependency, for the same reason the hashes
//! are: the shapes are flat and known, and a dependency in the path that
//! reads an account's positions is a dependency that has to be trusted
//! with them.
//!
//! This lives apart from any one venue because the second venue is what
//! revealed which of it was venue-specific and which was not. None of
//! it is: OKX wraps its payloads in an envelope Binance does not have,
//! and its fields are spelled differently, but a scan for a key in a
//! flat object is the same scan.

use crate::VenueError;

/// Split a JSON array into its top-level objects.
pub(crate) fn objects(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut start = None;
    let mut in_string = false;
    let mut escaped = false;
    for (i, c) in body.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            '{' if !in_string => {
                if depth == 0 {
                    start = Some(i);
                }
                depth += 1;
            }
            '}' if !in_string => {
                depth = depth.saturating_sub(1);
                if depth == 0
                    && let Some(s) = start.take()
                {
                    out.push(body[s..=i].to_string());
                }
            }
            _ => {}
        }
    }
    out
}

/// The raw text following `"key":`, quoted or not.
///
/// Escapes are honoured for the same reason [`objects`] honours them: the
/// two functions read the same bytes, and a value that ends at a
/// different place depending on which one is looking is a value that gets
/// silently truncated. `clientOrderId` is the field a reconciler matches
/// on, so a truncated one is an order that appears to have vanished.
/// The innermost JSON object containing `needle`.
///
/// Brace-matched outward from the match, honouring strings, so a
/// contract's own definition is returned rather than the array or the
/// document that holds it. [`objects`] cannot do this: exchangeInfo is a
/// single top-level object, so splitting at depth zero yields the whole
/// body, and reading a field from that returns whichever contract
/// happens to be listed first — which is right for exactly one symbol
/// and silently wrong for every other.
pub(crate) fn object_containing(body: &str, needle: &str) -> Option<String> {
    let at = body.find(needle)?;
    let mut depth = 0i32;
    let mut start = None;
    for (i, c) in body[..at].char_indices().rev() {
        match c {
            '}' => depth += 1,
            '{' if depth == 0 => {
                start = Some(i);
                break;
            }
            '{' => depth -= 1,
            _ => {}
        }
    }
    let start = start?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (i, c) in body[start..].char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            '{' if !in_string => depth += 1,
            '}' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    return Some(body[start..=start + i].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

pub(crate) fn raw_field(body: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let at = body.find(&needle)? + needle.len();
    let rest = body[at..].trim_start().strip_prefix(':')?.trim_start();
    if let Some(inner) = rest.strip_prefix('"') {
        return unescape_until_quote(inner);
    }
    let end = rest.find([',', '}', ']']).unwrap_or(rest.len());
    Some(rest[..end].trim().to_string())
}

/// Read a JSON string body up to its closing quote, resolving the escapes
/// a venue actually emits.
///
/// Not a general JSON string reader: `\u` sequences are left as written
/// because nothing in these responses uses them, and inventing a decoder
/// for a case that does not arise is how a parser gets to be wrong in
/// interesting ways.
pub(crate) fn unescape_until_quote(inner: &str) -> Option<String> {
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                '"' => out.push('"'),
                '\\' => out.push('\\'),
                '/' => out.push('/'),
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                'b' => out.push('\u{8}'),
                'f' => out.push('\u{c}'),
                other => {
                    out.push('\\');
                    out.push(other);
                }
            },
            _ => out.push(c),
        }
    }
    // No closing quote: the response was cut short, which is a malformed
    // answer rather than a value that happens to run to the end.
    None
}

/// Read a field a comparison depends on, or name the one that was absent.
///
/// The alternative is a default, and a default is what turns a changed
/// response into a changed account: `unwrap_or(0.0)` on a balance yields
/// a number no different from an empty account, and zero is a value a
/// risk gate acts on rather than stops at.
pub(crate) fn need_f64(body: &str, key: &'static str) -> Result<f64, VenueError> {
    field_f64(body, key).ok_or_else(|| malformed(key, body))
}

pub(crate) fn need_i64(body: &str, key: &'static str) -> Result<i64, VenueError> {
    field_i64(body, key).ok_or_else(|| malformed(key, body))
}

pub(crate) fn need_str(body: &str, key: &'static str) -> Result<String, VenueError> {
    field_str(body, key).ok_or_else(|| malformed(key, body))
}

pub(crate) fn need_bool(body: &str, key: &'static str) -> Result<bool, VenueError> {
    field_bool(body, key).ok_or_else(|| malformed(key, body))
}

/// Enough of the response to identify what arrived, and not a megabyte of
/// it: a thousand-fill page in an error message is a message nobody reads.
pub(crate) fn malformed(what: &'static str, body: &str) -> VenueError {
    const LIMIT: usize = 512;
    let mut shown: String = body.chars().take(LIMIT).collect();
    if body.chars().nth(LIMIT).is_some() {
        shown.push('…');
    }
    VenueError::Malformed { what, body: shown }
}

pub(crate) fn field_str(body: &str, key: &str) -> Option<String> {
    raw_field(body, key)
}

pub(crate) fn field_f64(body: &str, key: &str) -> Option<f64> {
    raw_field(body, key)?.parse().ok()
}

pub(crate) fn field_i64(body: &str, key: &str) -> Option<i64> {
    raw_field(body, key)?.parse().ok()
}

pub(crate) fn field_bool(body: &str, key: &str) -> Option<bool> {
    match raw_field(body, key)?.as_str() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

// The fixtures below are Binance-shaped because that is the venue these
// scanners were written against. They stayed with the scanners rather
// than with the adapter: what they check is the scan, not the venue.
#[cfg(test)]
mod tests {
    use super::*;

    const POSITIONS: &str = r#"[
      {"symbol":"BTCUSDT","positionAmt":"0.256","entryPrice":"71444.87","positionSide":"LONG","unRealizedProfit":"-2197.75"},
      {"symbol":"BTCUSDT","positionAmt":"-0.004","entryPrice":"62820.40","positionSide":"SHORT","unRealizedProfit":"-0.16"},
      {"symbol":"BTCUSDT","positionAmt":"0.000","entryPrice":"0.0","positionSide":"BOTH","unRealizedProfit":"0"}
    ]"#;

    #[test]
    fn both_legs_of_a_hedged_position_are_read() {
        let parsed: Vec<_> = objects(POSITIONS)
            .into_iter()
            .filter_map(|o| {
                let amount = field_f64(&o, "positionAmt")?;
                if amount == 0.0 {
                    return None;
                }
                Some((field_str(&o, "positionSide")?, amount))
            })
            .collect();
        assert_eq!(
            parsed,
            vec![("LONG".to_string(), 0.256), ("SHORT".to_string(), -0.004)]
        );
    }

    /// The venue reports a flat leg for every instrument it knows about.
    /// Keeping them turns "what is open" into a list of mostly zeroes and
    /// makes a reconciler's diff meaningless.
    #[test]
    fn flat_legs_are_dropped() {
        assert_eq!(objects(POSITIONS).len(), 3, "three legs in the payload");
        let open = objects(POSITIONS)
            .into_iter()
            .filter(|o| field_f64(o, "positionAmt").unwrap_or(0.0) != 0.0)
            .count();
        assert_eq!(open, 2);
    }

    #[test]
    fn numbers_arrive_as_strings_and_are_read_as_numbers() {
        let one = &objects(POSITIONS)[0];
        assert_eq!(field_f64(one, "entryPrice"), Some(71_444.87));
        assert_eq!(field_f64(one, "unRealizedProfit"), Some(-2_197.75));
    }

    #[test]
    fn a_brace_inside_a_string_does_not_split_an_object() {
        let body = r#"[{"clientOrderId":"a{b}c","orderId":7},{"clientOrderId":"d","orderId":8}]"#;
        let objs = objects(body);
        assert_eq!(objs.len(), 2, "got {objs:?}");
        assert_eq!(field_i64(&objs[0], "orderId"), Some(7));
        assert_eq!(field_i64(&objs[1], "orderId"), Some(8));
    }

    #[test]
    fn an_escaped_quote_does_not_end_a_string() {
        let body = r#"[{"clientOrderId":"a\"}b","orderId":9}]"#;
        let objs = objects(body);
        assert_eq!(objs.len(), 1, "got {objs:?}");
        assert_eq!(field_i64(&objs[0], "orderId"), Some(9));
    }

    #[test]
    fn booleans_are_read() {
        let body = r#"{"maker":true,"buyer":false}"#;
        assert_eq!(field_bool(body, "maker"), Some(true));
        assert_eq!(field_bool(body, "buyer"), Some(false));
    }

    /// A field a comparison depends on must fail the read rather than
    /// default. Zero is not a sentinel: an account with a zero balance
    /// and an account whose balance could not be read are the same value
    /// and opposite facts, and a risk gate acts on the first.
    #[test]
    fn a_missing_number_is_an_error_rather_than_zero() {
        let body = r#"{"totalUnrealizedProfit":"1.0"}"#;
        let err = need_f64(body, "totalWalletBalance").expect_err("must not default");
        match err {
            VenueError::Malformed { what, .. } => assert_eq!(what, "totalWalletBalance"),
            other => panic!("wrong variant: {other:?}"),
        }
        assert_eq!(
            need_f64(body, "totalUnrealizedProfit").expect("present"),
            1.0
        );
    }

    /// A renamed or absent field must not shorten the list. An order that
    /// silently drops out of "what is open" is indistinguishable from an
    /// order the venue cancelled, which is the phantom-cancel class the
    /// design document exists to avoid.
    #[test]
    fn an_unreadable_entry_fails_the_read_rather_than_shrinking_the_list() {
        let body = r#"[{"positionAmt":"0.5","symbol":"BTCUSDT","entryPrice":"100.0"}]"#;
        let objs = objects(body);
        assert_eq!(objs.len(), 1);
        // `unRealizedProfit` is absent; the entry must not simply vanish.
        assert!(need_f64(&objs[0], "unRealizedProfit").is_err());
    }

    /// `objects` already honours escapes. `raw_field` must agree with it,
    /// or a value ends at a different place depending on which function is
    /// looking — and the field this bites is the reconciliation key.
    #[test]
    fn an_escaped_quote_inside_a_value_does_not_truncate_it() {
        let body = r#"{"clientOrderId":"a\"b","orderId":9}"#;
        assert_eq!(field_str(body, "clientOrderId").as_deref(), Some("a\"b"));
        assert_eq!(field_i64(body, "orderId"), Some(9));
    }

    /// A string with no closing quote is a response that was cut short,
    /// not a value that runs to the end of the buffer.
    #[test]
    fn an_unterminated_string_is_not_a_value() {
        assert_eq!(field_str(r#"{"symbol":"BTCUSD"#, "symbol"), None);
    }
}
