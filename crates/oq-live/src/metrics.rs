//! Numbers a monitoring system can read, and the conditions worth
//! waking somebody for.
//!
//! M3's scope asks for structured metrics and alert integration hooks
//! beside the latency histograms. The histograms existed and everything
//! else a live run knew was in `println!`, which is readable by a person
//! watching a terminal and by nothing else.
//!
//! # Why this is a snapshot rather than a client
//!
//! No HTTP server, no push, no third-party format. A crate that carried
//! a metrics client would carry its dependency tree into every process
//! that links it, and `oq-live`'s budget defends what a consumer pulls
//! in. So this produces a value and renders it in a line-oriented text
//! form; whatever scrapes it is the operator's choice and lives outside.
//!
//! The rendering is Prometheus-shaped because that is what most things
//! read, and it is a format rather than a dependency: seven lines of
//! `write!`.
//!
//! # Alerts are conditions, not notifications
//!
//! `FR-RISK-3`'s kill switch and the supervisor already act. What was
//! missing is the layer that says *this is worth waking somebody for*,
//! separated from the layer that decides what to do about it —
//! [`Alert`] is a judgement about a snapshot and sends nothing. A hook
//! that sent would put a network call on the path that notices a venue
//! is failing, which is the path least able to afford one.

use core::fmt::Write as _;

use crate::latency::Latency;

/// What a live run knows about itself at one instant.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// Observations folded.
    pub ticks: u64,
    /// Orders sent.
    pub sent: u64,
    /// Fills booked.
    pub fills: u64,
    /// Fills the venue reported twice.
    ///
    /// Counted rather than merely handled: a stream repeating itself is
    /// routine, and the *trend* is what says whether the link is getting
    /// worse.
    pub duplicate_fills: u64,
    /// Reports carrying no trade id, which cannot be deduplicated and
    /// are therefore not booked.
    pub unidentifiable_fills: u64,
    /// Reports this build could not read at all, and did not book.
    ///
    /// A price or quantity that is absent, unparseable, or not positive.
    /// Distinct from [`Snapshot::unidentifiable_fills`], which is about a
    /// missing trade id: the consequence is the same but the cause is
    /// not, and a run that sees one of these is looking at a venue whose
    /// reports this build does not understand.
    pub unbookable_reports: u64,
    /// Orders resting at the venue that this process did not send.
    pub foreign_orders: u64,
    /// Times the account stream dropped.
    pub disconnects: u64,
    /// Times a read of the account came back incomplete.
    pub incomplete_reads: u64,
    /// Times this process's books disagreed with the venue.
    pub reconciliation_mismatches: u64,
    /// Whether the kill switch is down.
    pub halted: bool,
}

impl Snapshot {
    /// Render in the line-oriented form most collectors read.
    ///
    /// A format, not a dependency. Every metric carries a `# HELP` line
    /// because a counter named `oq_unidentifiable_fills_total` means
    /// nothing to the person who finds it at 3 a.m., and a dashboard
    /// built on a metric nobody understands is a dashboard nobody acts
    /// on.
    #[must_use]
    pub fn render(&self, latency: Option<&Latency>) -> String {
        let mut out = String::new();
        let mut counter = |name: &str, help: &str, value: u64| {
            let _ = writeln!(out, "# HELP {name} {help}");
            let _ = writeln!(out, "# TYPE {name} counter");
            let _ = writeln!(out, "{name} {value}");
        };

        counter(
            "oq_ticks_total",
            "observations folded into the engine; a flat line here means the feed \
             stopped, which looks the same as a quiet market",
            self.ticks,
        );
        counter(
            "oq_orders_sent_total",
            "orders this process sent, whatever the venue then did with them",
            self.sent,
        );
        counter(
            "oq_fills_total",
            "fills booked into this process's own account state, after deduplication",
            self.fills,
        );
        counter(
            "oq_duplicate_fills_total",
            "fills the venue reported more than once; routine after a reconnect, and \
             the trend says whether the link is getting worse",
            self.duplicate_fills,
        );
        counter(
            "oq_unidentifiable_fills_total",
            "fill reports with no trade id; not booked, because a fill that cannot be \
             deduplicated could arrive any number of times",
            self.unidentifiable_fills,
        );
        counter(
            "oq_foreign_orders",
            "orders resting at the venue that this process did not send",
            self.foreign_orders,
        );
        counter(
            "oq_stream_disconnects_total",
            "times the account stream dropped",
            self.disconnects,
        );
        counter(
            "oq_incomplete_reads_total",
            "account reads that could not be completed; nothing was compared, which is \
             not the same as nothing having changed",
            self.incomplete_reads,
        );
        counter(
            "oq_reconciliation_mismatches_total",
            "times this process's books disagreed with the venue",
            self.reconciliation_mismatches,
        );

        let _ = writeln!(out, "# HELP oq_halted whether the kill switch is down");
        let _ = writeln!(out, "# TYPE oq_halted gauge");
        let _ = writeln!(out, "oq_halted {}", u8::from(self.halted));

        if let Some(l) = latency {
            for q in [0.5, 0.99] {
                if let Some(v) = l.quantile(q) {
                    let _ = writeln!(
                        out,
                        "# HELP oq_decision_latency_ns in-process latency, journal flush to \
                         client call — not the venue round trip"
                    );
                    let _ = writeln!(out, "# TYPE oq_decision_latency_ns gauge");
                    let _ = writeln!(out, "oq_decision_latency_ns{{quantile=\"{q}\"}} {v}");
                }
            }
        }
        out
    }
}

