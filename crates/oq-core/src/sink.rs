//! Where the sequencer records events before applying them.
//!
//! This trait exists so that the ordering guarantee can be *tested*
//! rather than merely documented. "Journal before apply" is the claim
//! the whole recovery story rests on, and until a test can make the
//! journal fail on demand, nothing stops a refactor from swapping the
//! two lines for a plausible-looking reason and leaving the suite green.
//!
//! With an injectable sink, the test is direct: fail the append, then
//! assert the kernel did not move. An implementation that applied first
//! cannot pass it.

use oq_journal::{Result, SyncPolicy, Writer};
use std::path::Path;

/// Somewhere durable to put an event.
pub trait EventSink {
    /// Record one event, returning its sequence number.
    ///
    /// # Errors
    /// Whatever the underlying medium reports. A caller that receives
    /// an error must not treat the event as having happened.
    fn append(&mut self, kind: u16, payload: &[u8]) -> Result<u64>;

    /// Push buffered records to the OS.
    ///
    /// # Errors
    /// I/O failures.
    fn flush(&mut self) -> Result<()>;

    /// Push to the OS and fsync.
    ///
    /// # Errors
    /// I/O failures.
    fn sync(&mut self) -> Result<()>;
}

impl EventSink for Writer {
    fn append(&mut self, kind: u16, payload: &[u8]) -> Result<u64> {
        Self::append(self, kind, payload)
    }

    fn flush(&mut self) -> Result<()> {
        Self::flush(self)
    }

    fn sync(&mut self) -> Result<()> {
        Self::sync(self)
    }
}

/// Open a journal file as a sink.
///
/// # Errors
/// I/O failures, or corruption in the middle of an existing journal.
pub fn file_sink(path: impl AsRef<Path>, policy: SyncPolicy) -> Result<Writer> {
    Writer::open(path, policy)
}

/// A sink that keeps records in memory.
///
/// For tests and for embedded uses where the journal's replay and audit
/// value is wanted without a file. It is not durable, and it says so in
/// its name rather than in a comment someone might not read.
#[derive(Debug, Default)]
pub struct MemorySink {
    records: Vec<(u16, Vec<u8>)>,
}

impl MemorySink {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn records(&self) -> &[(u16, Vec<u8>)] {
        &self.records
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

impl EventSink for MemorySink {
    fn append(&mut self, kind: u16, payload: &[u8]) -> Result<u64> {
        let seq = self.records.len() as u64;
        self.records.push((kind, payload.to_vec()));
        Ok(seq)
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }

    fn sync(&mut self) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_sink_numbers_from_zero() {
        let mut sink = MemorySink::new();
        assert_eq!(sink.append(1, b"a").expect("append"), 0);
        assert_eq!(sink.append(1, b"b").expect("append"), 1);
        assert_eq!(sink.len(), 2);
        assert_eq!(sink.records()[1].1, b"b");
    }
}
