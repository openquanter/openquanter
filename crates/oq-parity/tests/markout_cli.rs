//! `oq-parity markout`, run as a user runs it: a run file and a tick file
//! on disk, and the numbers it prints.

use std::process::Command;

use oq_engine::Tick;
use oq_parity::manifest::RunManifest;
use oq_parity::wire::Run;
use oq_parity::{Fill, RunOutput};
use oq_types::{Side, Stamp};

const S: i64 = 1_000_000_000;

fn write(dir: &std::path::Path, name: &str, bytes: &[u8]) -> String {
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("write");
    path.display().to_string()
}

#[test]
fn markout_prints_each_horizon_and_the_contrast() {
    let dir = std::env::temp_dir().join(format!("oq-markout-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");

    // A market that rises a tick a second for two minutes.
    let ticks: Vec<Tick> = (0..120)
        .map(|i| {
            let p = 10_000 + i;
            Tick::trades_only(Stamp::new(i * S, i * S), p, p, p)
        })
        .collect();
    let ticks_path = write(&dir, "t.oqtk", &oq_data::ticks::encode(1, &ticks));

    // Forty buys at the market in the first seconds: every one favourable.
    let run = |price_offset: i64| {
        let fills = (0..40)
            .map(|i| Fill::new(i * S / 10, "X", Side::Buy, 10_000 + price_offset, 1))
            .collect();
        Run::new(
            RunManifest::from_content("c", b"d", b"g", "L0"),
            RunOutput::new(fills, 0.0),
        )
        .render()
    };
    let backtest = write(&dir, "b.run", run(0).as_bytes());
    let live = write(&dir, "l.run", run(5).as_bytes());

    let out = Command::new(env!("CARGO_BIN_EXE_oq-parity"))
        .args(["markout", &backtest, &ticks_path, &live])
        .output()
        .expect("runs");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{text}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("    1 s      40 fills"), "{text}");
    assert!(text.contains("   60 s      40 fills"), "{text}");
    assert!(text.contains("adverse   0.0%"), "{text}");
    // Filled five ticks worse: five basis points of 10,000.
    assert!(text.contains("contrast"), "{text}");
    assert!(text.contains("    1 s     -5.00 bp"), "{text}");

    let missing = Command::new(env!("CARGO_BIN_EXE_oq-parity"))
        .args(["markout", &backtest, "/nonexistent.oqtk"])
        .output()
        .expect("runs");
    assert_eq!(missing.status.code(), Some(3));
    let _ = std::fs::remove_dir_all(&dir);
}
