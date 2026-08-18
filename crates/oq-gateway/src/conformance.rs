//! What any execution adapter must mean, expressed as something that
//! runs.
//!
//! `FR-VENUE-2` asks for a conformance suite exercising every adapter
//! against recorded and synthetic venue behaviour, including lost,
//! duplicated and out-of-order reports. One exists for market data, in
//! `oq-l2feed`. This is the half that was missing, and it is the half
//! that decides whether a second venue can be trusted with money.
//!
//! # The [`Execution`] trait says what an adapter provides, not what it means
//!
//! Two adapters can both compile and disagree about whether a timeout is
//! a rejection, whether an HTTP 200 is an acceptance, whether a client
//! id survives a response that omits it. Those disagreements do not
//! fail. They produce a caller that books a position the venue never
//! took — which is the single worst outcome available here, and the
//! reason the placement contract has three outcomes rather than two.
//!
//! # The payloads come from the adapter
//!
//! A suite carrying its own fixtures would be a suite testing one
//! venue's wire format. Every venue's bytes differ — that is the whole
//! reason the seam exists — so each adapter brings a recorded response
//! of each kind and states what it means. The suite checks the adapter
//! against its own stated meaning, which is the only thing an outside
//! test can honestly check.
//!
//! Binance answers a refusal with an HTTP status; OKX answers one with
//! HTTP 200 and a body carrying two codes. Both are conforming. An
//! adapter that read the status alone would pass a suite written around
//! Binance and lose money on OKX, which is precisely why the suite asks
//! each adapter what its own bytes mean.
//!
//! # What this deliberately does not cover
//!
//! Anything requiring a network or credentials. A suite that needed
//! either is a suite nobody runs on a laptop, and one nobody runs is one
//! that stops being true. Live behaviour — signatures, precision,
//! subscription — is what `oq-order-check` is for, and it is named in
//! the report rather than silently substituted for.

use crate::exec::{Placed, Reject};

/// Recorded responses from one venue, with what each one means.
///
/// Every field is a real payload the venue sent, not one written by
/// hand. A fixture invented to satisfy a suite tests the invention.
pub struct Responses {
    /// The venue this describes.
    pub venue: &'static str,
    /// The client id used in every payload below.
    pub client_id: &'static str,
    /// A response accepting an order.
    pub accepted: &'static str,
    /// The venue's own id in that response.
    pub accepted_venue_id: i64,
    /// A response refusing an order, and the HTTP status it arrived
    /// with.
    ///
    /// The status is part of the sample because it differs by venue and
    /// is the thing an adapter is most likely to trust wrongly.
    pub rejected: (u16, &'static str),
    /// The venue's error code in that refusal, when it gave one.
    pub rejected_code: Option<i64>,
    /// A response saying the venue could not answer, with its status.
    pub unavailable: (u16, &'static str),
    /// A response to a status query for an order that does not exist.
    pub absent: &'static str,
    /// A response to a status query for an order that does.
    pub present: &'static str,
    /// Something that is not this venue's answer at all.
    pub foreign: &'static str,
}

/// How an adapter classifies a response.
///
/// The one function every execution adapter must have and the trait
/// cannot express: turning bytes into one of three outcomes. Supplied
/// per adapter because it is the adapter, and the suite exists to check
/// it rather than to provide it.
pub type Classify = fn(u16, &str, &str) -> Placed;

/// How an adapter reads a status query.
pub type ReadStatus = fn(&str, &str) -> Option<crate::exec::OrderAck>;

/// What the suite found.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Report {
    /// Checks run.
    pub checks: usize,
    /// Each failure, in the adapter's own terms.
    pub failures: Vec<String>,
}

impl Report {
    /// Whether the adapter conforms.
    #[must_use]
    pub fn conforms(&self) -> bool {
        self.failures.is_empty()
    }

