//! Reading a capture file in the form it is actually stored in.
//!
//! Capture writes `.oqcap`. The archive step compresses it to
//! `.oqcap.zst` and uploads it, and the pull brings that down. So every
//! byte that survives more than a few days on disk is compressed — and
//! until this module existed, **no tool in this workspace could read
//! one**. `oq-book-check`, `oq-trade-check`, `oq-merge`, `oq-resequence`
//! and `oq-ingest` all called `fs::read` and got a zstd frame they then
//! failed to parse.
//!
//! That is the whole pipeline built and one joint missing, which is the
//! shape this repository keeps finding. It matters more here than most,
//! because `oq-book-check`'s entire purpose is to prove an archive can
//! be used, and it could not open the archive as stored.
//!
//! # Why decompress here rather than ask the caller to pipe
//!
//! `zstdcat x.zst | oq-book-check -` needs `zstd` on the machine holding
//! the archive, and the machine holding this archive is a Synology that
//! does not have it and is not a comfortable place to install one. A
//! tool that can only read its own data on a host with an extra package
//! is a tool that will be run somewhere else, on a copy, by hand.
//!
//! # Detection is by content, not by name
//!
//! A zstd frame starts `28 B5 2F FD`. Reading the magic rather than the
//! extension means a compressed file somebody renamed still opens, and
//! a file called `.zst` that is not one fails with that sentence instead
//! of a parse error two layers down.

use std::io::{self, Read};
use std::path::Path;

/// zstd's frame magic, little-endian `0xFD2FB528`.
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

/// Read a capture file, decompressing it if it is compressed.
///
/// # Errors
/// Anything the read reports, and a decompression failure — which is
/// named as one rather than surfacing as a corrupt-record error from
/// whatever tried to parse the frames afterwards.
pub fn read(path: impl AsRef<Path>) -> io::Result<Vec<u8>> {
    let path = path.as_ref();
    let raw = std::fs::read(path)?;
    let looks_compressed = raw.len() >= 4 && raw[..4] == ZSTD_MAGIC;
    let named_compressed = path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("zst"));

    if !looks_compressed {
        if named_compressed {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{} is named .zst but does not start with a zstd frame; \
                     it was renamed, truncated, or never compressed",
                    path.display()
                ),
            ));
        }
        return Ok(raw);
    }

    let mut out = Vec::with_capacity(raw.len() * 4);
    let mut decoder =
        ruzstd::decoding::StreamingDecoder::new(io::Cursor::new(&raw)).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: {e}", path.display()),
            )
        })?;
    decoder.read_to_end(&mut out).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "{}: decompression failed after {} bytes: {e}",
                path.display(),
                out.len()
            ),
        )
    })?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("oq-archive-{}-{name}", std::process::id()));
        p
    }

    #[test]
    fn an_uncompressed_file_reads_through_unchanged() {
        let p = tmp("plain.oqcap");
        std::fs::write(&p, b"not compressed").expect("write");
        assert_eq!(read(&p).expect("read"), b"not compressed");
        let _ = std::fs::remove_file(&p);
    }

    /// A file named `.zst` that is not one says so.
    ///
    /// Without this it reaches the frame parser, which reports a corrupt
    /// record at offset zero — a true statement that sends the reader to
    /// the wrong place entirely.
    #[test]
    fn a_file_named_zst_that_is_not_one_is_named_as_the_problem() {
        let p = tmp("liar.oqcap.zst");
        std::fs::write(&p, b"plain bytes wearing a hat").expect("write");
        let err = read(&p).expect_err("must refuse");
        assert!(
            format!("{err}").contains("does not start with a zstd frame"),
            "{err}"
        );
        let _ = std::fs::remove_file(&p);
    }

    /// And a compressed one round-trips, detected by content rather than
    /// by the name it happens to carry.
    #[test]
    fn a_compressed_file_round_trips_whatever_it_is_called() {
        // A zstd frame of "hello" from the reference encoder, bytes
        // and all. Hand-writing one is how the first version of this
        // test failed: the header claimed a checksum the frame did not
        // carry, and the failure read as a decoder bug rather than a
        // fixture bug.
        let frame: [u8; 18] = [
            0x28, 0xB5, 0x2F, 0xFD, 0x04, 0x58, 0x29, 0x00, 0x00, 0x68, 0x65, 0x6C, 0x6C, 0x6F,
            0xA3, 0x6D, 0x9F, 0x88,
        ];
        for name in ["named.oqcap.zst", "misnamed.oqcap"] {
            let p = tmp(name);
            std::fs::write(&p, frame).expect("write");
            assert_eq!(read(&p).expect("read"), b"hello", "{name}");
            let _ = std::fs::remove_file(&p);
        }
    }
}
