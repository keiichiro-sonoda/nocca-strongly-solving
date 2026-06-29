//! Paper-B reachable projection for the 6×5 NOCCA×NOCCA in-memory solution.
//!
//! The retrograde solved the *full* mirror-rep space (R_full). Paper B (山本・保木
//! 2022) reports counts over the *reachable* set (pseudo-reachable, mirror-unfolded).
//! This tool: (1) forward-BFS the reachable set in mirror_rank index space, (2) marks
//! self-symmetric reps, (3) streams the kept DTM stream intersecting with reachable
//! to get per-DTM win/lose counts, then un-folds (pseudo = 2·mirror − self_sym) and
//! compares to Table 8 (reachable W/L/D) and Table A.1 (per-ply distribution).
//!
//! Usage:
//!   reachproj <rows> <cols> <stream_dir> --work-dir DIR [--stage 1|2|3|all]
//!     stage 1  forward BFS → reachable.bits + census (照合A, avg moves, terminals)
//!     stage 2  self-symmetric bitset → selfsym.bits + |selfsym|, |selfsym∩reach|
//!     stage 3  stream projection → un-fold → Table 8 / Table A.1 comparison
//!     all      1→2→3 in one process (bitsets kept in RAM)

use nocca::inmem_retro::{
    bitset_intersection_count, dump_atomic_words, load_atomic_words, project_stream,
    reachable_bfs, self_sym_bitset, BitSet,
};
use nocca::mirror_rank::MirrorRanker;
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};
use std::time::Instant;

fn peak_rss_gib() -> f64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmHWM:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse::<f64>().ok())
        })
        .unwrap_or(0.0)
        / (1024.0 * 1024.0)
}

fn dump_bits(bs: &BitSet, path: &Path) {
    let f = std::fs::File::create(path).expect("create bits");
    let mut bw = BufWriter::with_capacity(8 << 20, f);
    dump_atomic_words(bs.words(), &mut bw).expect("dump bits");
}