    /// One line, for a report a person reads.
    #[must_use]
    pub fn summary_line(&self, venue: &str) -> String {
        if self.conforms() {
            format!("{venue}: {} checks, conforms", self.checks)
        } else {
            format!(
                "{venue}: {} checks, {} failure(s): {}",
                self.checks,
                self.failures.len(),
                self.failures.join("; ")
            )
        }
    }
}

/// Drive an adapter through the contract.
#[must_use]
pub fn check(r: &Responses, classify: Classify, status: ReadStatus) -> Report {
    let mut out = Report::default();
    let mut fail = |why: String| out.failures.push(why);

    // 1. An acceptance is an acceptance, and carries the id the caller
    //    chose. That id is the only handle that survives a request whose
    //    answer never came back, so an adapter that dropped it would
    //    leave the caller unable to ask about the order it just sent.
    out.checks += 1;
    match classify(200, r.accepted, r.client_id) {
        Placed::Accepted(a) => {
            if a.client_id != r.client_id {
                fail(format!(
                    "an acceptance came back with client id {:?}, not the {:?} that was sent",
                    a.client_id, r.client_id
                ));
            }
            if a.venue_id != r.accepted_venue_id {
                fail(format!(
                    "an acceptance reported venue id {} rather than {}",
                    a.venue_id, r.accepted_venue_id
                ));
            }
        }
        other => fail(format!("an accepted order classified as {other:?}")),
    }

    // 2. A refusal is final, and must not be reported as unknown. An
    //    unknown invites a caller to resolve it, and resolving a
    //    refusal costs a request per order forever.
    out.checks += 1;
    let (status_code, body) = r.rejected;
    match classify(status_code, body, r.client_id) {
        Placed::Rejected(Reject { code, message }) => {
            if code != r.rejected_code {
                fail(format!(
                    "a refusal reported code {code:?} rather than {:?}",
                    r.rejected_code
                ));
            }
            if message.trim().is_empty() {
                fail(
                    "a refusal carried no message; the reason is the half a caller acts on"
                        .to_string(),
                );
            }
        }
        other => fail(format!(
            "a refusal at HTTP {status_code} classified as {other:?} — the venue said no"
        )),
    }

    // 3. **The one that matters most.** A venue that could not answer
    //    leaves the order unknown, never rejected. Folding it into a
    //    rejection is what produces duplicate positions: the caller
    //    believes nothing landed and sends it again.
    out.checks += 1;
    let (status_code, body) = r.unavailable;
    match classify(status_code, body, r.client_id) {
        Placed::Unknown(u) => {
            if u.client_id != r.client_id {
                fail(format!(
                    "an unresolved placement lost its client id ({:?}), which is the only \
                     handle left to ask about it",
                    u.client_id
                ));
            }
        }
        Placed::Rejected(_) => fail(
            "a venue that could not answer was classified as a refusal — a caller that \
             believes nothing landed will send it again, and that is how a position doubles"
                .to_string(),
        ),
        other => fail(format!("an unanswerable request classified as {other:?}")),
    }

    // 4. A response this adapter does not recognise says nothing about
    //    the order, so it is unknown rather than either answer.
    out.checks += 1;
    match classify(200, r.foreign, r.client_id) {
        Placed::Unknown(_) => {}
        other => fail(format!(
            "an unrecognised response classified as {other:?}; nothing can be concluded \
             from bytes this adapter cannot read"
        )),
    }

    // 5. "No such order" is the answer that licenses a resend after an
    //    unknown, so it has to be distinguishable from every other
    //    answer.
    out.checks += 1;
    if status(r.absent, r.client_id).is_some() {
        fail(
            "a status query for an order the venue does not have returned one; after an \
             unknown placement that is the answer that says it is safe to send again"
                .to_string(),
        );
    }

    // 6. And an order that exists comes back with its state, unmapped.
    out.checks += 1;
    match status(r.present, r.client_id) {
        None => fail("a status query for an existing order returned nothing".to_string()),
        Some(a) => {
            if a.status.trim().is_empty() {
                fail(
                    "an existing order came back with no status; a state this build has \
                     never heard of must surface as itself rather than as the nearest \
                     known one"
                        .to_string(),
                );
            }
        }
    }

    out
}
