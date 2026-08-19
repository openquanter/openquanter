//! Recent history, as the ticks a strategy would have seen.
//!
//! A strategy whose indicator needs two hundred bars cannot act for the
//! first two hundred minutes of a run — three hours and twenty minutes,
//! measured on the deployment this was written for. Every restart pays
//! it again, which on a production account is that long unable to manage
//! whatever it is holding.
//!
//! The reference implementation loads a day of bars at startup and falls
//! back to the venue's REST endpoint when its database has none. This is
//! the fallback, and it is the only path here: a venue always has its
//! own recent history, and depending on a database to start is a second
//! thing that can be down.
//!
//! # Why ticks rather than bars
//!
//! Because live data is ticks. A strategy folds history with the code it
//! already runs — its own bar generator, its own windows — instead of a
//! second path that has to agree with the first. Two paths that must
//! agree, with nothing forcing them to, is the shape this project exists
//! to argue against.
//!
//! # What a bar cannot say
//!
//! There is no book in a kline, so `bid` and `ask` are zero, which is
//! this format's word for unknown. `last`, `high` and `low` are real.
//! Volume is accumulated across the returned bars rather than reported
//! per bar, because that is the convention live ticks carry and a
//! consumer takes differences between consecutive observations.

/// One completed bar, in this instrument's own units.
///
/// Deliberately plain integers and no engine types: this crate talks to
/// venues and does not depend on the engine, and a dependency added for
/// one struct would be paid by everything that links a gateway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Kline {
    /// When the bar opened, milliseconds since the epoch.
    pub open_ms: i64,
    pub high: i64,
    pub low: i64,
    pub close: i64,
    /// Traded in this bar alone, not cumulative.
    pub volume: i64,
}

/// One row of Binance's kline response, positionally.
///
/// The venue returns an array of arrays rather than objects, so the
/// fields are identified by position and nothing here can check a name.
/// The indices are pinned by the tests below against a real response.
const OPEN_TIME: usize = 0;
const HIGH: usize = 2;
const LOW: usize = 3;
const CLOSE: usize = 4;
const VOLUME: usize = 5;

/// Split a JSON array of arrays into its rows' contents.
///
/// Deliberately small: the payload is numbers and quoted decimals, with
/// no nesting below the second level and no escapes to speak of. A
/// general parser would be more code to audit for a shape this fixed.
fn rows(body: &str) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    let mut depth = 0u32;
    let mut field = String::new();
    let mut row: Vec<String> = Vec::new();
    let mut in_string = false;
    for c in body.chars() {
        match c {
            '"' => in_string = !in_string,
            '[' if !in_string => {
                depth += 1;
                if depth == 2 {
                    row = Vec::new();
                    field = String::new();
                }
            }
            ']' if !in_string => {
                if depth == 2 {
                    row.push(core::mem::take(&mut field));
                    out.push(core::mem::take(&mut row));
                }
                depth = depth.saturating_sub(1);
            }
            ',' if !in_string && depth == 2 => row.push(core::mem::take(&mut field)),
            _ if depth == 2 => field.push(c),
            _ => {}
        }
    }
    out
}

/// A quoted or bare decimal as an integer count at `scale`.
fn scaled(text: &str, scale: u8) -> Option<i64> {
    let t = text.trim();
    let (whole, frac) = t.split_once('.').unwrap_or((t, ""));
    let mut digits = String::from(whole);
    let want = usize::from(scale);
    for i in 0..want {
        digits.push(frac.as_bytes().get(i).copied().unwrap_or(b'0') as char);
    }
    digits.parse::<i64>().ok()
}

/// Read a kline response, oldest first.
///
/// Returns `None` if the body is not the shape this expects, rather than
/// a partial list: a warm-up that silently loads half its history is one
/// whose indicator is wrong in a way nothing reports.
#[must_use]
pub fn parse(body: &str, price_scale: u8, qty_scale: u8) -> Option<Vec<Kline>> {
    let rows = rows(body);
    if rows.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        if r.len() <= VOLUME {
            return None;
        }
        out.push(Kline {
            open_ms: r[OPEN_TIME].trim().parse::<i64>().ok()?,
            high: scaled(&r[HIGH], price_scale)?,
            low: scaled(&r[LOW], price_scale)?,
            close: scaled(&r[CLOSE], price_scale)?,
            volume: scaled(&r[VOLUME], qty_scale)?,
        });
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::{Kline, parse};

    /// One row of a real response, abbreviated to the fields read.
    ///
    /// The venue returns arrays rather than objects, so every field is
    /// identified by position and nothing in the parser can check a
    /// name. That makes this test the only thing standing between a
    /// reordered response and a strategy warmed on the wrong column —
    /// high read as low, or volume read as a close.
    const BODY: &str = r#"[
      [1499040000000,"0.01634790","0.80000000","0.01575800","0.01577100","148976.11427815",
       1499644799999,"2434.19055334",308,"1756.87402397","28.46694368","0"],
      [1499040060000,"0.01577100","0.02000000","0.01500000","0.01900000","100.00000000",
       1499644859999,"2434.19055334",308,"1756.87402397","28.46694368","0"]
    ]"#;

    #[test]
    fn each_column_is_read_from_its_own_position() {
        let k = parse(BODY, 8, 8).expect("parses");
        assert_eq!(k.len(), 2);
        assert_eq!(
            k[0],
            Kline {
                open_ms: 1_499_040_000_000,
                // index 2, not 1: open is 0.01634790 and high is 0.8
                high: 80_000_000,
                low: 1_575_800,
                close: 1_577_100,
                volume: 14_897_611_427_815,
            }
        );
    }

    /// Oldest first, because that is the order a replay needs and the
    /// order the venue returns. Reversing it would warm an indicator
    /// backwards, which converges to a number and reports no error.
    #[test]
    fn bars_come_back_oldest_first() {
        let k = parse(BODY, 8, 8).expect("parses");
        assert!(k[0].open_ms < k[1].open_ms, "{k:?}");
    }

    /// Volume is per bar. Accumulating is the consumer's business,
    /// because only the consumer knows what convention it is feeding.
    #[test]
    fn volume_is_this_bar_alone() {
        let k = parse(BODY, 8, 2).expect("parses");
        assert_eq!(k[1].volume, 10_000, "100.0 at two decimal places");
    }

    /// A short row is refused rather than half-read.
    ///
    /// A warm-up that silently loads part of its history leaves an
    /// indicator that is wrong and an error that was never raised.
    #[test]
    fn a_truncated_row_is_refused() {
        assert!(parse(r#"[[1499040000000,"1.0","2.0"]]"#, 2, 2).is_none());
    }

    #[test]
    fn an_empty_response_is_refused() {
        assert!(parse("[]", 2, 2).is_none());
        assert!(parse("", 2, 2).is_none());
    }

    /// Scaling matches the instrument, not the string.
    #[test]
    fn decimals_scale_to_the_instrument() {
        let k = parse(
            r#"[[1,"0","0","0","65432.10","1.5",2,"0",0,"0","0","0"]]"#,
            2,
            3,
        )
        .expect("parses");
        assert_eq!(k[0].close, 6_543_210, "two decimal places");
        assert_eq!(k[0].volume, 1_500, "three decimal places");
    }
}
