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
//!   reachproj <rows> <cols> <stream_dir> --work-dir DIR [--stage 1|2|3|all|uf|cand]
//!     stage 1  forward BFS → reachable.bits + census (照合A, avg moves, terminals)
//!     stage 2  self-symmetric bitset → selfsym.bits + |selfsym|, |selfsym∩reach|
//!     stage 3  stream projection → un-fold → Table 8 / Table A.1 comparison
//!     all      1→2→3 in one process (bitsets kept in RAM)
//!     uf       full-space un-fold vs Table A.1
//!     cand     clear bits of reachable.bits + their value/DTM → candidates.csv

use nocca::inmem_retro::{
    bitset_intersection_count, dump_atomic_words, load_atomic_words, project_stream,
    reachable_bfs, self_sym_bitset, BitSet,
};
use nocca::mirror_rank::MirrorRanker;
use std::io::{BufReader, BufWriter, Write};
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

/// `"B|.|WB|.|./..."` — row 0 (side-to-move's home row) first, cells left to right.
/// A cell is `.` when empty, else its stack bottom→top (`B` = side to move).
/// Separators are `|`/`/` so the field remains a single CSV column.
fn board_str(b: &nocca::varboard::VarBoard) -> String {
    let (rows, cols) = (b.rows(), b.cols());
    let key = b.key();
    let mut s = String::with_capacity(rows * cols * 2);
    for r in 0..rows {
        if r > 0 {
            s.push('/');
        }
        for c in 0..cols {
            if c > 0 {
                s.push('|');
            }
            let code = ((key >> ((r * cols + c) * 4)) & 0xF) as u8;
            if code == 0 {
                s.push('.');
                continue;
            }
            let h = 31 - (code as u32).leading_zeros();
            for i in (0..h).rev() {
                s.push(if (code >> i) & 1 == 1 { 'B' } else { 'W' });
            }
        }
    }
    s
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 3 {
        eprintln!(
            "usage: reachproj <rows> <cols> <stream_dir> --work-dir DIR \
             [--stage 1|2|3|all|uf|cand]"
        );
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

        // First retain the raw exact-reachable projection R. The D1 stream records
        // no-move terminals as round-0 losses, so they are present in these totals.
        let (mut mw, mut ml, mut sw, mut sl) = (0u64, 0u64, 0u64, 0u64);
        for r in &proj {
            mw += r.mwin;
            ml += r.mlose;
            sw += r.swin;
            sl += r.slose;
        }
        let mdraw = reach_total - mw - ml;
        let sdraw = sreach_total - sw - sl;
        println!("--- STAGE3 raw R (mirror-rep) W/L/D = {mw} / {ml} / {mdraw} (reach={reach_total}) ---");
        println!("--- raw self-sym R W/L/D = {sw} / {sl} / {sdraw} (S_reach={sreach_total}) ---");

        // Normalize exact reachability R to paper B's pseudo-reachable set P:
        //   P = (R − (N − T)) ∪ U,  T ⊂ N ⊂ R,  U ∩ R = ∅.
        // N is the round-0 no-move terminal set. T is the 15-rep subset retained as
        // `unknown` (Draw) by the authors. U is 27 Win/DTM1 + 3 Lose/DTM4 reps.
        // T and U are non-self-symmetric, so only N changes self-symmetric counts.
        let (n_reps, n_sym) = proj
            .iter()
            .find(|r| r.round == 0)
            .map(|r| (r.mlose, r.slose))
            .unwrap_or((0, 0));
        let paper_b = (rows, cols) == (6, 5);
        let (mw, ml, mdraw, sl) = if paper_b {
            assert!(ml >= n_reps && sl >= n_sym);
            println!(
                "--- normalize R→P: remove N={n_reps} (self-sym {n_sym}), retain T={PAPER_T} as Draw, \
                 add U=Win{PAPER_U_WIN}/DTM{PAPER_U_WIN_DTM}+Lose{PAPER_U_LOSE}/DTM{PAPER_U_LOSE_DTM} ---"
            );
            (
                mw + PAPER_U_WIN,
                ml - n_reps + PAPER_U_LOSE,
                mdraw + PAPER_T,
                sl - n_sym,
            )
        } else {
            println!("--- non-6×5: paper-B normalization is undefined; reporting raw R ---");
            (mw, ml, mdraw, sl)
        };

        // Un-fold: pseudo = 2*mirror - self_sym.
        let pw = 2 * mw - sw;
        let pl = 2 * ml - sl;
        let pd = 2 * mdraw - sdraw;
        let p_reps = mw + ml + mdraw;
        println!("=== 照合B pseudo W/L/D = {pw} / {pl} / {pd} (mirror-rep {mw} / {ml} / {mdraw}, total {p_reps}) ===");
        if paper_b {
            println!("    |P| = {p_reps} vs paper {PAPER_P} {}",
                if p_reps == PAPER_P { "OK" } else { "MISMATCH" });
            println!("    paper B Table 8 = {PAPER_T8_W} / {PAPER_T8_L} / {PAPER_T8_D}   {}",
                if (pw, pl, pd) == (PAPER_T8_W, PAPER_T8_L, PAPER_T8_D) { "ALL OK" } else { "DIFF" });
            println!("    pseudo total = {} (paper {PAPER_PSEUDO_TOTAL} {})", pw + pl + pd,
                if pw + pl + pd == PAPER_PSEUDO_TOTAL { "OK" } else { "DIFF" });
            assert_eq!(p_reps, PAPER_P, "paper-B folded total mismatch");
            assert_eq!((pw, pl, pd), (PAPER_T8_W, PAPER_T8_L, PAPER_T8_D),
                "paper-B Table 8 mismatch");
            assert_eq!(pw + pl + pd, PAPER_PSEUDO_TOTAL,
                "paper-B pseudo total mismatch");
        }

        // Per-ply (DTM) un-folded distribution → Table A.1 (win=odd round, lose=even).
        // Round 0 = N and is outside Table A.1. U contributes at DTM 1 and DTM 4.
        println!("=== 照合C 表A.1 (pseudo per-手数) ===");
        println!("{:>4} {:>6} {:>18} {:>18}", "手数", "種別", "pseudo(=2m-s)", "paperA.1");

        let table_csv_path = work.join("table_a1_reachable_vs_paperB.csv");
        let mut table_csv = if paper_b {
            let f = std::fs::File::create(&table_csv_path).expect("create Table A.1 CSV");
            let mut w = BufWriter::new(f);
            writeln!(w, "# Table A.1: per-ply (=DTM) distribution over paper B's P set, pseudo-reachable (mirror-unfolded) counts.").unwrap();
            writeln!(w, "# computed_pseudo = stage 3 R-to-P normalization: P = (R - (N - T)) union U.").unwrap();
            writeln!(w, "# paperB = Yamamoto-Hoki (GPW 2022) Table A.1.").unwrap();
            writeln!(w, "# diff = computed - paperB. (The GPW PDF truncates ply 48; the journal version gives 879,284.)").unwrap();
            writeln!(w, "ply,kind,computed_pseudo,paperB,diff").unwrap();
            Some(w)
        } else {
            None
        };
        let (mut compared, mut diffs, mut unchecked) = (0usize, 0usize, 0usize);
        for r in &proj {
            if r.round == 0 {
                continue;
            }
            let (m, s, kind, csv_kind) = if r.round % 2 == 1 {
                (r.mwin, r.swin, "勝ち", "win")
            } else {
                (r.mlose, r.slose, "負け", "lose")
            };
            if m == 0 {
                continue;
            }
            let u_add = if paper_b { paper_b_u_reps(r.round) } else { 0 };
            let pseudo = 2 * (m + u_add) - s;
            let paper = paper_a1(r.round);
            let tag = match paper {
                Some(p) => {
                    compared += 1;
                    if p == pseudo { "OK" } else { diffs += 1; "DIFF" }
                }
                None => { unchecked += 1; "?" }
            };
            println!(
                "{:>4} {:>6} {:>18} {:>18} {}",
                r.round, kind, pseudo,
                paper.map(|p| p.to_string()).unwrap_or_else(|| "-".into()),
                tag
            );
            if let (Some(w), Some(p)) = (table_csv.as_mut(), paper) {
                writeln!(w, "{},{},{},{},{}", r.round, csv_kind, pseudo, p,
                    pseudo as i128 - p as i128).unwrap();
            }
        }
        if paper_b {
            unchecked += PAPER_A1_ROWS.saturating_sub(compared);
            println!("    Table A.1 summary: compared={compared} DIFF={diffs} unchecked={unchecked}");
            assert_eq!(diffs, 0, "paper-B Table A.1 differences remain");
            assert_eq!(unchecked, 0, "paper-B Table A.1 rows remain unchecked");
        }
        if let Some(mut w) = table_csv {
            w.flush().unwrap();
            println!("    wrote {}", table_csv_path.display());
        }
        println!("    (注: r0=DTM0手詰終端NはTable A.1外。T={PAPER_T}代表のみDrawとしてTable 8へ算入)");
        println!("peakRSS={:.1}GiB", peak_rss_gib());
    }

    // -------- stage cand: unreachable reps + their solved value/DTM --------
    // Let R be the exact forward-reachable set, N its no-move terminals, P paper
    // B's ZDD set, T the exceptional no-move terminals retained in P, and U the
    // ZDD-member positions that are not actually reachable:
    //
    //   P = (R − (N − T)) ∪ U.
    //
    // At 6×5, |T|=15 mirror reps and |U|=30. U lies among the clear bits of
    // reachable.bits, so this stage emits every clear bit with its full-space
    // value/DTM. tools/paper_b_verification/filter_candidates then intersects
    // that pool with the authors' ZDD and recovers the 30 reps.
    if stage == "cand" {
        let t0 = Instant::now();
        let bits = load_bits(r_full, &reach_path);
        let reach_count = bits.count();
        eprintln!(
            "[cand] reachable.bits loaded: set={reach_count} clear={} ({:.1}s)",
            r_full - reach_count,
            t0.elapsed().as_secs_f64()
        );

        // Clear bits (ascending) = the candidate pool.
        let mut cands: Vec<u64> = Vec::new();
        for (wi, w) in bits.words().iter().enumerate() {
            let base = (wi as u64) << 6;
            let mut z = !w.load(std::sync::atomic::Ordering::Relaxed);
            while z != 0 {
                let b = z.trailing_zeros() as u64;
                z &= z - 1;
                if base + b < r_full {
                    cands.push(base + b);
                }
            }
        }
        eprintln!("[cand] candidate pool = {} reps", cands.len());
        assert_eq!(cands.len() as u64, r_full - reach_count);

        // Invert in place so the stream scan marks exactly the pool. Tail bits
        // past r_full also flip, but every stream rank is below r_full.
        for w in bits.words() {
            w.fetch_xor(u64::MAX, std::sync::atomic::Ordering::Relaxed);
        }

        // 0 = draw (never decided), 1 = win, 2 = lose.
        let mut kind = vec![0u8; cands.len()];
        let mut dtm = vec![0u32; cands.len()];
        let t = Instant::now();
        let mut hits = 0u64;
        nocca::inmem_retro::scan_stream_marked(&stream_dir, &bits, |rank, round, is_win| {
            let i = cands.binary_search(&rank).expect("marked rank must be in pool");
            kind[i] = if is_win { 1 } else { 2 };
            dtm[i] = round;
            hits += 1;
        })
        .expect("scan_stream_marked");
        eprintln!(
            "[cand] stream scanned in {:.1}s: {hits} of {} candidates decided (rest = draw)",
            t.elapsed().as_secs_f64(),
            cands.len()
        );

        let out_path = work.join("candidates.csv");
        let f = std::fs::File::create(&out_path).expect("create candidates.csv");
        let mut w = BufWriter::with_capacity(8 << 20, f);
        writeln!(w, "# unreachable mirror reps of {rows}x{cols} (clear bits of reachable.bits)")
            .unwrap();
        writeln!(w, "# board: row 0 (= side-to-move's home row) first, rows split by '/', cells by '|';")
            .unwrap();
        writeln!(w, "# a cell is '.' (empty) or its stack bottom->top, B = side to move, W = opponent.")
            .unwrap();
        writeln!(w, "# key_hex: VarBoard::key(), cell i in nibble i (little-endian), i = row*cols + col.")
            .unwrap();
        writeln!(w, "rank,value,dtm,selfsym,nmoves,try_win,key_hex,board").unwrap();

        let mut mv: Vec<(usize, usize)> = Vec::new();
        let mut by_kind: std::collections::BTreeMap<(u8, u32), u64> =
            std::collections::BTreeMap::new();
        let mut n_selfsym = 0u64;
        for (i, &rank) in cands.iter().enumerate() {
            let b = mr.mirror_unrank_fast2(rank);
            let selfsym = b == b.mirror();
            if selfsym {
                n_selfsym += 1;
            }
            b.moves(&mut mv, true);
            let value = match kind[i] {
                1 => "Win",
                2 => "Lose",
                _ => "Draw",
            };
            *by_kind.entry((kind[i], dtm[i])).or_insert(0) += 1;
            writeln!(
                w,
                "{rank},{value},{},{},{},{},{:032x},{}",
                dtm[i],
                u8::from(selfsym),
                mv.len(),
                u8::from(b.try_win()),
                b.key(),
                board_str(&b)
            )
            .unwrap();
        }
        w.flush().unwrap();

        println!("=== STAGE cand: unreachable reps of {rows}x{cols} ===");
        println!(
            "pool={} selfsym={n_selfsym} → {}",
            cands.len(),
            out_path.display()
        );
        println!("{:>6} {:>5} {:>12}", "value", "dtm", "reps");
        for ((k, d), n) in &by_kind {
            let v = match k {
                1 => "Win",
                2 => "Lose",
                _ => "Draw",
            };
            println!("{v:>6} {d:>5} {n:>12}");
        }
        if (rows, cols) == (6, 5) {
            let p: i128 = 73_986_754_080;
            let n: i128 = 4_459_725;
            let t: i128 = 15;
            let u = p - reach_count as i128 + n - t;
            println!(
                "paper P={p} / exact R={reach_count} / no-move N={n} / \
                 retained terminal T={t} ⇒ |U|={u} (ZDD filter expected: 30)"
            );
        }
        println!("peakRSS={:.1}GiB", peak_rss_gib());
    }

    // -------- stage uf: full-space un-fold vs Table A.1 --------
    // pseudo_full(k) = 2·full_mirror(k) − sym_full(k). The residual against
    // paper A.1 is the full-space complement of paper P at that DTM, not merely
    // the exact-reachability complement.
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
        println!("{:>4} {:>6} {:>20} {:>20} {:>16}", "手数", "種別", "pseudo_full", "paperA.1", "残差(full−P)");
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
        println!("Σ残差(全層) = {tot_resid}  (= full-space − paper P by DTM)  Σsym(全空間) = {tot_sym}");
        println!("peakRSS={:.1}GiB", peak_rss_gib());
    }
}

