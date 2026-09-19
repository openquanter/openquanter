# More venues

[English](VENUES.md) · [中文](VENUES.zh-CN.md)

What it takes to add Coinbase, Backpack, Kraken, Bitget, Lighter,
Hyperliquid and Aster to a gateway that currently speaks to two venues,
and what has to change before the first line of any of them is written.

Design first, for the reason [the live path](LIVE-PATH.md) was: the
survey there found nine systems that built the order path and left
reconciliation for later, and every one carries the same class of bug.
The equivalent mistake here is writing seven adapters against the shape
the first two happened to need.

## The claim this rests on

**Seven venues are not seven problems.** They are four signing families,
three structural differences, and one question about cryptography that
has to be answered before any of the three decentralised ones can be
started. Sorted that way, two of the seven are nearly free and one of
them should probably not be built at all.

## What the survey found

### Coinbase's perpetuals are no longer at Coinbase

Coinbase International Exchange stopped serving derivatives on
**2026-09-09**, nine days before this was written. Perpetuals in the
Coinbase app and on coinbase.com now run on a Deribit-powered gateway,
and API users were moved to a **JSON-RPC** endpoint. The INTX REST API
this document would otherwise have targeted is marked deprecated in
Coinbase's own developer documentation.

An INTX adapter would therefore be an adapter to something that has been
switched off. The real target is Deribit's JSON-RPC, which is not a
variation on the REST venues here — it is a different protocol family,
with its own request/response identity model.

This is the survey's best result: it removed work rather than adding it.

### Bitget is moving too, and it started three days ago

Bitget began migrating classic accounts to a **Unified Trading Account**
on **2026-09-15**, in batches by user activity. The API consequence is
stated plainly in their upgrade guide: **a UTA key cannot call the
classic endpoints**. Integrations on the classic API (v2) have to move
to the UTA API (v3).

So the `/api/v2/mix/*` paths this document would have targeted are the
ones that stop working for every account that gets migrated, and the
migration is under way rather than announced. A classic-API adapter
would be the Coinbase mistake made a second time, with three days'
notice instead of nine.

The target is UTA v3. That is not a rename: it is a different account
model — one balance across products rather than a margin coin per
contract — so the position and balance reads are shaped differently even
where the signing is not.

**Two of seven venues turned out to be mid-migration.** That is the
strongest argument this survey produced for doing the survey: both were
found by reading the venue's own documentation before writing an
adapter, and both would otherwise have been found by an adapter that
stopped working.

### The families

