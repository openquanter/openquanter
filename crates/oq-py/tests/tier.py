"""The Python tier, tested from Python — which is the only place the
claims are actually true or false.

The Rust side can prove that a callback fires. It cannot prove that a
strategy someone would write runs unmodified, that batch=1 really is
compatibility mode, or that the numbers a user reads mean what the
argument names say. Those are properties of the boundary, so they are
tested from the far side of it.

    python crates/oq-py/tests/tier.py
"""

import math
import sys

import openquanter as oq


def ticks(n):
    """A market with a cycle in it, so a crossover has something to cross."""
    out = []
    for i in range(n):
        mid = 6_000_000 + int(2000 * math.sin(i / 90.0)) + (i * 7) % 40 - 20
        out.append(
            oq.Tick(
                exch_ts=1_700_000_000_000_000_000 + i * 1_000_000,
                last=mid,
                local_ts=1_700_000_000_000_000_000 + i * 1_000_000 + 90_000,
                high=mid + 5,
                low=mid - 5,
                bid=mid - 2,
                ask=mid + 2,
                volume=i,
            )
        )
    return out


class Cross:
    """A strategy written the obvious way, with no framework in it."""

    name = "cross"

    def __init__(self, fast=10, slow=60):
        self.fast, self.slow, self.hist, self.long = fast, slow, [], False

    def _signal(self, px):
        self.hist.append(px)
        if len(self.hist) < self.slow:
            return None
        if len(self.hist) > self.slow * 2:
            del self.hist[: self.slow]
        return (
            sum(self.hist[-self.fast :]) / self.fast
            > sum(self.hist[-self.slow :]) / self.slow
        )

    def on_tick(self, ctx):
        want = self._signal(ctx.last)
        if want is None or want == self.long:
            return None
        self.long = want
        return [
            oq.Order(
                "buy" if want else "sell", 1, "open" if ctx.position == 0 else "close"
            )
        ]


class CrossBatched(Cross):
    """The same strategy converted to throughput mode."""

    name = "cross-batched"

    def on_batch(self, batch):
        want = None
        for t in batch:
            want = self._signal(t.last)
        if want is None or want == self.long:
            return None
        self.long = want
        # `self.position` was mirrored onto this object before the call,
        # so reading it costs nothing.
        return [
            oq.Order(
                "buy" if want else "sell", 1, "open" if self.position == 0 else "close"
            )
        ]


FAILURES = []


def check(name, condition, detail=""):
    if condition:
        print(f"  ok    {name}")
    else:
        print(f"  FAIL  {name}  {detail}")
        FAILURES.append(name)


def main():
    series = ticks(50_000)
    balance = 100_000

    # --- compatibility mode ------------------------------------------
    r = oq.run_backtest(Cross(), series, balance)
    check("a strategy with no framework in it runs", r.ticks == len(series), repr(r))
    check("it trades", r.fills > 0, repr(r))
    # The failure that got past the first version of this file: a run
    # liquidated on every trade produces numbers that look like results.
    check("it is not a run of liquidations", r.liquidations == 0, repr(r))

    # --- G7: batch=1 is compatibility mode ---------------------------
    same = oq.compare_modes(Cross, series, balance, batch=1)
    check("batch=1 is exactly compatibility mode", same.identical, repr(same))

    # --- what batching costs -----------------------------------------
    profit = r.final_equity - balance * oq.CASH_SCALE
    check("the strategy has an edge to lose", profit > 0, f"profit={profit}")

    costs = []
    for n in (2, 8, 64, 512):
        c = oq.compare_modes(CrossBatched, series, balance, batch=n)
        costs.append(-c.equity_difference)
        print(f"        batch={n:<4} costs {-c.equity_difference / oq.CASH_SCALE:>10.2f}")
    check(
        "batching costs more the larger the batch",
        costs == sorted(costs),
        f"costs={costs}",
    )
    check(
        "a large enough batch destroys the edge",
        costs[-1] > profit,
        f"cost={costs[-1]} profit={profit}",
    )

    # --- refusals ----------------------------------------------------
    try:
        oq.Order("long", 1)
        check("a bad side is refused", False)
    except ValueError as e:
        check("a bad side is refused", "buy" in str(e), str(e))

    try:
        oq.Order("buy", 0)
        check("a zero quantity is refused", False)
    except ValueError:
        check("a zero quantity is refused", True)

    class NoBatch:
        name = "no-batch"

        def on_tick(self, ctx):
            return None

    try:
        oq.run_backtest(NoBatch(), series[:10], balance, batch=4)
        check("a strategy without on_batch cannot run batched", False)
    except TypeError as e:
        check("a strategy without on_batch cannot run batched", "on_batch" in str(e))

    class Angry:
        name = "angry"

        def on_tick(self, ctx):
            raise RuntimeError("deliberate")

    try:
        oq.run_backtest(Angry(), series[:10], balance)
        check("an exception in a strategy reaches the caller", False)
    except ValueError as e:
        check("an exception in a strategy reaches the caller", "deliberate" in str(e))

    class Confused:
        name = "confused"

        def on_tick(self, ctx):
            return "buy one please"

    try:
        oq.run_backtest(Confused(), series[:10], balance)
        check("a nonsense return value is refused, not ignored", False)
    except ValueError as e:
        check("a nonsense return value is refused, not ignored", "Order" in str(e))

    print()
    if FAILURES:
        print(f"{len(FAILURES)} failure(s): {', '.join(FAILURES)}")
        return 1
    print("the Python tier holds")
    return 0


if __name__ == "__main__":
    sys.exit(main())
