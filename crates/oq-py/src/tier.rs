//! Tier B: a strategy written in Python, run by the Rust engine.
//!
//! # Two modes, and the thing that separates them
//!
//! **Compatibility mode** calls the strategy once per tick. It is the
//! mode a strategy is written in, it is exactly the semantics of the
//! Rust trait, and it costs one FFI round trip per observation.
//!
//! **Throughput mode** calls it once per batch of `n` ticks and mirrors
//! the account state onto the strategy object as plain attributes, so
//! Python reads locals instead of crossing the boundary for each field.
//!
//! Batching is not free and the cost is not hidden here: a strategy
//! called every `n` ticks *cannot* act on ticks 1..n-1 of a batch, so its
//! decisions are late by up to `n - 1` ticks. There is no way to batch
//! and preserve per-tick semantics — a decision made after seeing tick
//! `i` cannot be placed before tick `i` was seen — and any binding that
//! claims otherwise is either not batching or not preserving them.
//!
//! What follows from that is the design:
//!
//! - `n = 1` is exactly compatibility mode, and a test asserts the two
//!   produce identical fills rather than the docs asserting it.
//! - For `n > 1` the divergence is **measured**, by
//!   [`compare_modes`], not assumed small. A strategy that acts on a
//!   cadence slower than its batch will show none; a strategy that reacts
//!   to every tick will show a lot, and should not be batched.
//!
//! That is a worse story than "batching is free" and it is the true one.
//! The alternative — batching quietly and letting the user discover the
//! difference in production — is the failure this whole project is
//! organised against.
//!
//! # What a strategy looks like
//!
//! ```python
//! class Cross:
//!     name = "cross"
//!
//!     def on_tick(self, ctx):
//!         if ctx.last > self.threshold and ctx.position == 0:
//!             return [Order("buy", 1)]
//!         return None
//! ```
//!
//! In throughput mode it implements `on_batch(ticks)` instead and reads
//! `self.position`, `self.equity`, `self.entry` — mirrored before each
//! call rather than fetched across the boundary.

use oq_backtest::{Context, Intent, MarginMode, RunConfig, Strategy, run};
use oq_engine::Tick;
use oq_margin::{Contract, TierTable};
use oq_types::{Cash, InstrumentId, Offset, OrderId, QtyLots, Side};
use pyo3::exceptions::{PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyList;

/// One observation, as Python sees it.
///
/// Prices are integers in their native tick units, exactly as they are
/// on the Rust side. Scaling them here would put a float on the path
/// between the engine and the decision, which is the one place this
/// project refuses to put one.
#[pyclass(name = "Tick", frozen, get_all, from_py_object)]
#[derive(Clone, Copy)]
pub struct PyTick {
    /// When the venue says it happened, in nanoseconds.
    pub exch_ts: i64,
    /// When this process saw it, in nanoseconds.
    pub local_ts: i64,
    /// Last traded price, in ticks.
    pub last: i64,
    /// High over the observation, in ticks.
    pub high: i64,
    /// Low over the observation, in ticks.
    pub low: i64,
    /// Best bid, in ticks, or zero when the book had none.
    pub bid: i64,
    /// Best ask, in ticks, or zero when the book had none.
    pub ask: i64,
    /// Cumulative volume, in lots.
    pub volume: i64,
}

impl From<&Tick> for PyTick {
    fn from(t: &Tick) -> Self {
        Self {
            exch_ts: t.stamp.exch.0,
            local_ts: t.stamp.local.0,
            last: t.last.0,
            high: t.high.0,
            low: t.low.0,
            bid: t.bid.0,
            ask: t.ask.0,
            volume: t.volume.0,
        }
    }
}

#[pymethods]
impl PyTick {
    /// A tick. `bid` and `ask` default to zero, meaning no book — which
    /// is a real state, not a missing value, and features are expected
    /// to decline to answer for it rather than invent a mid.
    #[new]
    #[pyo3(signature = (exch_ts, last, local_ts = None, high = None, low = None, bid = 0, ask = 0, volume = 0))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        exch_ts: i64,
        last: i64,
        local_ts: Option<i64>,
        high: Option<i64>,
        low: Option<i64>,
        bid: i64,
        ask: i64,
        volume: i64,
    ) -> Self {
        Self {
            exch_ts,
            // Defaulting local to exch says "this file records no feed
            // latency", which is true of a synthetic series and is a
            // better default than zero, which would say the tick was
            // observed in 1970.
            local_ts: local_ts.unwrap_or(exch_ts),
            last,
            high: high.unwrap_or(last),
            low: low.unwrap_or(last),
            bid,
            ask,
            volume,
        }
    }

    fn __repr__(&self) -> String {
        format!(
            "Tick(exch_ts={}, last={}, bid={}, ask={})",
            self.exch_ts, self.last, self.bid, self.ask
        )
    }
}

