//! In-memory 2-bit bounded retrograde for NOCCA×NOCCA — the 6×5 production
//! solver. Streams the DTM to <stream_dir> (the heavy artifact; HDD for 6×5) and,
//! with `--snap-dir`, takes periodic val+accumulator snapshots (C2; put on SSD,
//! physically separate from the stream) so a crash inside the multi-hour round 2
//! resumes with ≤ `--snap-interval` rollback. All crash modes (SIGKILL / OOM /
//! panic / power-loss, via C4 fsync) are covered.
//!
//! Resume = re-run the SAME command with the SAME <stream_dir>/<snap-dir>; the
//! solver auto-detects the stream marker / snapshot and continues.
//!
//! Usage:
//!   inmem <rows> <cols> <stream_dir> [--conv d1|d0] [--snap-dir DIR]
//!         [--snap-interval SECS] [--stop-after K] [--base]
//!     --conv          d1 = try-win DTM1 (paper-B; production), d0 = DTM0. default d1
//!     --snap-dir      enable C2 snapshots here (SSD recommended)
//!     --snap-interval seconds between round-2 snapshots (default 7200 = 2h)
//!     --stop-after    stop after round K (testing only)
//!     --base          disable the L2+L5 levers (default = L2+L5, oracle-identical)

use nocca::inmem_retro::{dtm_round_sizes, read_meta, run_bounded_snap, Convention, Opts, SnapConfig};
use nocca::mirror_rank::MirrorRanker;
use std::path::PathBuf;
use std::time::Instant;

fn peak_rss_mib() -> f64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmHWM:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse::<f64>().ok())
        })
        .unwrap_or(0.0)
        / 1024.0
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 3 {
        eprintln!(
            "usage: inmem <rows> <cols> <stream_dir> [--conv d1|d0] [--snap-dir DIR] \
             [--snap-interval SECS] [--stop-after K] [--base]"
        );
        std::process::exit(2);
    }
    let rows: usize = args[0].parse().expect("rows");
    let cols: usize = args[1].parse().expect("cols");
    let dir = PathBuf::from(&args[2]);

    let mut conv = Convention::Depth1;
    let mut snap_dir: Option<PathBuf> = None;
    let mut snap_interval: u64 = 7200;
    let mut stop_after: Option<u32> = None;
    let mut base = false;
    let mut i = 3;
    while i < args.len() {
        match args[i].as_str() {
            "--conv" => {
                i += 1;
                conv = match args.get(i).map(|s| s.as_str()) {
                    Some("d0") => Convention::Dtm0,
                    Some("d1") => Convention::Depth1,
                    _ => panic!("--conv expects d1|d0"),
                };
            }
            "--snap-dir" => {
                i += 1;
                snap_dir = Some(PathBuf::from(args.get(i).expect("--snap-dir DIR")));
            }
            "--snap-interval" => {
                i += 1;
                snap_interval = args.get(i).and_then(|s| s.parse().ok()).expect("--snap-interval SECS");
            }
            "--stop-after" => {
                i += 1;
                stop_after = Some(args.get(i).and_then(|s| s.parse().ok()).expect("--stop-after K"));
            }
            "--base" => base = true,
            x => {
                eprintln!("unknown flag: {x}");
                std::process::exit(2);
            }
        }
        i += 1;
    }

    let opts = if base {
        Opts::default()
    } else {
        Opts { fast_preds: true, fast_unrank2: true, ..Default::default() }
    };
    let snap = match &snap_dir {
        Some(d) => SnapConfig::new(d.clone(), snap_interval),
        None => SnapConfig::disabled(),
    };

    eprintln!(
        "[inmem] {rows}x{cols} conv={conv:?} opts={} snap={:?}@{}s stream={}",
        if base { "base" } else { "L2+L5" },
        snap_dir,
        snap_interval,
        dir.display()
    );
    match read_meta(&dir) {
        Some((r, len)) => eprintln!("[inmem] RESUME: stream marker round={r} len={len}"),
        None => eprintln!("[inmem] fresh start"),
    }

    let mr = MirrorRanker::build(rows, cols);
    eprintln!("[inmem] R_full={}", mr.r_full());
    let t0 = Instant::now();
    let (_val, st) =
        run_bounded_snap(&mr, rows, cols, true, &dir, conv, stop_after, opts, snap).expect("run failed");
    let secs = t0.elapsed().as_secs_f64();

    println!(
        "{rows}x{cols} {conv:?}: root={}/{} rounds={} W={} L={} D={} R_full={} {:.1}s peakRSS={:.0}MiB",
        st.root_value, st.root_dist, st.rounds, st.n_win, st.n_lose, st.n_draw, st.r_full, secs,
        peak_rss_mib()
    );
    assert_eq!(st.n_win + st.n_lose + st.n_draw, st.r_full, "W+L+D != R_full (overflow/bug)");

    // D1 parity invariant: wins decided only on odd rounds, loses only on even.
    if conv == Convention::Depth1 {
        if let Ok(sizes) = dtm_round_sizes(&dir) {
            let mut ok = true;
            for &(r, nw, nl) in &sizes {
                if (r % 2 == 1 && nl != 0) || (r % 2 == 0 && r != 0 && nw != 0) {
                    ok = false;
                    eprintln!("[inmem] PARITY VIOLATION round={r} n_win={nw} n_lose={nl}");
                }
            }
            eprintln!(
                "[inmem] D1 parity over {} rounds: {}",
                sizes.len(),
                if ok { "OK (win=odd, lose=even)" } else { "FAILED" }
            );
        }
    }
}
