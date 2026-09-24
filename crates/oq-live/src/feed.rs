//! Market data, in memory rather than on disk.
//!
//! The capture path writes what it receives and hands nothing back;
//! this hands everything back and writes nothing. They share the
//! connector, the venue's stream definitions and the parsers, and they
//! differ only in what happens to the bytes — which is the difference
//! that should exist between capturing a market and trading in one.
//!
//! # Two sockets, one thread
//!
//! Depth and trades arrive on separate connections, and so does the
//! account's own stream. All three are read with short timeouts and
//! polled in turn rather than blocked on, because a thread parked on
//! one socket is a thread that cannot renew the key on another. The
//! cost is a little latency per poll; the alternative is a process that
//! stops trading because it was waiting politely.

use core::time::Duration;

/// Silence beyond which a market data connection is presumed dead.
///
/// Depth updates arrive several times a second and trades on a liquid
/// contract are not far behind, so half a minute without either is not
/// a quiet market. Chosen well above the ordinary gap and well below the
/// windows that were being lost.
const DEFAULT_STALE_AFTER: Duration = Duration::from_secs(30);
use std::io;

use oq_l2feed::session::{Connector, MessageSource};
use oq_l2feed::venue::{Deployment, Venue};
use oq_l2feed::ws::WsConnector;

/// A connector of any kind, behind one type.
///
/// The live loop reads market data from a websocket; a simulation of the
/// whole process reads it from a scripted feed. Both are connectors, and
/// holding either behind this lets the loop be the same code for both.
pub struct AnyConnector(Box<dyn FnMut() -> io::Result<Box<dyn MessageSource>>>);

impl AnyConnector {
    pub fn new<C>(mut connector: C) -> Self
    where
        C: Connector + 'static,
        C::Source: 'static,
    {
        Self(Box::new(move || {
            connector
                .connect()
                .map(|s| Box::new(s) as Box<dyn MessageSource>)
        }))
    }
}

impl Connector for AnyConnector {
    type Source = Box<dyn MessageSource>;

    fn connect(&mut self) -> io::Result<Self::Source> {
        (self.0)()
    }
}

/// One market data stream, reconnecting as needed.
pub struct Stream<C: Connector = AnyConnector> {
    name: &'static str,
    connector: C,
    source: Option<C::Source>,
    /// Connections opened since start. A rising count is the symptom
    /// worth seeing; any single reconnection is unremarkable.
    reconnects: u64,
    /// When this stream last delivered anything, on the caller's clock.
    last_message: Duration,
    /// Silence beyond which the connection is presumed dead.
    stale_after: Duration,
    /// Connections dropped for going quiet rather than for failing.
    stalls: u64,
}

impl Stream<AnyConnector> {
    /// Open the venue's stream named `name` for `symbol`.
    ///
    /// # Errors
    /// When the venue does not publish a stream by that name.
    pub fn open(
        venue: &dyn Venue,
        symbol: &str,
        name: &'static str,
        read_timeout: Duration,
    ) -> Result<Self, String> {
        let spec = venue
            .streams(symbol)
            .into_iter()
            .find(|s| s.name == name)
            .ok_or_else(|| format!("this venue publishes no stream called {name:?}"))?;
        Ok(Self {
            name,
            connector: AnyConnector::new(WsConnector::new(venue.transport(&spec), read_timeout)),
            source: None,
            reconnects: 0,
            last_message: Duration::ZERO,
            stale_after: DEFAULT_STALE_AFTER,
            stalls: 0,
        })
    }
}

impl<C: Connector> Stream<C> {
    /// A stream over any connector. For tests and for venues whose
    /// transport is not a websocket.
    pub fn over(name: &'static str, connector: C, stale_after: Duration) -> Self {
        Self {
            name,
            connector,
            source: None,
            reconnects: 0,
            last_message: Duration::ZERO,
            stale_after,
            stalls: 0,
        }
    }