/// The tick plus the account, as Python sees it.
///
/// Flat rather than nested: a nested `ctx.tick.last` is two attribute
/// lookups across the boundary where `ctx.last` is one, and this object
/// is built once per tick in the mode that cares about that.
#[pyclass(name = "Context", frozen, get_all, skip_from_py_object)]
#[derive(Clone, Copy)]
pub struct PyContext {
    /// The observation this call is about.
    pub tick: PyTick,
    /// Last traded price, in ticks. Shorthand for `ctx.tick.last`.
    pub last: i64,
    /// Net long position, in lots.
    pub position: i64,
    /// Average entry of the long position, in ticks.
    pub entry: i64,
    /// Net short position, in lots.
    pub short_position: i64,
    /// Average entry of the short position, in ticks.
    pub short_entry: i64,
    /// Account equity, in the cash unit's smallest denomination.
    pub equity: i64,
    /// Orders resting at the venue.
    pub working: usize,
}

impl From<&Context> for PyContext {
    fn from(c: &Context) -> Self {
        Self {
            tick: PyTick::from(&c.tick),
            last: c.tick.last.0,
            position: c.position.0,
            entry: c.entry.0,
            short_position: c.short_position.0,
            short_entry: c.short_entry.0,
            equity: c.equity.0,
            working: c.working,
        }
    }
}

#[pymethods]
impl PyContext {
    fn __repr__(&self) -> String {
        format!(
            "Context(last={}, position={}, equity={})",
            self.last, self.position, self.equity
        )
    }
}

/// An order a Python strategy wants placed.
///
/// `side` is `"buy"` or `"sell"`; `offset` is `"open"` or `"close"`.
/// Both are validated at construction rather than at submission, so a
/// typo fails on the line that wrote it instead of somewhere inside the
/// engine.
#[pyclass(name = "Order", frozen, get_all, from_py_object)]
#[derive(Clone, Copy)]
pub struct PyOrder {
    /// `"buy"` or `"sell"`.
    pub side: &'static str,
    /// Lots.
    pub qty: i64,
    /// `"open"` or `"close"`.
    pub offset: &'static str,
    /// Limit price in ticks, or `None` for a market order.
    pub price: Option<i64>,
}

#[pymethods]
impl PyOrder {
    /// An order. `offset` defaults to `"open"`, `price` to a market
    /// order.
    #[new]
    #[pyo3(signature = (side, qty, offset = "open", price = None))]
    fn new(side: &str, qty: i64, offset: &str, price: Option<i64>) -> PyResult<Self> {
        let side = match side {
            "buy" => "buy",
            "sell" => "sell",
            other => {
                return Err(PyValueError::new_err(format!(
                    "side must be \"buy\" or \"sell\", got {other:?}"
                )));
            }
        };
        let offset = match offset {
            "open" => "open",
            "close" => "close",
            other => {
                return Err(PyValueError::new_err(format!(
                    "offset must be \"open\" or \"close\", got {other:?}"
                )));
            }
        };
        if qty <= 0 {
            return Err(PyValueError::new_err(format!(
                "qty must be positive, got {qty}"
            )));
        }
        Ok(Self {
            side,
            qty,
            offset,
            price,
        })
    }

    fn __repr__(&self) -> String {
        match self.price {
            None => format!("Order({:?}, {}, {:?})", self.side, self.qty, self.offset),
            Some(p) => format!(
                "Order({:?}, {}, {:?}, price={p})",
                self.side, self.qty, self.offset
            ),
        }
    }
}

impl PyOrder {
    fn into_intent(self, id: u64) -> Intent {
        let side = if self.side == "buy" {
            Side::Buy
        } else {
            Side::Sell
        };
        let offset = if self.offset == "open" {
            Offset::Open
        } else {
            Offset::Close
        };
        let qty = QtyLots(self.qty);
        let id = OrderId(id);
        match self.price {
            None => Intent::Market {
                id,
                side,
                qty,
                offset,
            },
            Some(p) => Intent::Limit {
                id,
                side,
                qty,
                price: oq_types::PriceTicks(p),
                offset,
            },
        }
    }
}