/// Something worth waking somebody for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    /// Short name, for routing.
    pub name: &'static str,
    /// What is wrong, in terms the person woken can act on.
    pub detail: String,
    /// Whether this needs a person now.
    pub urgent: bool,
}

/// When to raise each alert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlertRules {
    /// Foreign orders past which the risk gate's counts are unreliable.
    ///
    /// One is enough. A single order this process did not send consumes
    /// its limit, and the failure it caused on a real venue was a risk
    /// gate that stopped being able to fire.
    pub max_foreign_orders: u64,
    /// Reconciliation mismatches past which the books cannot be trusted.
    pub max_mismatches: u64,
    /// Incomplete reads past which the account is effectively unwatched.
    pub max_incomplete_reads: u64,
}

impl Default for AlertRules {
    fn default() -> Self {
        Self {
            max_foreign_orders: 0,
            max_mismatches: 0,
            max_incomplete_reads: 20,
        }
    }
}

/// Everything a snapshot says is worth waking somebody for.
///
/// A judgement, not a notification. Sending is the operator's, and a
/// hook that sent would put a network call on the path that notices a
/// venue is failing — the path least able to afford one.
#[must_use]
pub fn alerts(s: &Snapshot, rules: AlertRules) -> Vec<Alert> {
    let mut out = Vec::new();

    if s.halted {
        out.push(Alert {
            name: "halted",
            detail: "the kill switch is down and stays down until a person clears it".to_string(),
            urgent: true,
        });
    }
    if s.reconciliation_mismatches > rules.max_mismatches {
        out.push(Alert {
            name: "books_disagree",
            detail: format!(
                "{} reconciliation mismatch(es): this process and the venue disagree \
                 about the position, and every decision from here is computed from a \
                 different base",
                s.reconciliation_mismatches
            ),
            urgent: true,
        });
    }
    if s.foreign_orders > rules.max_foreign_orders {
        out.push(Alert {
            name: "foreign_orders",
            detail: format!(
                "{} order(s) resting that this process did not send; they consume the \
                 risk gate's limit, so its caps may no longer be able to fire",
                s.foreign_orders
            ),
            urgent: true,
        });
    }
    if s.unbookable_reports > 0 {
        out.push(Alert {
            name: "unbookable_reports",
            detail: format!(
                "{} fill report(s) could not be read and were not booked; the position this \
                 process believes it holds is smaller than the account's",
                s.unbookable_reports
            ),
            urgent: true,
        });
    }
    if s.unidentifiable_fills > 0 {
        out.push(Alert {
            name: "unbookable_fills",
            detail: format!(
                "{} fill report(s) with no trade id were not booked; the position this \
                 process believes it holds is smaller than the account's",
                s.unidentifiable_fills
            ),
            urgent: true,
        });
    }
    if s.incomplete_reads > rules.max_incomplete_reads {
        out.push(Alert {
            name: "account_unwatched",
            detail: format!(
                "{} incomplete read(s): the account has stretches nobody compared, and \
                 that is not the same as stretches where nothing changed",
                s.incomplete_reads
            ),
            // Not urgent on its own. Measured against a real venue, a
            // rate of one in five was the venue's own backend failing
            // for a few hours — worth knowing, not worth waking anyone.
            urgent: false,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A quiet run raises nothing. An alert layer that fires on a
    /// healthy process is one that gets muted, and a muted alert is
    /// worse than none because it looks like coverage.
    #[test]
    fn a_healthy_run_raises_nothing() {
        let s = Snapshot {
            ticks: 10_000,
            sent: 12,
            fills: 12,
            ..Snapshot::default()
        };
        assert_eq!(alerts(&s, AlertRules::default()), Vec::new());
    }

    /// One foreign order is enough. It consumes the risk gate's limit,
    /// and the failure this comes from was a gate that stopped being
    /// able to fire.
    #[test]
    fn a_single_foreign_order_is_worth_waking_someone_for() {
        let s = Snapshot {
            foreign_orders: 1,
            ..Snapshot::default()
        };
        let a = alerts(&s, AlertRules::default());
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].name, "foreign_orders");
        assert!(a[0].urgent);
        assert!(a[0].detail.contains("caps may no longer be able to fire"));
    }

    /// A fill that could not be booked means the position is understated,
    /// which is the direction that gets discovered by a liquidation.
    #[test]
    fn an_unbookable_fill_is_urgent() {
        let s = Snapshot {
            unidentifiable_fills: 1,
            ..Snapshot::default()
        };
        let a = alerts(&s, AlertRules::default());
        assert!(a.iter().any(|x| x.name == "unbookable_fills" && x.urgent));
    }

    /// Duplicates are counted and do not alert. A reconnecting stream
    /// repeating itself is routine; the books deduplicate it, and waking
    /// somebody for routine behaviour is how an alert stops being read.
    #[test]
    fn duplicates_are_counted_but_do_not_wake_anyone() {
        let s = Snapshot {
            duplicate_fills: 500,
            ..Snapshot::default()
        };
        assert_eq!(alerts(&s, AlertRules::default()), Vec::new());
        assert!(s.render(None).contains("oq_duplicate_fills_total 500"));
    }

    /// Incomplete reads matter and are not urgent. Measured against a
    /// real venue, one in five was the venue's own backend failing for a
    /// few hours.
    #[test]
    fn incomplete_reads_are_reported_without_waking_anyone() {
        let s = Snapshot {
            incomplete_reads: 500,
            ..Snapshot::default()
        };
        let a = alerts(&s, AlertRules::default());
        assert_eq!(a.len(), 1);
        assert!(!a[0].urgent);
        assert!(
            a[0].detail
                .contains("not the same as stretches where nothing changed")
        );
    }

    /// Every metric carries help text. A counter nobody can interpret at
    /// 3 a.m. is a dashboard nobody acts on.
    #[test]
    fn every_metric_says_what_it_means() {
        let text = Snapshot::default().render(None);
        let names: Vec<&str> = text
            .lines()
            .filter(|l| l.starts_with("# HELP "))
            .map(|l| l.trim_start_matches("# HELP "))
            .collect();
        assert!(names.len() >= 10, "{names:?}");
        for n in names {
            let (_, help) = n.split_once(' ').expect("help text");
            assert!(help.len() > 12, "{n:?} has no useful help");
        }
    }

    /// The latency line says which boundary it measured. G6's far
    /// boundary is not observable, and a metric named for the round trip
    /// would be claiming a number nobody has.
    #[test]
    fn the_latency_metric_names_the_boundary_it_measured() {
        let mut l = Latency::new();
        for ns in [100, 200, 300, 400] {
            l.record(ns);
        }
        let text = Snapshot::default().render(Some(&l));
        assert!(text.contains("oq_decision_latency_ns"));
        assert!(
            text.contains("not the venue round trip"),
            "the boundary must be named: {text}"
        );
    }
}

#[cfg(test)]
mod unbookable {
    use super::{Snapshot, alerts};

    /// A report the venue sent and this build could not read is urgent.
    ///
    /// The position this process believes it holds is then smaller than
    /// the account's, and every order it sizes afterwards is sized
    /// against a picture that is wrong. Before this counter existed the
    /// report was discarded with no line, no count and no record — a
    /// fill that happened and left no trace anywhere in this process.
    #[test]
    fn an_unreadable_report_is_urgent_and_says_what_it_costs() {
        let s = Snapshot {
            unbookable_reports: 2,
            ..Snapshot::default()
        };
        let a = alerts(&s, super::AlertRules::default());
        let it = a
            .iter()
            .find(|x| x.name == "unbookable_reports")
            .expect("an unreadable report must be reported");
        assert!(it.urgent);
        assert!(
            it.detail.contains("smaller than the account's"),
            "the alert has to say what it costs: {}",
            it.detail
        );
    }

    /// Distinct from a missing trade id: same consequence, different
    /// cause, and a run that sees one is looking at a venue whose
    /// reports this build does not understand.
    #[test]
    fn it_is_not_the_same_alert_as_a_missing_trade_id() {
        let s = Snapshot {
            unbookable_reports: 1,
            unidentifiable_fills: 1,
            ..Snapshot::default()
        };
        let a = alerts(&s, super::AlertRules::default());
        assert!(a.iter().any(|x| x.name == "unbookable_reports"));
        assert!(a.iter().any(|x| x.name == "unbookable_fills"));
    }

    #[test]
    fn none_is_silent() {
        let a = alerts(&Snapshot::default(), super::AlertRules::default());
        assert!(!a.iter().any(|x| x.name == "unbookable_reports"));
    }
}
