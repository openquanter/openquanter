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
import time

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

    # --- what batching buys ------------------------------------------
    # The cost above is only half the trade. A binding that measured the
    # accuracy cost of throughput mode and not its speed would be
    # reporting the price without the goods.
    class Noop:
        """Does nothing, so this measures the boundary, not a strategy."""

        name = "noop"

        def on_tick(self, ctx):
            return None

        def on_batch(self, batch):
            return None

    speeds = []
    for n in (1, 8, 64, 512):
        t0 = time.perf_counter()
        oq.run_backtest(Noop(), series, balance, batch=n)
        dt = time.perf_counter() - t0
        speeds.append(len(series) / dt)
        print(f"        batch={n:<4} {len(series) / dt / 1e6:>6.2f} M ticks/s"
              f"   {speeds[-1] / speeds[0]:>5.2f}x")
    # Not a threshold: shared machines vary by several times and a tight
    # number would fail on noise. What must hold is the direction, and
    # that batching buys something rather than nothing.
    check("batching is faster than per-tick", speeds[-1] > speeds[0] * 2,
          f"{speeds[-1] / speeds[0]:.2f}x")
    check("more batching is not slower", speeds == sorted(speeds),
          f"{[round(s / 1e6, 2) for s in speeds]}")

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

    # --- ticks from a file ------------------------------------------
    #
    # The property worth asserting is not that a file can be read. It is
    # that reading one changes nothing: a run over a file and a run over
    # the same ticks in a list must agree exactly, or every number
    # measured through the streaming path belongs to a different engine
    # than the one the list path tested.
    import os
    import tempfile

    path = os.path.join(tempfile.mkdtemp(), "series.oqtk")
    written = oq.save_ticks(path, series)
    check("save_ticks writes every tick it was given", written == len(series))

    source = oq.load_ticks(path)
    check("load_ticks reads the count from the header", len(source) == len(series))
    check("and does not read the file to do it", os.path.getsize(path) > len(source) * 8)

    from_list = oq.run_backtest(Cross(), series, balance)
    from_file = oq.run_backtest(Cross(), source, balance)
    check(
        "a run from a file matches a run from a list",
        (
            from_file.ticks == from_list.ticks
            and from_file.fills == from_list.fills
            and from_file.final_equity == from_list.final_equity
        ),
        f"{from_file!r} vs {from_list!r}",
    )

    # A source is reusable, which is what holding a path rather than an
    # open reader buys. `compare_modes` depends on it.
    again = oq.run_backtest(Cross(), source, balance)
    check("a source can be run more than once", again.fills == from_file.fills)

    cmp_file = oq.compare_modes(CrossBatched, source, balance, 64)
    cmp_list = oq.compare_modes(CrossBatched, series, balance, 64)
    check(
        "compare_modes agrees across the two input paths",
        cmp_file.compat_fills == cmp_list.compat_fills
        and cmp_file.batched_fills == cmp_list.batched_fills,
    )

    try:
        oq.load_ticks(path + ".missing")
        check("a missing file is refused", False)
    except ValueError:
        check("a missing file is refused", True)

    # A file that is not this format must fail on open, not partway
    # through a run that has already reported numbers.
    junk = os.path.join(os.path.dirname(path), "junk.oqtk")
    with open(junk, "wb") as fh:
        fh.write(b"not a tick file, not even close, but long enough" * 4)
    try:
        oq.load_ticks(junk)
        check("a file that is not this format is refused on open", False)
    except ValueError as e:
        check("a file that is not this format is refused on open", "magic" in str(e).lower())

    print()
    if FAILURES:
        print(f"{len(FAILURES)} failure(s): {', '.join(FAILURES)}")
        return 1
    print("the Python tier holds")
    return 0


if __name__ == "__main__":
    sys.exit(main())