| Family | Members | Shape |
|---|---|---|
| **Binance** | `binance` (built), **Aster** | `HMAC-SHA256` hex over the query string; `timestamp` + `recvWindow` |
| **OKX** | `okx` (built), **Bitget** | base64 of `HMAC-SHA256(timestamp + METHOD + path + body)`; a key/secret/**passphrase** triple |
| **Kraken** | **Kraken Futures** | `SHA-256(postData + nonce + path)`, then `HMAC-SHA-512` under the base64-decoded secret, then base64. Headers are `APIKey` and `Authent` |
| **Asymmetric** | **Backpack** (Ed25519), **Hyperliquid** (secp256k1 / EIP-712), **Lighter** (its own scheme, per-key nonce) | A keypair, not a shared secret |

Aster's documentation describes `HMAC SHA256` over "the query string
concatenated with the request body", with `timestamp` and a `recvWindow`
defaulting to 5000 — which is Binance's scheme, restated. Bitget signs
`timestamp + METHOD + requestPath + "?" + queryString + body` and
base64s it, under `ACCESS-KEY` / `ACCESS-SIGN` / `ACCESS-PASSPHRASE` —
which is OKX's scheme with different header names.

**So two of the seven land inside families that already exist here.**
They are the cheapest work on this list and they are the two that
demonstrate whether the existing abstractions generalise, which is worth
knowing before the expensive ones start.

### The structural differences

Three things separate venues in ways a signing family does not capture.

**1. What identifies an instrument.** Every venue built so far takes a
symbol string. Hyperliquid takes a **numeric asset index** — the
position in the `universe` array of its `meta` response, with spot
assets at `10000 + index`. Nothing in the current `Instrument` or in any
call signature carries that. An adapter must resolve symbol to index at
load and keep the mapping, and the mapping is not stable across venue
metadata updates.

**2. How many events arrive in one frame.** Binance's user stream sends
one event per message. OKX's `orders` channel sends a `data` **array**,
and Hyperliquid's order response returns a `statuses` array — one entry
per order in the batch. A reader that returns the first and drops the
rest loses a fill, and a lost fill is a position that never existed.

**3. Whether a client id survives the order.** `L4` in the live-path
design requires a client order id that is reconstructible, and both
built venues honour it: one client id, one venue id, for the life of the
order. **Hyperliquid's modify is a cancel-replace** — it cancels the
original (old `oid`), opens a replacement (new `oid`), and both carry
the **same `cloid`**. So the mapping is one-to-many over time, and a
deduplication table keyed on the client id sees two orders where the
strategy sees one. NautilusTrader's adapter handles this by suppressing
the stale cancel and promoting the replacement into a single
`OrderUpdated`; whatever this workspace does, it has to do something,
because the default behaviour is a book that believes an order was
cancelled while it is resting.

### Kraken Futures, read before writing

Three findings, and the first one has already been acted on.

**Order ids are names, not numbers.** `179f9af8-e45e-469d-b3e9-2fd4675cb7d0`.
`OrderAck::venue_id` and `OrderUpdate::venue_id` were `i64`, which was
true of the first two venues and false of this one — so the type was
carrying a constraint no caller needed, since `L4` makes `client_id` the
join key precisely so a venue's handle can be anything. Fixed ahead of
the adapter.

**A client order id is globally unique here, and that is a gift.**
`L4` records that Binance's `clientOrderId` is unique only among *open*
orders, so it is not an idempotency token — and all three surveyed
projects treated it as one. Kraken's is unique across the account's
history, up to 100 characters, and a repeat is refused by name
(`clientOrderIdAlreadyExist`). That makes a resend after an
[`Placed::Unknown`] answerable by the venue rather than by inference,
which is the single hardest case in the order path. The adapter should
use it; nothing else here can.

**Sizes and prices arrive as JSON numbers**, not as decimal strings.
Both other venues quote, and this crate keeps the venue's text precisely
because a float cannot hold what the text says. Here the text *is* a
float literal, so the raw field is still what gets kept — but nothing
downstream should be surprised to find `9392.749993345933` where the
other venues would have sent `"9392.75"`.

Rejections arrive inside HTTP 200, as `sendStatus.status` plus a
`REJECT` event — the third of four families to do it that way, and the
reason V8 below says the body is the rule.

### Where Kraken stops, and why it stops there

Its `Execution` side is built and passes the conformance suite. Its
`Account` side is not, and the reason is worth stating rather than
leaving as an absence.

The `/accounts` response for a flex account carries `availableMargin`,
`balanceValue` and `collateralValue`. `AccountSnapshot` carries a wallet
balance, an unrealized P&L and a margin balance. **Which maps to which
cannot be settled from the documented shapes** — the names are close
enough to guess at and different enough that a guess would be wrong
somewhere.

A wrong balance is worse than a missing one. This crate already records
why: a balance that could not be read was becoming `0.0`, "which is a
number a risk gate acts on", and the fix was to fail rather than to
invent. Inventing a *mapping* is the same mistake wearing better
clothes — it produces a number that is plausible, that no test here can
contradict, and that a position size is computed from.

So the account side waits for one real response from the venue. That is
a five-minute answer with a demo key and an unbounded one without.

### What other implementations paid for

NautilusTrader ships a Hyperliquid adapter written in Rust, and its
integration document is the most useful artefact this survey found —
it is a list of what the venue does that a reasonable implementer would
not expect:

- **An over-precise price fails as `user or API wallet does not exist`.**
  Prices are capped to five significant figures, and exceeding that
  makes signature verification fail, which the venue reports as a
  missing wallet. This is the same class as the defect Binance's first
  real run found here — a price with the right number of decimals that
  was not on the tick grid — except the error message points at the
  wrong thing entirely.
- **An agent wallet's orders belong to the master account**, and a
  client that does not set the master address explicitly gets empty
  holdings from REST, an empty user stream, and orders that never
  reconcile. Everything connects; nothing matches.
- **There are no market orders.** They are simulated with an IOC limit
  at a slippage-adjusted price, which requires a cached quote — so the
  order path depends on the data path, which is not true of any venue
  here today.
- **Rate limits are partly earned.** 10,000 actions plus one per
  cumulative USDC traded, with a separate cancel allowance. A budget
  that grows with volume is not the fixed weight-per-minute model
  `oq-live`'s rate limiter assumes.

CCXT's note on signing cost is worth recording too: a pure-language
ECDSA implementation signs in about 45 ms, against under 0.05 ms for a
native curve library. **A 45 ms signature sits inside the order path**,
and this workspace has a latency gate (`G6`) measured from journal write
to socket write. Whichever way the cryptography question below is
answered, the answer has a number attached to it.

## The cryptography question

`scripts/check-composability.sh` records the policy, and it is explicit:

> Signing and JSON reading are written out by hand rather than pulled
> in: this is the crate that holds the API secret, and every dependency
> here is one more thing trusted with it.

That policy is affordable for SHA-256 and HMAC, which are deterministic
bit operations with published test vectors — `oq-hash` passes RFC 4231.
It is not affordable for the asymmetric schemes:

- **Kraken** needs **SHA-512**, which `oq-hash` does not have. This one
  *is* affordable: it is the same shape of work as SHA-256, with the
  same kind of vectors to check against.
- **Backpack** needs **Ed25519**. **Hyperliquid** needs **secp256k1
  ECDSA plus Keccak-256** for EIP-712. **Lighter** needs its own
  scheme, over a curve, with a per-API-key nonce.

Hand-writing elliptic-curve arithmetic is not the same act as
hand-writing a hash. It is constant-time field arithmetic, point
multiplication, and — for ECDSA — nonce generation where a bias leaks
the key. These keys do not merely place orders: a wallet key moves
funds. **The safe options are to take an audited dependency or to not
build these venues**, and pretending there is a third is how a workspace
ends up with its own curve implementation.

The budget table says raising a budget is a deliberate act, to be made
in the commit that adds the dependency, saying what it buys. This is
that act, when it comes: one entry per curve, named, with the
alternative recorded as refused rather than unconsidered.

## Decisions

### V1 — Organise by signing family, not by CEX and DEX

The intuitive split is centralised against decentralised. It is the
wrong axis: **Aster is a perpetual DEX whose API is Binance's**, down to
`recvWindow` defaulting to 5000, while Backpack is a centralised
exchange that needs Ed25519. Sorting by how a request is signed and
what identifies an order puts the reusable work together and the
genuinely new work where it belongs.

### V2 — The two family members come first

Aster and Bitget are the cheapest and the most informative. Each drops
into a family that exists, so each one is a test of whether the
abstractions generalise or whether they encode one venue's habits. If
adding Aster requires editing `binance.rs`, the abstraction is wrong and
it is much better to learn that from a venue that costs days than from
one that costs weeks.

### V3 — Coinbase means Deribit, or it means nothing

An INTX adapter targets a switched-off service. Either the target
becomes Deribit's JSON-RPC gateway — a protocol family with no member
here yet, and a bigger piece of work than any REST venue on this list —
or Coinbase leaves the list. Recorded rather than silently dropped.

### V4 — A venue reader returns a list, not an option

`Events::read` currently answers `Option<UserEvent>`. OKX's `orders`
channel and Hyperliquid's `statuses` both carry several. The signature
becomes a list, and `UserStreamReader` holds the surplus in a queue that
`next` drains before reading the socket again. This is not a
Hyperliquid change — it is already required by the OKX work in flight.

### V5 — Resolving an instrument is the adapter's job, and it has state

Hyperliquid's numeric asset index has to come from somewhere, and that
somewhere is a `meta` response the adapter fetches and keeps. The
`Instrument` type stays free of it: what changes is that an adapter may
need to be *prepared* before it can trade a symbol, which today is only
true in the weak sense of reading a listing. Making preparation explicit
also gives Hyperliquid's five-significant-figure price cap a place to
live, so an over-precise price is refused here rather than returned as
a missing wallet.

### V6 — One client id may outlive its venue id

`L4` stands, but the invariant it implies — one client id, one venue id
— is Binance's and OKX's, not a property of venues. The order book
model needs a venue id that can change under a stable client id, and a
deduplication key that does not treat the replacement as a second order.
Whatever shape this takes, it is decided once, here, rather than
discovered separately in each adapter.

### V7 — Documented shapes are labelled as documented shapes

Every adapter's conformance payloads will come from the venues' public
documentation, because no account exists to capture real ones from.
`okx.rs` already carries the right disclosure and it is the model:

> It has not been run against OKX. Every pure function here is tested
> against payloads taken from the venue's documented shapes, and that is
> not the same as having placed an order. […] Assume this one has its
> own five.

Each new adapter carries the same paragraph, names its own venue, and
stays out of `Endpoint::Live` until someone has run it. A module that
cannot say this honestly is not ready to be merged.

### V8 — Rejection is read from the body, everywhere, by default

Binance says no with an HTTP status. OKX says no with HTTP 200 and a
code. **Hyperliquid also says no with HTTP 200**, inside
`statuses[].error`. Two of the four families already put the refusal in
the body, so the body is the rule and the status line is the exception —
`classify` is right and every new adapter implements it before anything
else, with a conformance case for exactly this.

## Per-venue notes, with confidence stated

| Venue | Family | Confidence | The thing most likely to bite |
|---|---|---|---|
| **Aster** | Binance | High — documentation read | Whether its perpetual semantics match Binance's as closely as its signing does |
| **Bitget** | OKX (signing only) | Medium — signing read, but the target moved | Mid-migration to UTA v3; and the family is the signature only — the success code is `"00000"` rather than `"0"`, and sizes are in **coins** where OKX counts contracts |
| **Kraken Futures** | Kraken | High — algorithm, order and position responses read | SHA-512 is in (`oq-hash`); symbols are `PF_XBTUSD`; sizes are JSON numbers; and its globally-unique client id is worth using rather than ignoring |
| **Backpack** | Ed25519 | Medium — signing scheme read | Parameters are sorted alphabetically before signing, which is a whole class of bug on its own |
| **Hyperliquid** | secp256k1 | Medium-high — both the venue's docs and a Rust adapter's notes read | Everything in "What other implementations paid for" |
| **Lighter** | Own scheme | **Low** | Per-API-key nonces, key indices 0–254 with 0–3 reserved, and a signing scheme this survey has not yet read properly |
| **Coinbase** | — | n/a | It moved. See V3 |

Lighter is stated as low deliberately. Writing it from memory would
produce something that compiles, passes its own tests, and is wrong in
ways nobody can see — which is worse than not having it.

## Order, and where it got to

**Done, and on `main`:**

1. **OKX, complete** — `Execution`, `Account` and `Events`, conformance
   passed, selectable as `--venue okx`. Writing the layer above it found
   the first of its predicted defects: sizes had been converted to
   coins where this venue and `Instrument` both count contracts.
2. **Aster, complete** — the family test, and it passed. The whole venue
   is a sixteen-entry path table, because its API *is* Binance's. A test
   asserts every Aster path is the Binance path with the version
   changed, so the day that stops being true, the table says so.
3. **`oq-hash` gains SHA-512** — checked against the NIST digests and
   RFC 4231, and the commit that added it states where hand-writing a
   primitive stops.
4. **Kraken Futures, execution side** — signing, placement, cancel,
   status, conformance passed. It found two defects in code that was
   already shipping: `raw_field` stopped at the first occurrence of a
   key, so `{"result":"error","error":"..."}` turned a named refusal
   into an unexplained one; and the conformance suite caught this
   adapter reporting a captive portal as a *rejection*, which invites a
   resend into an order that may be resting.
5. **Bitget on UTA v3, execution side** — conformance passed, with two
   of its three paths marked as guesses in the module header.

**Blocked, each on something a person has to supply:**

6. **Kraken and Bitget account sides** — one real response each. The
   flex `/accounts` shape cannot be mapped onto `AccountSnapshot` from
   the documentation, and a wrong balance is worse than a missing one.
7. **The dependency decision** — Backpack (Ed25519), Hyperliquid
   (secp256k1 and Keccak) and Lighter all need asymmetric signing, and
   the composability budget's note is explicit about what a dependency
   in this crate costs. Nothing past here moves until that is decided
   and the budget table edited with the reason.
8. **Backpack**, then **Hyperliquid**, then **Lighter** — in that order,
   simplest scheme first, so the decision in (7) is proved by the
   cheapest of the three. Lighter still needs a survey of its own; it is
   the one venue here whose signing this document has not read properly.
9. **Deribit**, or Coinbase leaves the list.

Each step is a pull request, and steps 2 through 7 each begin by writing
the conformance payloads and end with the adapter passing them.

## What this does not cover

Market data. Every venue here also publishes a book and a trade stream,
and `oq-l2feed` has its own venue abstraction with its own conformance
suite. The two sides are deliberately separate — an execution adapter
and a capture adapter for one venue are different objects — and adding
seven venues to the capture side is a second document.
