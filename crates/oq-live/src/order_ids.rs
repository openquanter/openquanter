//! Reserve client-order sequences durably before a live session can send.
//!
//! Ownership stays stable across restarts; sequence numbers do not restart.
//! The host holds its interlock while reserving. Persisting the entire range
//! first makes a crash burn unused ids instead of assigning them twice.

use std::io::{self, Write};
use std::ops::RangeInclusive;
use std::path::Path;

const RESERVATION: u64 = 1_000_000_000;

impl crate::interlock::Interlock {
    /// Reserve ids under an existing exclusive host interlock.
    ///
    /// `root` must survive process restarts; `clock_floor` seeds a new store
    /// beyond legacy small sequence numbers. An existing high-water mark wins
    /// over a backwards clock. A missing or unreadable write is fatal.
    pub(crate) fn reserve_order_ids(
        &self,
        root: &Path,
        clock_floor: u64,
    ) -> io::Result<RangeInclusive<u64>> {
        std::fs::create_dir_all(root)?;
        let name = self
            .path()
            .file_name()
            .ok_or_else(|| io::Error::other("missing lock name"))?;
        let path = root.join(name).with_extension("ids");
        let prior = match std::fs::read_to_string(&path) {
            Ok(s) => s.trim().parse::<u64>().map_err(io::Error::other)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => 0,
            Err(e) => return Err(e),
        };
        let floor = prior.max(clock_floor);
        let end = floor
            .checked_add(RESERVATION)
            .ok_or_else(|| io::Error::other("client order sequence exhausted"))?;
        // Truncating the high-water file could erase it on a crash. Publish a
        // fully synced replacement instead. A leftover staging file refuses
        // startup, rather than quietly resetting an uncertain allocation.
        let temporary = path.with_extension("ids.next");
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        writeln!(file, "{end}")?;
        file.sync_all()?;
        std::fs::rename(&temporary, &path)?;
        std::fs::File::open(root)?.sync_all()?;
        Ok(floor + 1..=end)
    }
}

#[cfg(test)]
mod tests {
    use crate::interlock::Interlock;

    #[test]
    fn restart_and_backwards_clock_never_reuse_reserved_ids() {
        let prefix = format!("ids-restart-{}", std::process::id());
        let root = std::env::temp_dir().join(&prefix);
        let _ = std::fs::remove_dir_all(&root);
        let held = Interlock::claim("test", "contract", &prefix).unwrap();
        let first = held.reserve_order_ids(&root, 100).unwrap();
        drop(held);
        let held = Interlock::claim("test", "contract", &prefix).unwrap();
        let second = held.reserve_order_ids(&root, 50).unwrap();
        assert!(second.start() > first.end());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn corrupted_high_water_mark_does_not_reset_to_zero() {
        let prefix = format!("ids-corrupt-{}", std::process::id());
        let root = std::env::temp_dir().join(&prefix);
        let _ = std::fs::remove_dir_all(&root);
        let held = Interlock::claim("test", "contract", &prefix).unwrap();
        held.reserve_order_ids(&root, 100).unwrap();
        let path = root
            .join(held.path().file_name().unwrap())
            .with_extension("ids");
        std::fs::write(path, "broken").unwrap();
        assert!(held.reserve_order_ids(&root, 200).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn interrupted_publication_refuses_startup() {
        let prefix = format!("ids-interrupted-{}", std::process::id());
        let root = std::env::temp_dir().join(&prefix);
        let _ = std::fs::remove_dir_all(&root);
        let held = Interlock::claim("test", "contract", &prefix).unwrap();
        let first = held.reserve_order_ids(&root, 100).unwrap();
        let path = root
            .join(held.path().file_name().unwrap())
            .with_extension("ids.next");
        std::fs::write(path, "uncertain reservation").unwrap();
        assert!(held.reserve_order_ids(&root, 200).is_err());
        let published = root
            .join(held.path().file_name().unwrap())
            .with_extension("ids");
        assert_eq!(
            std::fs::read_to_string(published).unwrap().trim(),
            first.end().to_string()
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
