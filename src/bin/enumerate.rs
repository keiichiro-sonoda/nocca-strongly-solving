//! State-space enumeration for NOCCA×NOCCA.
//!
//! Breadth-first search from the opening over *pure* (board + side-to-move)
//! states — no move history, so a repeated position folds into a single state.
//! States are deduplicated by [`Position::canonical_key`] (left-right symmetry),
//! which roughly halves the count. Terminal states are counted but not expanded.
//!
//! Output: per-BFS-depth (ply) count of newly discovered unique states, running
//! cumulative total, next frontier size, terminal count, elapsed time, and real
//! resident memory (Linux `VmRSS`). A `--max N` cap stops the search early so
//! you can observe the early growth curve without running out of memory.
//!
//! Usage:
//!   cargo run --release --bin enumerate -- [--max N] [--no-sym]
//!     --max N    stop after N unique states have been visited (default: no cap)
//!     --no-sym   key states by raw key() instead of canonical_key()
//!                (useful to measure the symmetry reduction factor)

use std::collections::HashSet;
use std::hash::{BuildHasherDefault, Hasher};
use std::time::Instant;

use nocca::moves::MoveList;
use nocca::position::Position;

/// Identity-style hasher for `u128` keys. The key is already a well-mixed bit
/// representation of the board, so we just mix it into 64 bits — far faster than
/// the default SipHash and dependency-free.
#[derive(Default)]
struct U128Hasher(u64);

impl Hasher for U128Hasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
    #[inline]
    fn write_u128(&mut self, n: u128) {
        let mixed = (n ^ (n >> 64)) as u64;
        self.0 = mixed.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    }
    fn write(&mut self, bytes: &[u8]) {
        // Fallback (not used for u128 keys, which call write_u128).
        for &b in bytes {
            self.0 = (self.0.rotate_left(8) ^ b as u64).wrapping_mul(0x1000_0000_01b3);
        }
    }
}

type KeySet = HashSet<u128, BuildHasherDefault<U128Hasher>>;

struct Config {
    max_visited: usize,
    symmetric: bool,
}

fn parse_args() -> Config {
    let mut cfg = Config {
        max_visited: usize::MAX,
        symmetric: true,
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--no-sym" | "--nosym" => cfg.symmetric = false,
            "--sym" => cfg.symmetric = true,
            "--max" => {
                cfg.max_visited = args
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or_else(|| die("--max needs a number"));
            }
            other => {
                // Allow a bare number as a shorthand for --max.
                match other.parse::<usize>() {
                    Ok(n) => cfg.max_visited = n,
                    Err(_) => die(&format!("unknown argument: {other:?}")),
                }
            }
        }
    }
    cfg
}

fn die(msg: &str) -> ! {
    eprintln!("error: {msg}");
    eprintln!("usage: enumerate [--max N] [--no-sym]");
    std::process::exit(2);
}

/// Key a position according to the symmetry setting.
#[inline]
fn key_of(p: &Position, symmetric: bool) -> u128 {
    if symmetric {
        p.canonical_key()
    } else {
        p.key()
    }
}

/// Resident set size in KiB from /proc/self/status (Linux), if available.
fn rss_kib() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            return rest.split_whitespace().next()?.parse().ok();
        }
    }
    None
}

fn fmt_rss() -> String {
    match rss_kib() {
        Some(kib) => format!("{:.2} GiB", kib as f64 / (1024.0 * 1024.0)),
        None => "n/a".to_string(),
    }
}

fn main() {
    let cfg = parse_args();
    let start = Instant::now();

    let mut visited: KeySet = KeySet::default();
    let root = Position::initial();
    let root_key = key_of(&root, cfg.symmetric);
    visited.insert(root_key);

    let mut frontier: Vec<u128> = vec![root_key];
    let mut ml = MoveList::new();
    let mut terminal_total: u64 = 0;
    let mut ply: u32 = 0;
    let mut capped = false;

    println!(
        "BFS state-space enumeration ({}, max_visited={})",
        if cfg.symmetric {
            "canonical / symmetry-reduced"
        } else {
            "raw keys / no symmetry"
        },
        if cfg.max_visited == usize::MAX {
            "none".to_string()
        } else {
            cfg.max_visited.to_string()
        }
    );
    println!(
        "{:>4}  {:>15}  {:>15}  {:>15}  {:>13}  {:>9}  {:>10}",
        "ply", "new_unique", "cumulative", "next_frontier", "terminal_cum", "elapsed", "RSS"
    );
    // Depth 0: just the root.
    println!(
        "{:>4}  {:>15}  {:>15}  {:>15}  {:>13}  {:>8.1}s  {:>10}",
        0,
        1u64,
        visited.len(),
        frontier.len(),
        terminal_total,
        start.elapsed().as_secs_f64(),
        fmt_rss()
    );

    let mut last_heartbeat = Instant::now();

    while !frontier.is_empty() && !capped {
        ply += 1;
        let mut next: Vec<u128> = Vec::new();
        let mut processed: u64 = 0;

        for &k in &frontier {
            let pos = Position::from_key(k);

            // Terminal under try-type: the side to move wins now (uncovered piece
            // on its goal row) or has no legal move. Count it, don't expand.
            if pos.try_win() {
                terminal_total += 1;
                continue;
            }
            pos.generate_legal_moves(&mut ml);
            if ml.is_empty() {
                terminal_total += 1;
                continue;
            }

            for i in 0..ml.len() {
                let child = pos.apply_move(ml.get(i));
                let ck = key_of(&child, cfg.symmetric);
                if visited.insert(ck) {
                    next.push(ck);
                    if visited.len() >= cfg.max_visited {
                        capped = true;
                        break;
                    }
                }
            }

            processed += 1;
            if last_heartbeat.elapsed().as_secs_f64() >= 2.0 {
                eprint!(
                    "\r  ply {ply}: expanded {processed}/{} of frontier, visited {}, {}      ",
                    frontier.len(),
                    visited.len(),
                    fmt_rss()
                );
                last_heartbeat = Instant::now();
            }
            if capped {
                break;
            }
        }
        eprint!("\r{:80}\r", ""); // clear heartbeat line

        println!(
            "{:>4}  {:>15}  {:>15}  {:>15}  {:>13}  {:>8.1}s  {:>10}",
            ply,
            next.len(),
            visited.len(),
            next.len(),
            terminal_total,
            start.elapsed().as_secs_f64(),
            fmt_rss()
        );
        use std::io::Write;
        std::io::stdout().flush().ok();

        frontier = next;
    }

    println!();
    if capped {
        println!(
            "STOPPED at visit cap: {} unique states reached by depth {} (search incomplete).",
            visited.len(),
            ply
        );
    } else {
        println!(
            "COMPLETE: full reachable state space enumerated.\n  unique states : {}\n  terminal      : {}\n  max depth     : {}",
            visited.len(),
            terminal_total,
            ply.saturating_sub(1)
        );
    }
    println!(
        "  elapsed       : {:.1}s\n  peak RSS      : {}",
        start.elapsed().as_secs_f64(),
        fmt_rss()
    );
}
