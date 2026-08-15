//! Free-space guard.
//!
//! Capture grows monotonically and the host it runs on usually has other
//! jobs. Filling the disk does not merely stop capture — it takes down
//! whatever else lives there, which is a far worse outcome than losing
//! the tail of a trial run. So the capture loop checks free space and
//! stops itself while there is still room, rather than discovering the
//! limit the way everyone else on the machine discovers it.
//!
//! Free space is read by running `df`, which is available everywhere
//! this runs and needs no dependency. The parser is tested against real
//! output from both Linux and macOS because their column layouts differ.

use std::io;
use std::path::Path;
use std::process::Command;

/// Bytes available to an unprivileged writer under `path`.
///
/// # Errors
///
/// Fails when `df` cannot be run or its output cannot be parsed.
pub fn available_bytes(path: &Path) -> io::Result<u64> {
    let output = Command::new("df").arg("-k").arg(path).output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "df failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    parse_df_kilobytes(&String::from_utf8_lossy(&output.stdout))
        .map(|kb| kb * 1024)
        .ok_or_else(|| io::Error::other("could not parse df output"))
}

/// Parse the available-kilobytes column out of `df -k` output.
///
/// Both Linux and macOS put it in the fourth whitespace-separated column
/// of the data row, but their headers and extra columns differ, so the
/// parse is positional on the data row only.
#[must_use]
pub fn parse_df_kilobytes(output: &str) -> Option<u64> {
    // A long device name wraps onto its own line, so the fields of
    // interest are not necessarily on the second line.
    let fields: Vec<&str> = output
        .lines()
        .skip(1)
        .flat_map(str::split_whitespace)
        .collect();
    fields.get(3)?.parse().ok()
}

/// Whether free space under `path` is at or above `floor_bytes`.
///
/// # Errors
///
/// As [`available_bytes`].
pub fn above_floor(path: &Path, floor_bytes: u64) -> io::Result<bool> {
    Ok(available_bytes(path)? >= floor_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_linux_output() {
        let out = "Filesystem     1K-blocks     Used Available Use% Mounted on\n\
                   /dev/vda2       51290592 16106112  33547776  33% /\n";
        assert_eq!(parse_df_kilobytes(out), Some(33_547_776));
    }

    #[test]
    fn parses_macos_output() {
        let out = "Filesystem 1024-blocks      Used Available Capacity iused ifree %iused  Mounted on\n\
                   /dev/disk3s5  971350180 512345678 458004502    53%  1234 5678   18%   /System/Volumes/Data\n";
        assert_eq!(parse_df_kilobytes(out), Some(458_004_502));
    }

    #[test]
    fn parses_output_wrapped_onto_a_second_line() {
        let out = "Filesystem     1K-blocks     Used Available Use% Mounted on\n\
                   /dev/mapper/a-very-long-logical-volume-name\n\
                                   51290592 16106112  33547776  33% /\n";
        assert_eq!(parse_df_kilobytes(out), Some(33_547_776));
    }

    #[test]
    fn rejects_unparseable_output_rather_than_guessing() {
        assert_eq!(parse_df_kilobytes(""), None);
        assert_eq!(parse_df_kilobytes("Filesystem\n"), None);
        assert_eq!(parse_df_kilobytes("h1 h2\nnot a number here\n"), None);
    }

    #[test]
    fn reads_real_free_space_for_the_temp_directory() {
        let bytes = available_bytes(&std::env::temp_dir()).expect("df works here");
        assert!(bytes > 0, "the temp directory should have some space");
    }
}