/// How often the Python side is called.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cadence {
    /// Once per tick. Exactly the Rust trait's semantics.
    EveryTick,
    /// Once per `n` ticks, with the ticks since the last call handed
    /// over together. Decisions are late by up to `n - 1` ticks.
    Batched(usize),
}

/// A Python object, driven as a strategy.
struct PyDriven {
    object: Py<PyAny>,
    name: String,
    cadence: Cadence,
    /// Ticks seen since the last call, in throughput mode.
    pending: Vec<PyTick>,
    next_id: u64,
    /// The first error Python raised, kept so the run can report it
    /// rather than a panic mid-fold.
    failure: Option<String>,
}

impl PyDriven {
    /// Ask Python for the orders, given whatever this cadence hands it.
    fn ask(&mut self, py: Python<'_>, ctx: &Context, out: &mut Vec<Intent>) {
        if self.failure.is_some() {
            return;
        }
        let pyctx = PyContext::from(ctx);
        let result = match self.cadence {
            Cadence::EveryTick => self
                .object
                .bind(py)
                .call_method1("on_tick", (pyctx,))
                .map(pyo3::Bound::unbind),
            Cadence::Batched(_) => {
                // Mirrored state: set the fields Python reads most often
                // as plain attributes, so the strategy body does no FFI
                // at all beyond the one call.
                let obj = self.object.bind(py);
                let mirror = |name: &str, v: i64| obj.setattr(name, v);
                let mirrored = mirror("position", pyctx.position)
                    .and_then(|()| mirror("entry", pyctx.entry))
                    .and_then(|()| mirror("short_position", pyctx.short_position))
                    .and_then(|()| mirror("equity", pyctx.equity))
                    .and_then(|()| mirror("last", pyctx.last));
                let batch = core::mem::take(&mut self.pending);
                mirrored
                    .and_then(|()| PyList::new(py, batch))
                    .and_then(|list| obj.call_method1("on_batch", (list,)))
                    .map(pyo3::Bound::unbind)
            }
        };

        match result {
            Err(e) => self.failure = Some(e.to_string()),
            Ok(obj) => {
                if obj.is_none(py) {
                    return;
                }
                match obj.extract::<Vec<PyOrder>>(py) {
                    Ok(orders) => {
                        for o in orders {
                            self.next_id += 1;
                            out.push(o.into_intent(self.next_id));
                        }
                    }
                    Err(_) => {
                        self.failure = Some(
                            "a strategy must return None or a list of Order; \
                             returning anything else would be silently ignored"
                                .to_owned(),
                        );
                    }
                }
            }
        }
    }
}

impl Strategy for PyDriven {
    fn name(&self) -> &str {
        &self.name
    }

    fn on_tick(&mut self, ctx: &Context, out: &mut Vec<Intent>) {
        match self.cadence {
            Cadence::EveryTick => Python::attach(|py| self.ask(py, ctx, out)),
            Cadence::Batched(n) => {
                self.pending.push(PyTick::from(&ctx.tick));
                if self.pending.len() >= n {
                    Python::attach(|py| self.ask(py, ctx, out));
                }
            }
        }
    }
}

/// Read `strategy.name`, falling back to the class name.
fn strategy_name(py: Python<'_>, object: &Py<PyAny>) -> String {
    let bound = object.bind(py);
    bound
        .getattr("name")
        .ok()
        .and_then(|n| n.extract::<String>().ok())
        .or_else(|| {
            bound
                .get_type()
                .getattr("__name__")
                .ok()
                .and_then(|n| n.extract::<String>().ok())
        })
        .unwrap_or_else(|| "python-strategy".to_owned())
}

/// Build the tick series the engine will run over, from Python.
fn ticks_from(list: &Bound<'_, PyAny>) -> PyResult<Vec<Tick>> {
    let rows: Vec<PyTick> = list.extract().map_err(|_| {
        PyTypeError::new_err(
            "ticks must be a sequence of Tick; build them with openquanter.Tick(...) \
             or read them from a file with load_ticks()",
        )
    })?;
    Ok(rows
        .into_iter()
        .map(|t| Tick {
            stamp: oq_types::Stamp {
                exch: oq_types::Nanos(t.exch_ts),
                local: oq_types::Nanos(t.local_ts),
            },
            last: oq_types::PriceTicks(t.last),
            high: oq_types::PriceTicks(t.high),
            low: oq_types::PriceTicks(t.low),
            bid: oq_types::PriceTicks(t.bid),
            ask: oq_types::PriceTicks(t.ask),
            volume: QtyLots(t.volume),
        })
        .collect())
}

