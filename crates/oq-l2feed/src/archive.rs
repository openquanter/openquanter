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
//!
//! # Finding a capture file is the other half
//!
//! Opening one is no use to a tool that never offers it. Anything that
//! walks a directory, groups files by hour, or reads a symbol out of a
//! path is deciding what counts as a capture file **by its name**, and a
//! name-based rule written before the archive was compressed rejects
//! every file the archive actually holds -- `13.oqcap.zst` has the
//! extension `zst` and the stem `13.oqcap`, so an hourly grouping finds
//! no hours and a tool reports an empty directory rather than a
//! mismatch. That failure looks exactly like "there is no data here".
//!
//! So the naming rule lives beside the reading rule, and both are here.
//!
//! # Decompression is bounded
//!
//! A zstd frame can expand without limit: 33 KB of crafted input
//! decodes to 1 GB, and every byte of it lands in one `Vec`. The files
//! read here come from an archive another host writes, so a frame is
//! trusted to expand only as far as real capture data does — measured
//! at 5x to 10x at the archive's level 9 — with room to spare, and
//! refused past that instead of taking the machine's memory with it.

use std::io::{self, Read};
use std::path::Path;

/// zstd's frame magic, little-endian `0xFD2FB528`.
const ZSTD_MAGIC: [u8; 4] = [0x28, 0xB5, 0x2F, 0xFD];

/// How far a frame may expand, as a multiple of its compressed size.
///
/// Real capture data compresses 5x to 10x; 64x leaves six times the
/// largest ratio seen and still stops a crafted frame at a small
/// fraction of what it would expand to.
const MAX_RATIO: usize = 64;

/// Output any file may reach whatever its size, so a small file that
/// compresses unusually well is never the one refused.
const MIN_LIMIT: usize = 64 << 20;

/// The capture extension, before any compression suffix.
const CAPTURE_EXT: &str = ".oqcap";

/// The part of a capture file's name before `.oqcap[.zst]`.
///
/// `13` from both `13.oqcap` and `13.oqcap.zst`; the hour under an
/// hourly rotation and the day under a daily one, which is what every
/// caller here wants and what `Path::file_stem` does not give for the
/// compressed form.
///
/// `None` for anything that is not a capture file, so this doubles as
/// the test for one -- a caller filtering a directory and a caller
/// naming a window ask the same question, and answering it twice is how
/// they come to disagree.
#[must_use]
pub fn stem(path: impl AsRef<Path>) -> Option<String> {
    let name = path.as_ref().file_name()?.to_str()?;
    let base = name.strip_suffix(".zst").unwrap_or(name);
    base.strip_suffix(CAPTURE_EXT).map(str::to_string)
}

/// Whether a path names a capture file, compressed or not.
#[must_use]
pub fn is_capture(path: impl AsRef<Path>) -> bool {
    stem(path).is_some()
}

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

    decompress(&raw, limit(raw.len()))
        .map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", path.display())))
}

/// The most a frame of `compressed` bytes may decode to.
fn limit(compressed: usize) -> usize {
    MIN_LIMIT.max(compressed.saturating_mul(MAX_RATIO))
}

/// Decode one zstd stream, refusing to produce more than `limit` bytes.
fn decompress(raw: &[u8], limit: usize) -> io::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(raw.len().saturating_mul(4).min(limit));
    let decoder = ruzstd::decoding::StreamingDecoder::new(io::Cursor::new(raw))
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    // One byte past the limit is how an output of exactly `limit` is
    // told apart from one that would have kept going.
    let cap = u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1);
    decoder.take(cap).read_to_end(&mut out).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("decompression failed after {} bytes: {e}", out.len()),
        )
    })?;
    if out.len() > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "expands past {limit} bytes, {}x its {} compressed; \
                 no real capture compresses that well",
                limit / raw.len().max(1),
                raw.len()
            ),
        ));
    }
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

    /// A frame that expands past its limit is refused, not decoded into
    /// memory; one that reaches the limit exactly is not.
    #[test]
    fn a_frame_that_expands_past_its_limit_is_refused() {
        let zeros = vec![0u8; 1 << 20];
        let frame = ruzstd::encoding::compress_to_vec(
            zeros.as_slice(),
            ruzstd::encoding::CompressionLevel::Fastest,
        );
        assert!(
            frame.len() * MAX_RATIO < zeros.len(),
            "the fixture is a bomb"
        );

        assert_eq!(
            decompress(&frame, zeros.len()).expect("at the limit"),
            zeros
        );
        let err = decompress(&frame, zeros.len() - 1).expect_err("past the limit");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(format!("{err}").contains("expands past"), "{err}");
    }

    /// The limit follows the file's size: a floor so a small, very
    /// compressible file is never the one refused, and above it a ratio
    /// well clear of any real capture.
    #[test]
    fn the_limit_scales_with_the_compressed_size() {
        assert_eq!(limit(0), MIN_LIMIT);
        assert_eq!(
            limit(33 << 10),
            MIN_LIMIT,
            "the 33 KB bomb stops at the floor"
        );
        // The largest hourly file measured: 101 MB compressed, 1.1 GB raw.
        assert!(limit(101_511_938) > 1_113_493_011);
        assert_eq!(limit(1 << 30), 64 << 30);
        assert_eq!(limit(usize::MAX), usize::MAX, "saturates, never wraps");
    }

    /// Through `read`, a small file that compresses far past the ratio
    /// still opens, because it is under the floor.
    #[test]
    fn a_small_very_compressible_file_still_reads() {
        let small = vec![7u8; 1 << 20];
        let p = tmp("small.oqcap.zst");
        std::fs::write(
            &p,
            ruzstd::encoding::compress_to_vec(
                small.as_slice(),
                ruzstd::encoding::CompressionLevel::Fastest,
            ),
        )
        .expect("write");
        assert_eq!(read(&p).expect("under the floor"), small);
        let _ = std::fs::remove_file(&p);
    }

    /// The hour is `13` whether or not the file has been compressed.
    ///
    /// `Path::file_stem` gives `13.oqcap` for the compressed form, and
    /// an hourly grouping keyed on that finds no hour matching `13` --
    /// so a tool reports an empty directory for an archive full of
    /// data, which reads as "nothing was captured".
    #[test]
    fn the_stem_is_the_same_compressed_or_not() {
        assert_eq!(stem("a/b/13.oqcap").as_deref(), Some("13"));
        assert_eq!(stem("a/b/13.oqcap.zst").as_deref(), Some("13"));
        assert_eq!(stem("2026-08-19.oqcap.zst").as_deref(), Some("2026-08-19"));
        assert_eq!(
            std::path::Path::new("a/b/13.oqcap.zst")
                .file_stem()
                .and_then(|s| s.to_str()),
            Some("13.oqcap"),
            "the trap this exists to avoid"
        );
    }

    #[test]
    fn anything_that_is_not_a_capture_file_has_no_stem() {
        for name in [
            "13.json",
            "13.manifest.json",
            "13.oqcap.gz",
            "oqcap",
            "13.oqcapx",
            "",
        ] {
            assert!(stem(name).is_none(), "{name} is not a capture file");
            assert!(!is_capture(name), "{name}");
        }
        assert!(is_capture("13.oqcap"));
        assert!(is_capture("13.oqcap.zst"));
    }
}
