//! `oq-capture` — record one market data stream to the archive.
//!
//! One process, one stream. Running five streams means five processes,
//! which is the point: a crash, a restart, or a disk problem in one
//! stream must not touch the others, and the operating system already
//! knows how to supervise processes.
//!
//! ```text
//! oq-capture --root ./archive --symbol BTCUSDT --stream depth \
//!            --minutes 60 --floor-gb 10
//! ```
//!
//! Argument parsing is hand-written. The surface is six flags, and a
//! capture host should be able to build this crate without pulling a
//! parser and its dependency tree into the binary that is supposed to
//! keep running when everything else is broken.

use std::process::ExitCode;
use std::time::Duration;

use oq_l2feed::day::Rotation;
use oq_l2feed::session::{SessionConfig, StopReason, run};
use oq_l2feed::stream::{Software, StreamId};
use oq_l2feed::venue::{binance_perp_polls, binance_perp_streams, binance_perp_url};
use oq_l2feed::writer::CaptureWriter;
use oq_l2feed::ws::{PollConnector, WsConnector};

const USAGE: &str = "\
oq-capture — record one market data stream verbatim

USAGE:
    oq-capture --root <DIR> --symbol <SYMBOL> --stream <NAME> [OPTIONS]

OPTIONS:
    --root <DIR>          Archive root directory
    --symbol <SYMBOL>     Instrument, e.g. BTCUSDT
    --stream <NAME>       depth | bookTicker | trade | forceOrder | markPrice
                          (markPrice is polled over REST; the rest are streams)
    --venue <NAME>        Venue label for the archive path [default: binance-perp]
    --minutes <N>         Stop after N minutes [default: run until interrupted]
    --floor-gb <N>        Stop when free space falls below N GiB [default: 10]
    --rotation <WHEN>     daily | hourly [default: daily]
                          Hourly suits a host that cannot hold two days of raw
                          capture: the open file cannot be compressed, so the
                          local peak is always about two rotation periods.
    --help                Print this message
";

fn main() -> ExitCode {
    match real_main() {
        Ok(code) => code,
        Err(message) => {
            eprintln!("oq-capture: {message}");
            ExitCode::FAILURE
        }
    }
}

fn real_main() -> Result<ExitCode, String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{USAGE}");
        return Ok(ExitCode::SUCCESS);
    }

    let root = required(&args, "--root")?;
    let symbol = required(&args, "--symbol")?;
    let stream_name = required(&args, "--stream")?;
    let venue = optional(&args, "--venue").unwrap_or_else(|| "binance-perp".to_string());
    let minutes = optional(&args, "--minutes")
        .map(|v| {
            v.parse::<u64>()
                .map_err(|_| "--minutes must be a number".to_string())
        })
        .transpose()?;
    let floor_gb = optional(&args, "--floor-gb")
        .map(|v| {
            v.parse::<u64>()
                .map_err(|_| "--floor-gb must be a number".to_string())
        })
        .transpose()?
        .unwrap_or(10);

    // Some data has a stream; some only has an endpoint to poll. Both
    // go through the same capture path.
    let socket = binance_perp_streams(&symbol)
        .into_iter()
        .find(|s| s.name == stream_name);
    let poll = binance_perp_polls(&symbol)
        .into_iter()
        .find(|p| p.name == stream_name);

    let (name, url, poll_interval) = match (socket, poll) {
        (Some(spec), _) => {
            let url = binance_perp_url(&spec.topic);
            (spec.name, url, None)
        }
        (None, Some(spec)) => (
            spec.name,
            spec.url,
            Some(Duration::from_secs(spec.interval_secs)),
        ),
        (None, None) => {
            return Err(format!(
                "unknown stream {stream_name:?}; expected one of depth, bookTicker, trade, forceOrder, markPrice"
            ));
        }
    };

    let stream = StreamId::new(&venue, &symbol, &name);
    let software = Software::new(
        concat!("oq-l2feed ", env!("CARGO_PKG_VERSION")),
        option_env!("OQ_BUILD_COMMIT").unwrap_or("unknown"),
    );

    let mut config = SessionConfig::new(&root, stream.clone(), software.clone(), &url);
    config.duration = minutes.map(|m| Duration::from_secs(m * 60));
    config.disk_floor_bytes = floor_gb * 1024 * 1024 * 1024;

    let rotation = optional(&args, "--rotation")
        .map(|v| Rotation::parse(&v).ok_or_else(|| format!("unknown rotation {v:?}")))
        .transpose()?
        .unwrap_or(Rotation::Daily);

    let mut writer = CaptureWriter::new(&root, stream, software)
        .map_err(|e| e.to_string())?
        .with_rotation(rotation);

    eprintln!(
        "capturing {} {} from {url}",
        config.stream.symbol, config.stream.stream
    );
    eprintln!("archive root {root}, rotating {rotation:?}, stopping below {floor_gb} GiB free");

    let stats = match poll_interval {
        Some(interval) => {
            eprintln!("polling every {}s", interval.as_secs());
            let mut connector = PollConnector::new(&url, interval);
            run(&config, &mut connector, &mut writer)
        }
        None => {
            // A silent socket must look different from a quiet market,
            // or a half-open connection would be recorded as an
            // uneventful hour.
            let mut connector = WsConnector::new(&url, Duration::from_secs(60));
            run(&config, &mut connector, &mut writer)
        }
    }
    .map_err(|e| e.to_string())?;
    let sealed = writer.seal().map_err(|e| e.to_string())?;

    let mib = |bytes: u64| bytes as f64 / (1024.0 * 1024.0);
    eprintln!("stopped: {:?}", stats.stop);
    eprintln!(
        "  {} payloads, {:.1} MiB, {} gap(s) totalling {:.1}s over {:.1} min",
        stats.payloads,
        mib(stats.payload_bytes),
        stats.gaps,
        stats.outage.as_secs_f64(),
        stats.elapsed.as_secs_f64() / 60.0
    );
    eprintln!(
        "  projected {:.2} GiB/day at this rate",
        stats.projected_bytes_per_day() as f64 / (1024.0 * 1024.0 * 1024.0)
    );
    eprintln!(
        "  sealed {} ({} records)",
        sealed.window, sealed.manifest.records
    );
    eprintln!("  manifest {}", sealed.manifest_path.display());

    // A capture that stopped because the disk was filling is not a
    // success, even though it shut down cleanly.
    Ok(match stats.stop {
        StopReason::DurationElapsed => ExitCode::SUCCESS,
        StopReason::DiskFloor | StopReason::ConnectionLost => ExitCode::FAILURE,
    })
}

fn optional(args: &[String], flag: &str) -> Option<String> {
    let index = args.iter().position(|a| a == flag)?;
    args.get(index + 1).cloned()
}

fn required(args: &[String], flag: &str) -> Result<String, String> {
    optional(args, flag).ok_or_else(|| format!("missing {flag}\n\n{USAGE}"))
}
