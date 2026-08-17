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
use std::io;

use oq_l2feed::depth::Scales;
use oq_l2feed::session::{Connector, MessageSource};
use oq_l2feed::venue::{Deployment, Venue};
use oq_l2feed::ws::WsConnector;

/// One market data stream, reconnecting as needed.
pub struct Stream {
    name: &'static str,
    connector: WsConnector,
    source: Option<<WsConnector as Connector>::Source>,
    /// Connections opened since start. A rising count is the symptom
    /// worth seeing; any single reconnection is unremarkable.
    reconnects: u64,
}

impl Stream {
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
            connector: WsConnector::new(venue.transport(&spec), read_timeout),
            source: None,
            reconnects: 0,
        })
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
    pub fn poll(&mut self) -> io::Result<Option<Vec<u8>>> {
        if self.source.is_none() {
            self.source = Some(self.connector.connect()?);
            // Counted on every open, the first included, so the number
            // reads as connections made rather than as a fault count
            // that starts at minus one.
            self.reconnects += 1;
        }
        let source = self.source.as_mut().expect("just connected");
        match source.next_message() {
            Ok(bytes) => Ok(Some(bytes)),
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
    scales: Scales,
}

impl MarketData {
    /// # Errors
    /// When the venue is unknown on that deployment, or publishes
    /// neither stream, or does not list the contract.
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
        let instrument = venue
            .instrument(symbol)
            .ok_or_else(|| format!("{venue_id} does not list {symbol}"))?;
        let scales = Scales {
            price: u32::from(instrument.price_scale),
            qty: u32::from(instrument.qty_scale),
        };
        let md = Self {
            depth: Stream::open(venue.as_ref(), symbol, "depth", read_timeout)?,
            trade: Stream::open(venue.as_ref(), symbol, "trade", read_timeout)?,
            scales,
        };
        Ok((md, venue))
    }

    #[must_use]
    pub const fn scales(&self) -> Scales {
        self.scales
    }

    pub fn depth(&mut self) -> &mut Stream {
        &mut self.depth
    }

    pub fn trade(&mut self) -> &mut Stream {
        &mut self.trade
    }
}
