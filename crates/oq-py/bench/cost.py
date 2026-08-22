"""Where a batched run's time actually goes.

    python crates/oq-py/bench/cost.py                # synthetic market
    python crates/oq-py/bench/cost.py ticks.oqtk     # a real tick file

The batch sweep in `tests/tier.py` answers how much batching buys. This
answers the question that decides whether buying more is worth anything:
of the time a run spends, how much is the boundary this crate controls
and how much is the strategy's own Python, which it does not.

It measures by subtraction, over three strategies:

    A  the rule            engine + list + iteration + arithmetic
    B  returns at once     engine + list
    C  reads t.last only   engine + list + iteration

    B      the part this crate can make cheaper
    C - B  reading one attribute per observation
    A - C  the strategy's own arithmetic, which it cannot

# Why the same measurement is run on two kinds of data

Because they disagree, and the disagreement is the finding. A synthetic
market with a strategy that does nothing is almost entirely boundary, so
removing the boundary looks like a large win. On recorded ticks with a
rule that computes something, the boundary is a minority of the run and
the batch curve turns over early. Quoting either alone would be quoting
the convenient one.

Without an argument this uses the workspace's own seeded market, so the
numbers are reproducible by anyone. With a tick file it uses that, which
is the only way to get figures that describe a real load.

# Method

Three things, each because leaving it out produced a wrong answer once:

- A warm-up pass, because the first run over a large file reads it off
  a cold disk and the ones after it read the page cache. Measured once
  each in order, the strategy that does nothing came out *slowest*.
- Two runs per variant, reporting the faster, because a single run
  competes with whatever else the machine is doing.
- A reversed-order repeat, printed, so a reader can see whether order
  still decides the answer. If those figures move much, the rest is
  noise and should not be quoted.
"""

import math
import os
import sys
import time

import openquanter as oq

BATCHES = (1, 8, 64, 512, 4096)
COST_BATCH = 512
# Overridable so CI can run the whole script cheaply. CI is checking that
# the benchmark still runs, not what it reports: a shared runner's wall
# clock is not evidence about throughput, and pinning a number measured
# there would fail on load rather than on a regression.
SYNTHETIC_TICKS = int(os.environ.get("OQ_BENCH_TICKS", 2_000_000))


class Ignores:
    """B — the batch arrives and is dropped."""

    name = "ignores"
    position = 0

    def on_batch(self, batch):
        return None


class Touches:
    """C — one attribute read per observation, nothing computed."""

    name = "touches"
    position = 0

    def on_batch(self, batch):
        x = 0
        for t in batch:
            x = t.last
        return None


class DoubleMa:
    """A — a fast average crossing a slow one, one lot each way.

    A running sum rather than a recomputed window, so what is measured is
    a strategy doing a plausible amount of arithmetic rather than a
    deliberately slow one.
    """

    name = "double_ma"

    def __init__(self, fast=10, slow=20):
        self.fw, self.sw = fast, slow
        self.fb, self.sb = [0] * fast, [0] * slow
        self.ft = self.st = 0
        self.fi = self.si = 0
        self.ff = self.sf = False
        self.pf = self.ps = None
        self.position = 0

    def _push(self, price):
        if self.ff:
            self.ft -= self.fb[self.fi]
        self.fb[self.fi] = price
        self.ft += price
        self.fi += 1
        if self.fi == self.fw:
            self.fi, self.ff = 0, True
        if self.sf:
            self.st -= self.sb[self.si]
        self.sb[self.si] = price
        self.st += price
        self.si += 1
        if self.si == self.sw:
            self.si, self.sf = 0, True
        if not (self.ff and self.sf):
            return None
        f, s = self.ft / self.fw, self.st / self.sw
        pf, ps = self.pf, self.ps
        self.pf, self.ps = f, s
        if pf is None:
            return None
        if f > s and pf < ps:
            return 1
        if f < s and pf > ps:
            return -1
        return None

    def on_tick(self, ctx):
        want = self._push(ctx.last)
        if want is None:
            return None
        side = "buy" if want > 0 else "sell"
        return [oq.Order(side, 1, "open" if ctx.position == 0 else "close")]

    def on_batch(self, batch):
        want = None
        for t in batch:
            got = self._push(t.last)
            if got is not None:
                want = got
        if want is None:
            return None
        side = "buy" if want > 0 else "sell"
        return [oq.Order(side, 1, "open" if self.position == 0 else "close")]


def synthetic(n):
    """A market with a cycle in it, so a crossover has something to cross."""
    base = 1_700_000_000_000_000_000
    out = []
    for i in range(n):
        mid = 6_000_000 + int(2000 * math.sin(i / 90.0)) + (i * 7) % 40 - 20
        out.append(
            oq.Tick(
                exch_ts=base + i * 1_000_000,
                last=mid,
                local_ts=base + i * 1_000_000 + 90_000,
                high=mid + 5,
                low=mid - 5,
                bid=mid - 2,
                ask=mid + 2,
                volume=1,
            )
        )
    return out


def run(ticks, cls, batch, balance=1_000_000):
    t0 = time.perf_counter()
    r = oq.run_backtest(cls(), ticks, balance, batch=batch)
    return time.perf_counter() - t0, r


def main():
    if len(sys.argv) > 1:
        ticks = oq.load_ticks(sys.argv[1])
        n = len(ticks)
        source = sys.argv[1]
    else:
        ticks = synthetic(SYNTHETIC_TICKS)
        n = len(ticks)
        source = f"synthetic, seeded ({n:,} ticks)"

    print(f"source    {source}")
    print(f"ticks     {n:,}")
    print()

    print("warming ...", flush=True)
    run(ticks, Ignores, COST_BATCH)

    # --- what batching buys ------------------------------------------
    print(f"{'batch':>8} {'wall s':>10} {'ticks/s':>16} {'vs batch=1':>12} {'fills':>12}")
    first = None
    for b in BATCHES:
        wall, r = run(ticks, DoubleMa, b)
        rate = r.ticks / wall
        if first is None:
            first = rate
        print(f"{b:>8} {wall:>10.2f} {rate:>16,.0f} {rate / first:>11.2f}x {r.fills:>12,}")

    # --- and what it is buying against -------------------------------
    print()
    print(f"cost split at batch={COST_BATCH}, faster of two runs each:")
    times = {}
    for label, cls in (("B", Ignores), ("C", Touches), ("A", DoubleMa)):
        a, _ = run(ticks, cls, COST_BATCH)
        b, _ = run(ticks, cls, COST_BATCH)
        times[label] = min(a, b)
        print(f"  {label} {cls.name:<10} {times[label]:8.2f} s   (runs: {a:.2f}, {b:.2f})")

    a, b, c = times["A"], times["B"], times["C"]
    print()
    print(f"  engine + one Python object per tick   {b:8.2f} s   {100 * b / a:5.1f}%")
    print(f"  reading one attribute per tick        {c - b:8.2f} s   {100 * (c - b) / a:5.1f}%")
    print(f"  the strategy's own arithmetic         {a - c:8.2f} s   {100 * (a - c) / a:5.1f}%")

    # --- did the order decide any of that? ---------------------------
    print()
    print("  reversed order, as a check on the above:")
    for label, cls in (("A", DoubleMa), ("C", Touches), ("B", Ignores)):
        t, _ = run(ticks, cls, COST_BATCH)
        print(f"    {label} {cls.name:<10} {t:8.2f} s   ({100 * (t - times[label]) / times[label]:+.1f}%)")


if __name__ == "__main__":
    main()