    /// How long this stream may say nothing before it is presumed dead.
    #[must_use]
    pub fn stale_after(mut self, after: Duration) -> Self {
        self.stale_after = after;
        self
    }

    /// Connections dropped for silence rather than for an error.
    #[must_use]
    pub const fn stalls(&self) -> u64 {
        self.stalls
    }

    #[must_use]
    pub const fn name(&self) -> &'static str {
        self.name
    }

    #[must_use]
    pub const fn connections(&self) -> u64 {
        self.reconnects
    }

    /// Read one message, connecting or reconnecting if necessary.
    ///
    /// `Ok(None)` means nothing arrived within the timeout, which on a
    /// market data stream is ordinary. An error means the connection is
    /// gone and the next call will try to open a new one.
    ///
    /// `now` is the caller's monotonic clock — `Clock::elapsed` — so the
    /// silence that retires a connection is measured on the same clock
    /// as everything else the loop decides by.
    pub fn poll(&mut self, now: Duration) -> io::Result<Option<Vec<u8>>> {
        // Silence is not the same as nothing to say.
        //
        // A read that times out returns `Ok(None)` below, and that is
        // ordinary — for a moment. A half-open socket answers every read
        // exactly that way and answers it forever, so a stream that has
        // stopped delivering looks identical to a quiet market and is
        // never reconnected. The error path already knows about this
        // socket; the silent path did not.
        //
        // Measured, on a six-hour run: eleven windows with no market
        // data at all, ninety-five minutes in total, the longest
        // fourteen and a half. The venue's own records show more than a
        // thousand trades inside that window, so the market was not
        // quiet — this process was blind, and only stopped being blind
        // when a reset finally surfaced as a real error.
        let silent = now.saturating_sub(self.last_message);
        if self.source.is_some() && silent > self.stale_after {
            self.source = None;
            self.stalls += 1;
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("silent for {}s; presumed dead", silent.as_secs()),
            ));
        }
        if self.source.is_none() {
            self.source = Some(self.connector.connect()?);
            // Counted on every open, the first included, so the number
            // reads as connections made rather than as a fault count
            // that starts at minus one.
            self.reconnects += 1;
            // A fresh connection has not been silent; without this the
            // staleness it inherited would drop it again at once.
            self.last_message = now;
        }
        let source = self.source.as_mut().expect("just connected");
        match source.next_message() {
            Ok(bytes) => {
                self.last_message = now;
                Ok(Some(bytes))
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                Ok(None)
            }
            Err(e) => {
                // Dropped rather than retried in place: a half-open
                // socket answers every read the same way, and retrying
                // on it is a loop that never notices.
                self.source = None;
                Err(e)
            }
        }
    }
}

/// Depth and trades for one contract on one deployment.
pub struct MarketData {
    depth: Stream,
    trade: Stream,
}

impl MarketData {
    /// # Errors
    /// When the venue is unknown on that deployment, or publishes
    /// neither stream.
    pub fn open(
        venue_id: &str,
        deployment: Deployment,
        symbol: &str,
        read_timeout: Duration,
    ) -> Result<(Self, Box<dyn Venue>), String> {
        let venue = oq_l2feed::venue::by_id_at(venue_id, deployment).ok_or_else(|| {
            format!(
                "no adapter for {venue_id:?} on {deployment:?}; refusing rather than \
                 falling back to another deployment"
            )
        })?;
        // The coarser book where the venue publishes one.
        //
        // This host folds depth into fixed windows, so resolution finer
        // than a window is discarded the moment it arrives. Taking it
        // anyway made this process a slow consumer of its own feed: the
        // venue sends on every change, one message was taken per loop
        // pass, and the rest queued until the venue dropped the
        // connection for not keeping up.
        //
        // Falls back to the full-resolution stream on a venue that
        // publishes no coalesced one, which is the right failure: less
        // efficient, never wrong.
        let depth_stream = if venue.streams(symbol).iter().any(|s| s.name == "depth100") {
            "depth100"
        } else {
            "depth"
        };
        let md = Self {
            depth: Stream::open(venue.as_ref(), symbol, depth_stream, read_timeout)?,
            trade: Stream::open(venue.as_ref(), symbol, "trade", read_timeout)?,
        };
        Ok((md, venue))
    }

