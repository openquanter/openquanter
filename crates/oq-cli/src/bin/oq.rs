//! `oq` — one name for the tools.
//!
//! ```text
//! oq                    list what there is
//! oq capture --help     everything after the subcommand goes to the tool
//! ```
//!
//! Finds `oq-<subcommand>` on `PATH` and replaces this process with it,
//! so exit codes, signals and terminal behaviour are the tool's own. A
//! launcher that spawned a child and waited would sit between the user
//! and the process they think they are talking to, and Ctrl-C would go
//! to the wrong one.

use std::process::ExitCode;

use oq_cli::{TOOLS, binary_for, crate_for};

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let Some(sub) = args.next() else {
        list();
        return ExitCode::SUCCESS;
    };
    let sub = sub.to_string_lossy().to_string();
    if sub == "--help" || sub == "-h" || sub == "help" {
        list();
        return ExitCode::SUCCESS;
    }
    if sub == "--version" || sub == "-V" {
        println!("oq {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }

    let Some(binary) = binary_for(&sub) else {
        eprintln!("oq: no such tool: {sub}");
        eprintln!();
        list();
        return ExitCode::FAILURE;
    };

    let rest: Vec<std::ffi::OsString> = args.collect();
    let err = exec(&binary, &rest);

    // Reached only if exec failed, which on this path means the binary is
    // not on PATH. The message names the crate that ships it, because
    // "not found" leaves the reader to discover that nine tools live in
    // four crates.
    eprintln!("oq: could not run {binary}: {err}");
    if let Some(krate) = crate_for(&sub) {
        eprintln!("    it ships in {krate}: cargo install {krate}");
    }
    ExitCode::FAILURE
}

/// Replace this process with the tool, on platforms that can.
#[cfg(unix)]
fn exec(binary: &str, args: &[std::ffi::OsString]) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    std::process::Command::new(binary).args(args).exec()
}

/// Elsewhere, run it and forward the status.
///
/// Not an exec, so a signal reaches this process rather than the tool.
/// Recorded rather than hidden: the behaviour differs and a reader
/// debugging a Ctrl-C that did not land should find out here.
#[cfg(not(unix))]
fn exec(binary: &str, args: &[std::ffi::OsString]) -> std::io::Error {
    match std::process::Command::new(binary).args(args).status() {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(e) => e,
    }
}

fn list() {
    println!("oq — the OpenQuanter tools under one name");
    println!();
    println!("USAGE:");
    println!("    oq <tool> [ARGS...]");
    println!();
    println!("TOOLS:");
    for (name, what) in TOOLS {
        println!("    {name:<13} {what}");
    }
    println!();
    println!("Everything after the tool name is passed to it unchanged, so");
    println!("`oq capture --help` is `oq-capture --help`.");
    println!();
    println!("The tools ship inside the crates that need them, not as separate");
    println!("packages: oq-l2feed, oq-ingest, oq-gateway, oq-live.");
}