fn load_bits(r_full: u64, path: &Path) -> BitSet {
    let bs = BitSet::new(r_full);
    let f = std::fs::File::open(path).expect("open bits");
    let mut br = BufReader::with_capacity(8 << 20, f);
    load_atomic_words(bs.words(), &mut br).expect("load bits");
    bs
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 3 {
        eprintln!("usage: reachproj <rows> <cols> <stream_dir> --work-dir DIR [--stage 1|2|3|all]");
        std::process::exit(2);
    }
    let rows: usize = args[0].parse().expect("rows");
    let cols: usize = args[1].parse().expect("cols");
    let stream_dir = PathBuf::from(&args[2]);
    let mut work: Option<PathBuf> = None;
    let mut stage = "all".to_string();
    let mut ckpt_interval: u64 = 0;
    let mut expand_try = false;
    let mut i = 3;
    while i < args.len() {
        match args[i].as_str() {
            "--work-dir" => {
                i += 1;
                work = Some(PathBuf::from(args.get(i).expect("--work-dir DIR")));
            }
            "--stage" => {
                i += 1;
                stage = args.get(i).expect("--stage").clone();
            }
            "--ckpt-interval" => {
                i += 1;
                ckpt_interval = args.get(i).and_then(|s| s.parse().ok()).expect("--ckpt-interval SECS");
            }
            "--expand-try" => expand_try = true,
            x => {
                eprintln!("unknown flag: {x}");
                std::process::exit(2);
            }
        }
        i += 1;
    }
    let work = work.unwrap_or_else(|| PathBuf::from("./reachproj_work"));
    std::fs::create_dir_all(&work).expect("work dir");
    let reach_path = work.join("reachable.bits");
    let sym_path = work.join("selfsym.bits");

    eprintln!("[reachproj] {rows}x{cols} stream={} work={} stage={stage}", stream_dir.display(), work.display());
    let mr = MirrorRanker::build(rows, cols);
    let r_full = mr.r_full();
    eprintln!("[reachproj] R_full={r_full}");

    let mut reachable: Option<BitSet> = None;
    let mut selfsym: Option<BitSet> = None;

    // -------- stage 1: forward BFS (reachable set) --------
    if stage == "1" || stage == "all" {
        let t = Instant::now();
        let ckpt = if ckpt_interval > 0 {
            Some((work.as_path(), std::time::Duration::from_secs(ckpt_interval)))
        } else {
            None
        };
        eprintln!("[reachproj] expand_try={expand_try} (true=paper-B reachability)");
        let (vis, st) = reachable_bfs(&mr, rows, cols, true, expand_try, ckpt);
        let secs = t.elapsed().as_secs_f64();
        let sum = st.try_win_term + st.no_move_term + st.nonterm;
        println!(
            "STAGE1 {rows}x{cols}: reachable={} try_win_term={} no_move_term={} nonterm={} \
             (sum={} {}) max_depth={} avg_moves(nonterm)={:.3} avg_moves(reach)={:.3} \
             {:.1}s peakRSS={:.1}GiB",
            st.reachable, st.try_win_term, st.no_move_term, st.nonterm, sum,
            if sum == st.reachable { "OK" } else { "MISMATCH" },
            st.max_depth,
            st.total_moves as f64 / st.nonterm.max(1) as f64,
            st.total_moves as f64 / st.reachable.max(1) as f64,
            secs, peak_rss_gib()
        );
        dump_bits(&vis, &reach_path);
        eprintln!("[reachproj] reachable.bits dumped ({} bytes-ish)", vis.words().len() * 8);
        reachable = Some(vis);
    }

    // -------- stage 2: self-symmetric bitset --------
    if stage == "2" || stage == "all" {
        let t = Instant::now();
        let ss = self_sym_bitset(&mr);
        let n_sym = ss.count();
        dump_bits(&ss, &sym_path);
        let reach = reachable.take().unwrap_or_else(|| load_bits(r_full, &reach_path));
        let s_reach = bitset_intersection_count(&reach, &ss);
        let expect_sfull = nocca::mirror_rank::counts(rows, cols).s_full;
        println!(
            "STAGE2 {rows}x{cols}: self_sym(S_full)={n_sym} (expect {expect_sfull} {}) \
             self_sym∩reachable(S_reach)={s_reach} {:.1}s peakRSS={:.1}GiB",
            if n_sym == expect_sfull { "OK" } else { "MISMATCH" },
            t.elapsed().as_secs_f64(), peak_rss_gib()
        );
        selfsym = Some(ss);
        reachable = Some(reach);
    }

    // -------- stage 3: stream projection → un-fold → compare --------
    if stage == "3" || stage == "all" {
        let reach = reachable.take().unwrap_or_else(|| load_bits(r_full, &reach_path));
        let ss = selfsym.take().unwrap_or_else(|| load_bits(r_full, &sym_path));
        let reach_total = reach.count();
        let sreach_total = bitset_intersection_count(&reach, &ss);

        let t = Instant::now();
        let proj = project_stream(&stream_dir, &reach, &ss).expect("project_stream");
        eprintln!("[reachproj] stream projected in {:.1}s", t.elapsed().as_secs_f64());

        // Totals over mirror reps and self-symmetric reps.
        let (mut mw, mut ml, mut sw, mut sl) = (0u64, 0u64, 0u64, 0u64);
        for r in &proj {
            mw += r.mwin;
            ml += r.mlose;
            sw += r.swin;
            sl += r.slose;
        }
        let mdraw = reach_total - mw - ml;
        let sdraw = sreach_total - sw - sl;
        // Un-fold: pseudo = 2*mirror - self_sym.
        let pw = 2 * mw - sw;
        let pl = 2 * ml - sl;
        let pd = 2 * mdraw - sdraw;
        println!("--- STAGE3 reachable totals (mirror-rep) W/L/D = {mw} / {ml} / {mdraw} (reach={reach_total}) ---");
        println!("--- self-sym reachable W/L/D = {sw} / {sl} / {sdraw} (S_reach={sreach_total}) ---");
        println!("=== 照合B 表8(pseudo-reachable) W/L/D = {pw} / {pl} / {pd} ===");
        println!("    paper B 表8              = 106144078911 / 41129930509 / 695889860");
        println!("    pseudo total = {} (paper 147969899280)", pw + pl + pd);

        // Per-ply (DTM) un-folded distribution → Table A.1 (win=odd round, lose=even).
        // round = DTM = paper手数 (D1 convention). r0 = no-move terminals (DTM0 lose).
        println!("=== 照合C 表A.1 (pseudo per-手数) ===");
        println!("{:>4} {:>6} {:>18} {:>18}", "手数", "種別", "pseudo(=2m-s)", "paperA.1");
        for r in &proj {
            let (m, s, kind) = if r.round % 2 == 1 {
                (r.mwin, r.swin, "勝ち")
            } else {
                (r.mlose, r.slose, "負け")
            };
            if m == 0 {
                continue;
            }
            let pseudo = 2 * m - s;
            let paper = paper_a1(r.round);
            let tag = match paper {
                Some(p) if p == pseudo => "OK",
                Some(_) => "DIFF",
                None => "?",
            };
            println!(
                "{:>4} {:>6} {:>18} {:>18} {}",
                r.round, kind, pseudo,
                paper.map(|p| p.to_string()).unwrap_or_else(|| "-".into()),
                tag
            );
        }
        println!("    (注: r0=DTM0手詰終端は表A.1外。終端種別オフセットは層ごとに要確認)");
        println!("peakRSS={:.1}GiB", peak_rss_gib());
    }

    // -------- stage uf: full-space un-fold vs Table A.1 (residual = pure unreachable) --------
    // pseudo_full(k) = 2·full_mirror(k) − sym_full(k); residual = pseudo_full − paperA.1 = 2·unreachable(k).
    if stage == "uf" {
        let ss = if sym_path.exists() {
            load_bits(r_full, &sym_path)
        } else {
            let t = Instant::now();
            let s = self_sym_bitset(&mr);
            dump_bits(&s, &sym_path);
            eprintln!("[reachproj] self_sym built in {:.0}s", t.elapsed().as_secs_f64());
            s
        };
        let n_sym = ss.count();
        let expect_sfull = nocca::mirror_rank::counts(rows, cols).s_full;
        println!("UF self_sym(S_full)={n_sym} (expect {expect_sfull} {})", if n_sym == expect_sfull { "OK" } else { "MISMATCH" });

        // full-space per round (decided counts) + self-sym per round (project with reachable=self_sym).
        let full = nocca::inmem_retro::dtm_round_sizes(&stream_dir).expect("dtm_round_sizes");
        let t = Instant::now();
        let symrows = project_stream(&stream_dir, &ss, &ss).expect("sym scan");
        eprintln!("[reachproj] self-sym per-round scan in {:.0}s", t.elapsed().as_secs_f64());
        use std::collections::HashMap;
        let mut symw: HashMap<u32, u64> = HashMap::new();
        let mut syml: HashMap<u32, u64> = HashMap::new();
        for r in &symrows {
            symw.insert(r.round, r.mwin);
            syml.insert(r.round, r.mlose);
        }

        println!("=== 全空間un-fold (pseudo_full=2·mirror−sym) vs 表A.1 ===");
        println!("{:>4} {:>6} {:>20} {:>20} {:>16}", "手数", "種別", "pseudo_full", "paperA.1", "残差(2·到達不能)");
        let (mut tot_resid, mut tot_sym) = (0i128, 0u64);
        for (round, nw, nl) in &full {
            let (decided, sym, kind) = if round % 2 == 1 {
                (*nw, *symw.get(round).unwrap_or(&0), "勝ち")
            } else {
                (*nl, *syml.get(round).unwrap_or(&0), "負け")
            };
            if decided == 0 {
                continue;
            }
            tot_sym += sym;
            let pseudo_full = 2 * decided - sym;
            if let Some(pa) = paper_a1(*round) {
                let resid = pseudo_full as i128 - pa as i128;
                tot_resid += resid;
                if *round <= 6 || *round >= 64 || *round == 41 {
                    println!("{:>4} {:>6} {:>20} {:>20} {:>16}", round, kind, pseudo_full, pa, resid);
                }
            }
        }
        println!("Σ残差(全層) = {tot_resid}  (= 2·到達不能総数)  Σsym(全空間) = {tot_sym}");
        println!("peakRSS={:.1}GiB", peak_rss_gib());
    }
}