    /// Market data from streams the caller built — a simulated feed, or
    /// any connector that is not the venue's websocket.
    #[must_use]
    pub const fn from_streams(depth: Stream, trade: Stream) -> Self {
        Self { depth, trade }
    }

    pub fn depth(&mut self) -> &mut Stream {
        &mut self.depth
    }

    pub fn trade(&mut self) -> &mut Stream {
        &mut self.trade
    }
}

#[cfg(test)]
mod liveness {
    use super::{Duration, Stream};
    use oq_l2feed::session::{Connector, MessageSource};
    use std::io;

    /// A source that answers every read the way a half-open socket does:
    /// with a timeout, forever.
    struct Silent;

    impl MessageSource for Silent {
        fn next_message(&mut self) -> io::Result<Vec<u8>> {
            Err(io::Error::new(io::ErrorKind::WouldBlock, "nothing yet"))
        }
    }

    /// Counts how many times it was asked for a connection.
    struct Counting(u32);

    impl Connector for Counting {
        type Source = Silent;
        fn connect(&mut self) -> io::Result<Self::Source> {
            self.0 += 1;
            Ok(Silent)
        }
    }

    /// A stream that says nothing is reconnected, not waited on.
    ///
    /// A read that times out is ordinary for a moment and identical to a
    /// dead connection forever. The error path already dropped the
    /// socket rather than retrying on it — "a half-open socket answers
    /// every read the same way" — and the silent path did not, so a
    /// stream that had stopped delivering was never reconnected until a
    /// reset finally surfaced as a real error.
    ///
    /// Six hours of that cost ninety-five minutes of market data across
    /// eleven windows, the longest fourteen and a half, while the venue's
    /// own records show the market trading throughout.
    #[test]
    fn a_stream_that_goes_quiet_is_reconnected() {
        // Time is handed in, so the test moves it rather than waiting.
        let mut s = Stream::over("depth", Counting(0), Duration::from_secs(30));

        // The first poll connects and hears nothing, which is fine, and
        // so is hearing nothing for less than the limit.
        assert!(s.poll(Duration::ZERO).expect("connects").is_none());
        assert!(
            s.poll(Duration::from_secs(30))
                .expect("still within")
                .is_none()
        );
        assert_eq!(s.connections(), 1);
        assert_eq!(s.stalls(), 0);

        // The second finds it has been silent past the limit and drops
        // it, reporting why rather than reconnecting behind the reader's
        // back.
        let err = s
            .poll(Duration::from_secs(31))
            .expect_err("silence past the limit is an error");
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        assert!(
            err.to_string().contains("presumed dead"),
            "the reason has to reach the log: {err}"
        );
        assert_eq!(s.stalls(), 1);

        // And the next one opens a new connection.
        let _ = s.poll(Duration::from_secs(31));
        assert_eq!(s.connections(), 2, "a dead stream was never replaced");
    }

    /// A stream that is delivering is left alone.
    #[test]
    fn a_stream_that_speaks_is_not_dropped() {
        struct Talking;
        impl MessageSource for Talking {
            fn next_message(&mut self) -> io::Result<Vec<u8>> {
                Ok(b"{}".to_vec())
            }
        }
        struct Always(u32);
        impl Connector for Always {
            type Source = Talking;
            fn connect(&mut self) -> io::Result<Self::Source> {
                self.0 += 1;
                Ok(Talking)
            }
        }

        let mut s = Stream::over("trade", Always(0), Duration::from_secs(60));
        for minute in 0..10 {
            let now = Duration::from_secs(minute * 60);
            assert!(s.poll(now).expect("delivers").is_some());
        }
        assert_eq!(s.connections(), 1, "a live stream was reconnected");
        assert_eq!(s.stalls(), 0);
    }
}
