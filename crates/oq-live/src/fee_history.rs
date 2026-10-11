//! Bind original venue commissions to already-recorded unknown-fee fills.

use oq_gateway::account::ExecutionFee;
use oq_types::{Cash, Fill, Nanos};

/// Match this account/symbol's stable identity and direction. The complete
/// original fill remains the attachment key in the journal. Unmatched
/// executions stay unknown and do not become a measured zero.
pub fn bind(
    fills: &[Fill],
    rows: &[ExecutionFee],
    received: Nanos,
) -> Result<Vec<(Fill, i64, Cash)>, String> {
    let mut unique = std::collections::BTreeMap::new();
    for row in rows {
        let key = (row.trade_id, row.side == oq_types::Side::Buy);
        if row.trade_id == 0 || row.time_ms > received.0 / 1_000_000 {
            return Err("fee history has an unidentifiable or future row".into());
        }
        if unique.insert(key, row).is_some_and(|prior| prior != row) {
            return Err("fee history has conflicting execution identity evidence".into());
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for fill in fills {
        if fill.trade.0 == 0
            || fill.stamp.local > received
            || fill.qty.0 <= 0
            || fill.price.0 <= 0
            || fills
                .first()
                .is_some_and(|first| first.instrument != fill.instrument)
        {
            return Err("fee enrichment requires valid prior executions of one instrument".into());
        }
        let key = (fill.trade.0, fill.side == oq_types::Side::Buy);
        if !seen.insert(key) {
            return Err("unknown-fee inventory repeats an execution identity".into());
        }
        if let Some(row) = unique.get(&key) {
            out.push((*fill, row.time_ms, row.amount));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oq_types::{
        InstrumentId, Liquidity, Offset, OrderId, PriceTicks, QtyLots, Side, Stamp, TradeId,
    };

    fn fill(side: Side, id: u64) -> Fill {
        Fill {
            instrument: InstrumentId(1),
            order: OrderId(1),
            trade: TradeId(id),
            side,
            offset: Offset::Open,
            liquidity: Liquidity::Taker,
            qty: QtyLots(1),
            price: PriceTicks(100),
            stamp: Stamp::new(1_000_000, 1_000_000),
        }
    }
    fn row(side: Side, id: u64, amount: i64) -> ExecutionFee {
        ExecutionFee {
            trade_id: id,
            side,
            time_ms: 1,
            amount: Cash(amount),
        }
    }

    #[test]
    fn stable_identity_and_side_bind_rebates_and_measured_zero_without_double_counting() {
        let fills = [fill(Side::Buy, 1), fill(Side::Sell, 1), fill(Side::Buy, 2)];
        let rows = [
            row(Side::Buy, 1, -7),
            row(Side::Buy, 1, -7),
            row(Side::Sell, 1, 0),
        ];
        let bound = bind(&fills, &rows, Nanos(2_000_000)).unwrap();
        assert_eq!(bound, vec![(fills[0], 1, Cash(-7)), (fills[1], 1, Cash(0))]);
        assert_eq!(bound.len(), 2, "unmatched fill remains unknown");
    }

    #[test]
    fn conflicts_future_rows_and_repeated_unpriced_inventory_are_refused() {
        let fill = fill(Side::Buy, 1);
        let mut foreign = fill;
        foreign.instrument = InstrumentId(2);
        foreign.trade = TradeId(2);
        assert!(bind(&[fill, foreign], &[row(Side::Buy, 1, 7)], Nanos(2_000_000)).is_err());
        assert!(
            bind(
                &[fill],
                &[row(Side::Buy, 1, 7), row(Side::Buy, 1, 8)],
                Nanos(2_000_000)
            )
            .is_err()
        );
        assert!(bind(&[fill], &[row(Side::Buy, 1, 7)], Nanos(0)).is_err());
        assert!(bind(&[fill, fill], &[row(Side::Buy, 1, 7)], Nanos(2_000_000)).is_err());
    }
}