const PAPER_P: u64 = 73_986_754_080;
const PAPER_T: u64 = 15;
const PAPER_U_WIN: u64 = 27;
const PAPER_U_LOSE: u64 = 3;
const PAPER_U_WIN_DTM: u32 = 1;
const PAPER_U_LOSE_DTM: u32 = 4;
const PAPER_T8_W: u64 = 106_144_078_911;
const PAPER_T8_L: u64 = 41_129_930_509;
const PAPER_T8_D: u64 = 695_889_860;
const PAPER_PSEUDO_TOTAL: u64 = 147_969_899_280;
const PAPER_A1_ROWS: usize = 69;

fn paper_b_u_reps(ply: u32) -> u64 {
    match ply {
        PAPER_U_WIN_DTM => PAPER_U_WIN,
        PAPER_U_LOSE_DTM => PAPER_U_LOSE,
        _ => 0,
    }
}

/// Paper B Table A.1 per-手数 counts (win on odd, lose on even). Both columns are
/// verified against Table 8: Σodd = 106,144,078,911 and Σeven = 41,129,930,509.
/// The GPW PDF truncates ply 48; the journal version's Table A·3 gives 879,284.
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
        45 => 1_256_933, 46 => 1_327_058, 47 => 815_882, 48 => 879_284,
        49 => 516_770, 50 => 535_828, 51 => 313_835, 52 => 305_484,
        53 => 194_950, 54 => 176_195, 55 => 123_798, 56 => 111_626,
        57 => 77_589, 58 => 65_019, 59 => 36_591, 60 => 29_351,
        61 => 14_906, 62 => 8_074, 63 => 5_305, 64 => 2_244,
        65 => 1_704, 66 => 582, 67 => 118, 68 => 28, 69 => 30,
        _ => return None,
    };
    Some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paper_a1_columns_sum_to_table8() {
        let win: u64 = (1..=69).filter(|ply| ply % 2 == 1).filter_map(paper_a1).sum();
        let lose: u64 = (1..=69).filter(|ply| ply % 2 == 0).filter_map(paper_a1).sum();
        assert_eq!(win, PAPER_T8_W);
        assert_eq!(lose, PAPER_T8_L);
    }

    #[test]
    fn paper_b_normalization_reproduces_table8() {
        let (mw, ml, md) = (53_073_229_223, 20_570_027_276, 347_957_261);
        let (sw, sl, sd) = (2_379_589, 1_211_913, 24_692);
        let (n, n_sym) = (4_459_725, 7_314);
        let normalized = (
            mw + PAPER_U_WIN,
            ml - n + PAPER_U_LOSE,
            md + PAPER_T,
        );
        let normalized_sym = (sw, sl - n_sym, sd);
        assert_eq!(normalized.0 + normalized.1 + normalized.2, PAPER_P);
        assert_eq!(
            (
                2 * normalized.0 - normalized_sym.0,
                2 * normalized.1 - normalized_sym.1,
                2 * normalized.2 - normalized_sym.2,
            ),
            (PAPER_T8_W, PAPER_T8_L, PAPER_T8_D)
        );
        assert_eq!(paper_b_u_reps(1), 27);
        assert_eq!(paper_b_u_reps(4), 3);
        assert_eq!(paper_b_u_reps(2), 0);
    }

    #[test]
    fn committed_table_a1_csv_is_all_exact_and_matches_constants() {
        let csv = include_str!("../../results/table_a1_reachable_vs_paperB.csv");
        let mut rows = 0usize;
        let (mut win, mut lose) = (0u64, 0u64);
        for line in csv.lines().filter(|line| !line.is_empty() && !line.starts_with('#')) {
            if line.starts_with("ply,") {
                continue;
            }
            let fields: Vec<&str> = line.split(',').collect();
            assert_eq!(fields.len(), 5);
            let ply: u32 = fields[0].parse().unwrap();
            assert_eq!(ply, rows as u32 + 1, "CSV ply sequence");
            assert_eq!(fields[1], if ply % 2 == 1 { "win" } else { "lose" });
            let computed: u64 = fields[2].parse().unwrap();
            let paper: u64 = fields[3].parse().unwrap();
            let diff: i128 = fields[4].parse().unwrap();
            assert_eq!(computed, paper, "ply {ply}");
            assert_eq!(paper_a1(ply), Some(paper), "ply {ply}");
            assert_eq!(diff, 0, "ply {ply}");
            if ply % 2 == 1 { win += computed; } else { lose += computed; }
            rows += 1;
        }
        assert_eq!(rows, PAPER_A1_ROWS);
        assert_eq!(win, PAPER_T8_W);
        assert_eq!(lose, PAPER_T8_L);
    }
}