/// What a run produced.
#[pyclass(name = "RunResult", frozen, get_all)]
pub struct PyRunResult {
    /// The strategy's name.
    pub strategy: String,
    /// Observations processed.
    pub ticks: usize,
    /// Fills the strategy received.
    pub fills: usize,
    /// Times the venue closed the account.
    pub liquidations: usize,
    /// Equity at the end, in the cash unit's smallest denomination.
    pub final_equity: i64,
    /// The lowest equity the account showed.
    pub min_equity: i64,
    /// Fees paid.
    pub fees_paid: i64,
    /// Funding paid.
    pub funding_paid: i64,
    /// Prices of every fill, in order, in ticks.
    ///
    /// Carried so two runs can be compared exactly rather than through
    /// their totals, which can agree while the runs did not.
    pub fill_prices: Vec<i64>,
}

#[pymethods]
impl PyRunResult {
    fn __repr__(&self) -> String {
        // Liquidations are in the repr, and loudly, because a run where
        // the venue closed the account on every trade prints numbers
        // that look like a strategy's numbers. Having them available on
        // the object was not enough: the first thing written against
        // this binding did not look, and reported 178 liquidations as a
        // result. A repr is what gets read.
        let closed = if self.liquidations == 0 {
            String::new()
        } else {
            format!(", LIQUIDATED {}x", self.liquidations)
        };
        format!(
            "RunResult(strategy={:?}, ticks={}, fills={}, final_equity={}{closed})",
            self.strategy, self.ticks, self.fills, self.final_equity
        )
    }
}

/// Run a Python strategy over a tick series.
///
/// `balance` is in whole currency units — 100_000 means a hundred
/// thousand USDT — not in the internal fixed-point representation. The
/// internal one is exposed as `CASH_SCALE` for converting the integers
/// this returns, but is not what any argument takes: a balance argument
/// that silently meant one hundred-millionth of what the caller intended
/// produced a run liquidated on its first trade, and the numbers looked
/// plausible.
///
/// `contract_size` is how much of the underlying one lot is, at the same
/// scale. It has a default so a first run works, and it is an argument
/// because no default is right for a second one.
///
/// `batch` selects the mode: `1` (the default) is compatibility mode,
/// one call per tick, and anything larger is throughput mode. See the
/// module documentation for what batching costs.
///
/// # Errors
///
/// Propagates whatever the strategy raised, and refuses a strategy that
/// does not implement the method its cadence requires.
#[pyfunction]
// Eight arguments, all of them keyword-with-a-default on the Python
// side. Collapsing them into a config object would make the common call
// longer, not shorter, and Python callers already have keyword
// arguments — the lint is defending a Rust ergonomic that this function
// is not exposed through.
#[allow(clippy::too_many_arguments)]
#[pyo3(signature = (strategy, ticks, balance, batch = 1, enforce_margin = true, equity_every = 0, contract_size = 10_000))]
#[allow(clippy::needless_pass_by_value)]
pub fn run_backtest(
    py: Python<'_>,
    strategy: Py<PyAny>,
    ticks: &Bound<'_, PyAny>,
    balance: i64,
    batch: usize,
    enforce_margin: bool,
    equity_every: usize,
    contract_size: i64,
) -> PyResult<PyRunResult> {
    if batch == 0 {
        return Err(PyValueError::new_err(
            "batch must be at least 1; 1 is compatibility mode",
        ));
    }
    let cadence = if batch == 1 {
        Cadence::EveryTick
    } else {
        Cadence::Batched(batch)
    };
    let required = if batch == 1 { "on_tick" } else { "on_batch" };
    if !strategy.bind(py).hasattr(required)? {
        return Err(PyTypeError::new_err(format!(
            "a strategy run with batch={batch} must implement {required}()"
        )));
    }

    let series = ticks_from(ticks)?;
    let mut driven = PyDriven {
        name: strategy_name(py, &strategy),
        object: strategy,
        cadence,
        pending: Vec::with_capacity(batch),
        next_id: 0,
        failure: None,
    };

    let config = RunConfig::new(
        InstrumentId::new(1),
        Contract::new(contract_size),
        TierTable::example_btcusdt(),
        Cash::from_units(balance),
    )
    .with_margin(if enforce_margin {
        MarginMode::Enforced
    } else {
        MarginMode::Ignored
    })
    .sampling_equity_every(equity_every);

    // The engine holds no Python state, so the interpreter lock is not
    // needed for the parts of the run that are not calling back into it.
    let result = py.detach(|| run(&config, &mut driven, &series));

    if let Some(why) = driven.failure {
        return Err(PyValueError::new_err(format!(
            "the strategy failed during the run: {why}"
        )));
    }

    Ok(PyRunResult {
        strategy: result.strategy,
        ticks: result.ticks,
        fills: result.fills.len(),
        liquidations: result.liquidations.len(),
        final_equity: result.final_equity.0,
        min_equity: result.min_equity.0,
        fees_paid: result.fees_paid.0,
        funding_paid: result.funding_paid.0,
        fill_prices: result.fills.iter().map(|f| f.price.0).collect(),
    })
}