/// Paper B Table A.1 per-手数 counts (win on odd, lose on even). Win rows verified
/// (Σ = 106,144,078,911 = Table 8 win). Lose rows have a −3 transcription artifact
/// to resolve from the raw text; Σ even should be 41,129,930,509.
fn paper_a1(ply: u32) -> Option<u64> {
    let v: u64 = match ply {
        1 => 77_645_562_828, 2 => 22_410_730_165, 3 => 15_142_536_934, 4 => 7_707_358_885,
        5 => 2_996_378_622, 6 => 2_530_587_844, 7 => 1_793_971_952, 8 => 1_708_892_403,
        9 => 1_509_313_136, 10 => 1_085_543_075, 11 => 1_080_382_276, 12 => 855_684_894,
        13 => 992_892_748, 14 => 829_104_200, 15 => 952_307_416, 16 => 782_437_940,
        17 => 878_640_363, 18 => 718_653_702, 19 => 796_900_982, 20 => 656_328_294,
        21 => 694_050_711, 22 => 570_638_553, 23 => 568_580_405, 24 => 455_202_391,
        25 => 424_155_415, 26 => 327_678_503, 27 => 287_199_523, 28 => 215_552_713,
        29 => 176_144_437, 30 => 128_061_051, 31 => 99_613_880, 32 => 69_681_294,
        33 => 51_549_796, 34 => 36_136_187, 35 => 26_077_221, 36 => 18_510_133,
        37 => 12_725_495, 38 => 9_461_541, 39 => 6_281_721, 40 => 5_122_307,
        41 => 3_427_186, 42 => 3_122_345, 43 => 2_027_453, 44 => 2_001_316,
        45 => 1_256_933, 46 => 1_327_058, 47 => 815_882, 48 => 879_281,
        49 => 516_770, 50 => 535_828, 51 => 313_835, 52 => 305_484,
        53 => 194_950, 54 => 176_195, 55 => 123_798, 56 => 111_626,
        57 => 77_589, 58 => 65_019, 59 => 36_591, 60 => 29_351,
        61 => 14_906, 62 => 8_074, 63 => 5_305, 64 => 2_244,
        65 => 1_704, 66 => 582, 67 => 118, 68 => 28, 69 => 30,
        _ => return None,
    };
    Some(v)
}
