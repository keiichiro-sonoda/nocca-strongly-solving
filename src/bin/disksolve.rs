//! Run the disk-based (frontier) retrograde on a reduced version and report the
//! W/L/D partition, value, DTM, time and peak RSS. Validation/scale tool.
//!
//! Usage: cargo run --release --bin disksolve -- <rows> <cols> <workdir>
//!            [run_size] [fanin] [block_size] [compress(0|1)]
//!   block_size = frontier predecessor block (0 = whole frontier; default 0)
//!   compress   = 1 ⇒ 8-byte rank keys; 0 ⇒ 16-byte plain keys (default 1)

use nocca::disk_retro::solve_disk_frontier;
use nocca::extsort::{self, ExtConfig};

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
    let a: Vec<String> = std::env::args().skip(1).collect();
    if a.len() < 3 {
        eprintln!("usage: disksolve <rows> <cols> <workdir> [run_size] [fanin] [block_size] [compress]");
        std::process::exit(2);
    }
    let rows: usize = a[0].parse().expect("rows");
    let cols: usize = a[1].parse().expect("cols");
    let dir = std::path::PathBuf::from(&a[2]);
    let run_size: usize = a.get(3).and_then(|s| s.parse().ok()).unwrap_or(4_000_000);
    let fanin: usize = a.get(4).and_then(|s| s.parse().ok()).unwrap_or(8);
    let block_size: usize = a.get(5).and_then(|s| s.parse().ok()).unwrap_or(0);
    let compress: bool = a.get(6).map(|s| s != "0").unwrap_or(true);

    let cfg = ExtConfig::new(run_size, fanin);
    extsort::reset_profile();
    let t = std::time::Instant::now();
    let r = solve_disk_frontier(rows, cols, &cfg, block_size, compress, &dir).expect("solve_disk_frontier");
    let secs = t.elapsed().as_secs_f64();
    let (sort_s, gen_s) = (extsort::sort_secs(), extsort::gen_secs());
    let threads = rayon::current_num_threads();

    println!(
        "{}x{} [compress={compress} blk={block_size}]: {:?} dist={}  total={} win={} loss={} draw={}  {:.1}s  peakRSS={:.0}MiB  (run_size={run_size} fanin={fanin})",
        rows, cols, r.value, r.dist, r.n_total, r.n_win, r.n_loss, r.n_draw, secs, peak_rss_mib()
    );
    println!(
        "  profile: threads={threads}  gen(par)={gen_s:.1}s ({:.0}%)  sort(seq)={sort_s:.1}s ({:.0}%)  rest(merge I/O)={:.1}s ({:.0}%)",
        100.0 * gen_s / secs,
        100.0 * sort_s / secs,
        secs - gen_s - sort_s,
        100.0 * (secs - gen_s - sort_s) / secs,
    );
}