/// What batching cost, measured on this strategy and this data.
#[pyclass(name = "ModeComparison", frozen, get_all)]
pub struct PyModeComparison {
    /// Batch size the throughput run used.
    pub batch: usize,
    /// Fills in the per-tick run.
    pub compat_fills: usize,
    /// Fills in the batched run.
    pub batched_fills: usize,
    /// Index of the first fill whose price differs, or `None` when every
    /// fill matched.
    pub first_divergence: Option<usize>,
    /// Difference in final equity, batched minus per-tick.
    pub equity_difference: i64,
    /// Whether the two runs are indistinguishable.
    pub identical: bool,
}

#[pymethods]
impl PyModeComparison {
    fn __repr__(&self) -> String {
        if self.identical {
            format!("ModeComparison(batch={}, identical)", self.batch)
        } else {
            format!(
                "ModeComparison(batch={}, fills {} vs {}, first divergence {:?}, equity {:+})",
                self.batch,
                self.compat_fills,
                self.batched_fills,
                self.first_divergence,
                self.equity_difference
            )
        }
    }
}

/// Run one strategy both ways over the same ticks and report what
/// batching changed.
///
/// `build` is called twice — once per mode — because a strategy carries
/// state and reusing one instance would make the second run depend on
/// the first. Pass the class itself, or any zero-argument callable.
///
/// # Errors
///
/// Propagates whatever either run raised.
#[pyfunction]
#[pyo3(signature = (build, ticks, balance, batch, enforce_margin = true, contract_size = 10_000))]
#[allow(clippy::needless_pass_by_value)]
pub fn compare_modes(
    py: Python<'_>,
    build: Py<PyAny>,
    ticks: &Bound<'_, PyAny>,
    balance: i64,
    batch: usize,
    enforce_margin: bool,
    contract_size: i64,
) -> PyResult<PyModeComparison> {
    let make = || -> PyResult<Py<PyAny>> { Ok(build.bind(py).call0()?.unbind()) };

    let compat = run_backtest(
        py,
        make()?,
        ticks,
        balance,
        1,
        enforce_margin,
        0,
        contract_size,
    )?;
    let batched = run_backtest(
        py,
        make()?,
        ticks,
        balance,
        batch,
        enforce_margin,
        0,
        contract_size,
    )?;

    let first_divergence = compat
        .fill_prices
        .iter()
        .zip(&batched.fill_prices)
        .position(|(a, b)| a != b)
        .or_else(|| {
            (compat.fill_prices.len() != batched.fill_prices.len())
                .then(|| compat.fill_prices.len().min(batched.fill_prices.len()))
        });

    Ok(PyModeComparison {
        batch,
        compat_fills: compat.fills,
        batched_fills: batched.fills,
        first_divergence,
        equity_difference: batched.final_equity - compat.final_equity,
        identical: first_divergence.is_none() && compat.final_equity == batched.final_equity,
    })
}

/// Register the tier's types and functions on the module.
///
/// # Errors
///
/// Propagates any failure to add a class or function.
pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    // The scale the integer money fields are in, so a caller converting
    // them does not have to guess or hard-code 1e8.
    m.add("CASH_SCALE", oq_types::CASH_SCALE)?;
    m.add_class::<PyTick>()?;
    m.add_class::<PyContext>()?;
    m.add_class::<PyOrder>()?;
    m.add_class::<PyRunResult>()?;
    m.add_class::<PyModeComparison>()?;
    m.add_function(wrap_pyfunction!(run_backtest, m)?)?;
    m.add_function(wrap_pyfunction!(compare_modes, m)?)?;
    Ok(())
}
