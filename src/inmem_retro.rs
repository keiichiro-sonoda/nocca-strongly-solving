//! All-in-memory **2-bit-array retrograde** over mirror representatives
//! ([`crate::mirror_rank`]), for the independent 6×5 reproduction.
//!
//! INDEPENDENT codepath: this reads `varboard`/`mirror_rank` only and never
//! touches `disk_retro` / `rank` / the running disk solver. The DTM stream is
//! written to a caller-chosen directory (tests use the OS temp dir), never under
//! the disk solver's workdir.
//!
//! Design (approved):
//! * **D1-b** value array = 2 bits/entry `{00 unknown, 01 win, 10 lose}` (11 unused),
//!   32 entries per 64-bit word. DTM lives in a separate disk stream, not the array.
//! * **D1-a** Lose detection is **counter-free**: a lose candidate regenerates all
//!   its children on the fly and checks "all children Win" against the array — no
//!   per-node remaining-children counter (saves ~55 GB at 6×5).
//! * **D2** DTM stream = per round, ascending 5-byte (`R_full < 2^40`) ranks:
//!   `[round r | n_win | n_lose | win ranks… | lose ranks…]`, plus an atomically
//!   renamed `dtm.meta` round-complete marker. `D2-b` full per-state (round number
//!   = DTM), reused for resume and the move-distribution cross-check.
//! * **D3-a** parallel writers use `fetch_or` (idempotent OR of 01/10 onto a 00
//!   slot) so 2-bit word packing cannot lose updates. `D3-b` each round's decided
//!   ranks are sorted before being written ⇒ thread-count-independent byte output.
//! * **D4-a** resume = replay the DTM stream (zero steady-state overhead); the
//!   round-complete marker is the checkpoint. `D4-b` commit = stream flush →
//!   atomic `dtm.meta` rename; on restart truncate the stream to the marker.
//!
//! Round propagation (predecessor-frontier, edges regenerated, never stored). For
//! correctness/DTM equivalence with [`crate::retro::solve_ram`], each round's two
//! steps are computed against the **previous round's** array snapshot (both steps
//! read, neither writes, until both are computed) — so a node that only becomes
//! Win *this* round cannot prematurely complete a Lose parent's "all children Win"
//! test (which would shave one ply off its DTM).

use crate::mirror_rank::MirrorRanker;
use crate::varboard::VarBoard;
use rayon::prelude::*;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Value codes (also the numeric values `retro.rs` uses: WIN=1, LOSS=2).
pub const UNKNOWN: u8 = 0b00;
pub const WIN: u8 = 0b01;
pub const LOSE: u8 = 0b10;

// ============================ 2-bit value array =============================

/// `len` entries of 2 bits, packed 32 per `u64`. Writers use `fetch_or`.
pub struct ValArray {
    words: Vec<AtomicU64>,
    len: u64,
}

impl ValArray {
    pub fn new(len: u64) -> ValArray {
        let nwords = ((len + 31) / 32) as usize;
        let mut words = Vec::with_capacity(nwords);
        words.resize_with(nwords, || AtomicU64::new(0));
        ValArray { words, len }
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    #[inline]
    pub fn get(&self, i: u64) -> u8 {
        let w = (i >> 5) as usize;
        let sh = ((i & 31) * 2) as u64;
        ((self.words[w].load(Ordering::Relaxed) >> sh) & 0b11) as u8
    }

    #[inline]
    fn set(&self, i: u64, bits: u64) {
        let w = (i >> 5) as usize;
        let sh = ((i & 31) * 2) as u64;
        self.words[w].fetch_or(bits << sh, Ordering::Relaxed);
    }

    #[inline]
    pub fn set_win(&self, i: u64) {
        self.set(i, WIN as u64);
    }

    #[inline]
    pub fn set_lose(&self, i: u64) {
        self.set(i, LOSE as u64);
    }

    /// Raw little-endian word bytes — for byte-identical determinism checks.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(self.words.len() * 8);
        for w in &self.words {
            v.extend_from_slice(&w.load(Ordering::Relaxed).to_le_bytes());
        }
        v
    }

    /// Backing atomic words (for C2 streaming snapshots, no full-Vec copy).
    pub fn words(&self) -> &[AtomicU64] {
        &self.words
    }

    /// `(n_win, n_lose)` over the whole array.
    pub fn count(&self) -> (u64, u64) {
        (0..self.len)
            .into_par_iter()
            .map(|i| match self.get(i) {
                WIN => (1u64, 0u64),
                LOSE => (0u64, 1u64),
                _ => (0u64, 0u64),
            })
            .reduce(|| (0, 0), |a, b| (a.0 + b.0, a.1 + b.1))
    }
}

// ============================ 1-bit bitset =================================

/// `len` bits packed in `u64` words, atomic. Used for the bounded retrograde's
/// frontier / candidate sets (1 bit/rep = R_full/8 bytes). Parallel set-bit
/// iteration generates predecessors in chunks (bounded transient); single-thread
/// ascending iteration emits sorted ranks to the stream (determinism).
pub struct BitSet {
    words: Vec<AtomicU64>,
    len: u64,
}

impl BitSet {
    pub fn new(len: u64) -> BitSet {
        let nwords = ((len + 63) / 64) as usize;
        let mut words = Vec::with_capacity(nwords);
        words.resize_with(nwords, || AtomicU64::new(0));
        BitSet { words, len }
    }

    /// Set bit `i`; returns `true` iff it was previously clear (race-free dedup).
    #[inline]
    pub fn set(&self, i: u64) -> bool {
        let w = (i >> 6) as usize;
        let b = i & 63;
        let old = self.words[w].fetch_or(1u64 << b, Ordering::Relaxed);
        (old >> b) & 1 == 0
    }

    #[inline]
    pub fn get(&self, i: u64) -> bool {
        let w = (i >> 6) as usize;
        let b = i & 63;
        (self.words[w].load(Ordering::Relaxed) >> b) & 1 == 1
    }

    /// Clear bit `i` (atomic). Safe to call on the same word a parallel
    /// `par_for_each_set` is iterating: that iteration works on a per-word
    /// snapshot loaded before the closure runs, so concurrent clears don't
    /// perturb the in-flight word.
    #[inline]
    pub fn clear(&self, i: u64) {
        let w = (i >> 6) as usize;
        let b = i & 63;
        self.words[w].fetch_and(!(1u64 << b), Ordering::Relaxed);
    }

    pub fn words(&self) -> &[AtomicU64] {
        &self.words
    }

    pub fn clear_all(&self) {
        self.words.par_iter().for_each(|w| w.store(0, Ordering::Relaxed));
    }

    pub fn count(&self) -> u64 {
        self.words
            .par_iter()
            .map(|w| w.load(Ordering::Relaxed).count_ones() as u64)
            .sum()
    }

    /// Parallel over words: call `f(rep)` for every set bit. Order-independent;
    /// `f` must be commutative w.r.t. its side effects (we only `fetch_or` into
    /// other bitsets / value array). Bounded transient = one word's bits per task.
    pub fn par_for_each_set<F: Fn(u64) + Sync>(&self, f: F) {
        self.words.par_iter().enumerate().for_each(|(wi, w)| {
            let mut bits = w.load(Ordering::Relaxed);
            let base = (wi as u64) << 6;
            while bits != 0 {
                let b = bits.trailing_zeros() as u64;
                f(base + b);
                bits &= bits - 1;
            }
        });
    }

    /// Like [`par_for_each_set`](Self::par_for_each_set) but only over words
    /// `[w0, w1)`. Lets a round be processed in segments so C2 can checkpoint
    /// between them; order-independence (commutative `f`) is unchanged.
    pub fn par_for_each_set_range<F: Fn(u64) + Sync>(&self, w0: usize, w1: usize, f: F) {
        self.words[w0..w1].par_iter().enumerate().for_each(|(i, w)| {
            let wi = w0 + i;
            let mut bits = w.load(Ordering::Relaxed);
            let base = (wi as u64) << 6;
            while bits != 0 {
                let b = bits.trailing_zeros() as u64;
                f(base + b);
                bits &= bits - 1;
            }
        });
    }

    /// Single-thread ascending iteration over set bits (sorted output).
    pub fn for_each_set<F: FnMut(u64)>(&self, mut f: F) {
        for (wi, w) in self.words.iter().enumerate() {
            let mut bits = w.load(Ordering::Relaxed);
            let base = (wi as u64) << 6;
            while bits != 0 {
                let b = bits.trailing_zeros() as u64;
                f(base + b);
                bits &= bits - 1;
            }
        }
    }
}

// ============================== DTM stream ==================================

#[inline]
fn put5(buf: &mut Vec<u8>, x: u64) {
    buf.extend_from_slice(&x.to_le_bytes()[..5]);
}

#[inline]
fn get5(b: &[u8]) -> u64 {
    let mut a = [0u8; 8];
    a[..5].copy_from_slice(&b[..5]);
    u64::from_le_bytes(a)
}

fn stream_path(dir: &Path) -> PathBuf {
    dir.join("dtm.stream")
}
fn meta_path(dir: &Path) -> PathBuf {
    dir.join("dtm.meta")
}

/// Per-round `(round, n_win, n_lose)` parsed from a committed DTM stream's
/// headers (seeks past the rank payloads — cheap even for the 367GB 6×5 stream).
/// For D1 this lets callers verify the win=odd / lose=even parity invariant.
pub fn dtm_round_sizes(dir: &Path) -> io::Result<Vec<(u32, u64, u64)>> {
    let (_, committed) = read_meta(dir).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no marker"))?;
    let mut f = fs::File::open(stream_path(dir))?;
    let mut out = Vec::new();
    let mut pos = 0u64;
    let mut hdr = [0u8; 20];
    while pos + 20 <= committed {
        f.seek(SeekFrom::Start(pos))?;
        f.read_exact(&mut hdr)?;
        let round = u32::from_le_bytes(hdr[0..4].try_into().unwrap());
        let nw = u64::from_le_bytes(hdr[4..12].try_into().unwrap());
        let nl = u64::from_le_bytes(hdr[12..20].try_into().unwrap());
        out.push((round, nw, nl));
        pos += 20 + 5 * (nw + nl);
    }
    Ok(out)
}

/// `(last_completed_round, committed_stream_len)` if a marker exists.
pub fn read_meta(dir: &Path) -> Option<(u32, u64)> {
    let data = fs::read(meta_path(dir)).ok()?;
    if data.len() < 12 {
        return None;
    }
    let round = u32::from_le_bytes(data[0..4].try_into().unwrap());
    let len = u64::from_le_bytes(data[4..12].try_into().unwrap());
    Some((round, len))
}

// ============================ C2: array snapshots ===========================
// Periodic `val` + accumulator-bitset snapshots to a separate (SSD) directory so
// a crash inside the multi-hour round-2 lose-pass rolls back ≤ the snapshot
// interval (not the whole round), and restore reads ~28GB from SSD instead of
// replaying the ~367GB HDD stream. `set_win`/`set_lose` are monotonic
// (UNKNOWN→WIN/LOSE) and idempotent, so resume re-runs only the round's
// remaining word-segments; already-decided reps are skipped by the UNKNOWN gate.

/// Stream raw atomic words to `w` in bounded blocks (no full-Vec materialization).
pub fn dump_atomic_words<W: Write>(words: &[AtomicU64], w: &mut W) -> io::Result<()> {
    const BLK: usize = 1 << 16; // 64Ki words = 512KiB per write
    let mut buf: Vec<u8> = Vec::with_capacity(BLK * 8);
    for chunk in words.chunks(BLK) {
        buf.clear();
        for word in chunk {
            buf.extend_from_slice(&word.load(Ordering::Relaxed).to_le_bytes());
        }
        w.write_all(&buf)?;
    }
    Ok(())
}

/// Inverse of [`dump_atomic_words`]: load words back into the atomics (overwrite).
pub fn load_atomic_words<R: Read>(words: &[AtomicU64], r: &mut R) -> io::Result<()> {
    const BLK: usize = 1 << 16;
    let mut buf = vec![0u8; BLK * 8];
    let n = words.len();
    let mut idx = 0usize;
    while idx < n {
        let take = (n - idx).min(BLK);
        r.read_exact(&mut buf[..take * 8])?;
        for j in 0..take {
            let v = u64::from_le_bytes(buf[j * 8..j * 8 + 8].try_into().unwrap());
            words[idx + j].store(v, Ordering::Relaxed);
        }
        idx += take;
    }
    Ok(())
}

/// Round-2 sub-phases the cursor can point into.
const PHASE_A: u8 = 0; // build candidates (preds of round-1 winners)
const PHASE_B: u8 = 1; // filter candidates via all-children-Win

/// Production config for C2 array snapshots. `disabled()` = no snapshots (the
/// exact pre-C2 behaviour, byte-identical). `interval` between checkpoints;
/// `test_stop_after_snaps` simulates a crash N snapshots in (tests only).
#[derive(Clone)]
pub struct SnapConfig {
    pub dir: Option<PathBuf>,
    pub interval: std::time::Duration,
    pub test_stop_after_snaps: Option<u32>,
}

impl SnapConfig {
    pub fn disabled() -> SnapConfig {
        SnapConfig { dir: None, interval: std::time::Duration::ZERO, test_stop_after_snaps: None }
    }
    pub fn new(dir: impl Into<PathBuf>, interval_secs: u64) -> SnapConfig {
        SnapConfig {
            dir: Some(dir.into()),
            interval: std::time::Duration::from_secs(interval_secs),
            test_stop_after_snaps: None,
        }
    }
    fn enabled(&self) -> bool {
        self.dir.is_some()
    }
}

/// Raised by the snapshotter to unwind a *simulated* crash in tests (after the
/// configured number of snapshots). Carried as an `io::Error` so it threads
/// through `?` without changing signatures; the test recognises it by kind.
const SIM_CRASH: &str = "nocca-sim-crash";

struct SnapMeta {
    gen: u8,
    round: u32,
    phase: u8,
    cursor: u64,
    root_dist: u32,
    committed_len: u64,
}

/// Reads the "current generation" pointer and that generation's meta, if any.
fn read_snap_meta(dir: &Path) -> Option<SnapMeta> {
    let g = *fs::read(dir.join("snap.cur")).ok()?.first()?;
    let m = fs::read(dir.join(format!("snap.{g}.meta"))).ok()?;
    if m.len() < 25 {
        return None;
    }
    Some(SnapMeta {
        gen: g,
        round: u32::from_le_bytes(m[0..4].try_into().unwrap()),
        phase: m[4],
        cursor: u64::from_le_bytes(m[5..13].try_into().unwrap()),
        root_dist: u32::from_le_bytes(m[13..17].try_into().unwrap()),
        committed_len: u64::from_le_bytes(m[17..25].try_into().unwrap()),
    })
}

/// Loads a generation's val + accumulator back into the live arrays.
fn load_snapshot(dir: &Path, gen: u8, val: &ValArray, acc: &BitSet) -> io::Result<()> {
    let vf = fs::File::open(dir.join(format!("snap.{gen}.val")))?;
    load_atomic_words(val.words(), &mut io::BufReader::with_capacity(8 << 20, vf))?;
    let af = fs::File::open(dir.join(format!("snap.{gen}.acc")))?;
    load_atomic_words(acc.words(), &mut io::BufReader::with_capacity(8 << 20, af))?;
    Ok(())
}

/// Writes alternating (2-generation) val+accumulator snapshots, each fully fsync'd
/// before a single atomic `snap.cur` rename publishes it. A crash at any point
/// leaves the previous generation intact.
struct Snapshotter {
    dir: PathBuf,
    interval: std::time::Duration,
    last: std::time::Instant,
    gen: u8,
    count: u32,
    test_stop_after_snaps: Option<u32>,
}

impl Snapshotter {
    fn start(cfg: &SnapConfig, resumed_from_gen: Option<u8>) -> io::Result<Option<Snapshotter>> {
        match &cfg.dir {
            None => Ok(None),
            Some(dir) => {
                fs::create_dir_all(dir)?;
                Ok(Some(Snapshotter {
                    dir: dir.clone(),
                    interval: cfg.interval,
                    last: std::time::Instant::now(),
                    // next write targets the *other* generation than the one we
                    // resumed from, so resume never clobbers its own source.
                    gen: resumed_from_gen.unwrap_or(1),
                    count: 0,
                    test_stop_after_snaps: cfg.test_stop_after_snaps,
                }))
            }
        }
    }

    fn due(&self) -> bool {
        self.last.elapsed() >= self.interval
    }

    fn dump_file(&self, path: &Path, words: &[AtomicU64]) -> io::Result<()> {
        let f = fs::File::create(path)?;
        let mut bw = io::BufWriter::with_capacity(8 << 20, f);
        dump_atomic_words(words, &mut bw)?;
        let f = bw.into_inner().map_err(|e| e.into_error())?;
        f.sync_data()?; // C4-style: snapshot durable to platters
        Ok(())
    }

    fn write(
        &mut self,
        round: u32,
        phase: u8,
        cursor: u64,
        root_dist: u32,
        committed_len: u64,
        val: &ValArray,
        acc: &BitSet,
    ) -> io::Result<()> {
        let g = self.gen ^ 1;
        let t = std::time::Instant::now();
        self.dump_file(&self.dir.join(format!("snap.{g}.val")), val.words())?;
        self.dump_file(&self.dir.join(format!("snap.{g}.acc")), acc.words())?;
        let gib = (val.words().len() + acc.words().len()) as f64 * 8.0 / (1u64 << 30) as f64;
        eprintln!(
            "[snap] round{round} phase{phase} cursor{cursor}: val+acc {gib:.2}GiB written+fsync in {:.1}s",
            t.elapsed().as_secs_f64()
        );

        let mut meta = Vec::with_capacity(25);
        meta.extend_from_slice(&round.to_le_bytes());
        meta.push(phase);
        meta.extend_from_slice(&cursor.to_le_bytes());
        meta.extend_from_slice(&root_dist.to_le_bytes());
        meta.extend_from_slice(&committed_len.to_le_bytes());
        let mtmp = self.dir.join(format!("snap.{g}.meta.tmp"));
        {
            let mut f = fs::File::create(&mtmp)?;
            f.write_all(&meta)?;
            f.sync_all()?;
        }
        fs::rename(&mtmp, self.dir.join(format!("snap.{g}.meta")))?;

        // Publish the new generation atomically (this rename is the commit point).
        let ctmp = self.dir.join("snap.cur.tmp");
        {
            let mut f = fs::File::create(&ctmp)?;
            f.write_all(&[g])?;
            f.sync_all()?;
        }
        fs::rename(&ctmp, self.dir.join("snap.cur"))?;
        if let Ok(d) = fs::File::open(&self.dir) {
            let _ = d.sync_all();
        }

        self.gen = g;
        self.last = std::time::Instant::now();
        self.count += 1;
        if Some(self.count) == self.test_stop_after_snaps {
            return Err(io::Error::new(io::ErrorKind::Other, SIM_CRASH));
        }
        Ok(())
    }

    /// Drop snapshot files once the protected round is committed to the stream
    /// (they would be ignored anyway, but free the ~28GB×2 on SSD).
    fn clear(&self) {
        let _ = fs::remove_file(self.dir.join("snap.cur"));
        for g in 0..2u8 {
            let _ = fs::remove_file(self.dir.join(format!("snap.{g}.val")));
            let _ = fs::remove_file(self.dir.join(format!("snap.{g}.acc")));
            let _ = fs::remove_file(self.dir.join(format!("snap.{g}.meta")));
        }
    }
}

/// Appends round records and maintains the atomically-renamed marker.
struct StreamWriter {
    file: fs::File,
    dir: PathBuf,
    len: u64,
}

impl StreamWriter {
    fn create(dir: &Path) -> io::Result<StreamWriter> {
        fs::create_dir_all(dir)?;
        let file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(stream_path(dir))?;
        Ok(StreamWriter { file, dir: dir.to_path_buf(), len: 0 })
    }

    fn resume(dir: &Path, committed_len: u64) -> io::Result<StreamWriter> {
        let file = fs::OpenOptions::new().write(true).open(stream_path(dir))?;
        file.set_len(committed_len)?; // drop any half-written tail past the marker
        let mut file = file;
        file.seek(SeekFrom::End(0))?;
        Ok(StreamWriter { file, dir: dir.to_path_buf(), len: committed_len })
    }

    /// Append one round, flush, then atomically advance the marker (D4-b).
    fn commit_round(&mut self, round: u32, win: &[u64], lose: &[u64]) -> io::Result<()> {
        let mut buf = Vec::with_capacity(20 + 5 * (win.len() + lose.len()));
        buf.extend_from_slice(&round.to_le_bytes());
        buf.extend_from_slice(&(win.len() as u64).to_le_bytes());
        buf.extend_from_slice(&(lose.len() as u64).to_le_bytes());
        for &x in win {
            put5(&mut buf, x);
        }
        for &x in lose {
            put5(&mut buf, x);
        }
        self.file.write_all(&buf)?;
        self.file.flush()?;
        self.len += buf.len() as u64;

        self.write_marker(round)
    }

    /// Current committed stream length (= the marker's recorded length while no
    /// round is mid-commit). Used as the C2 snapshot's stream-consistency token.
    fn committed_len(&self) -> u64 {
        self.len
    }

    fn write_marker(&mut self, round: u32) -> io::Result<()> {
        let mut meta = Vec::with_capacity(12);
        meta.extend_from_slice(&round.to_le_bytes());
        meta.extend_from_slice(&self.len.to_le_bytes());
        let tmp = self.dir.join("dtm.meta.tmp");
        {
            // C4: marker bytes durable before the atomic rename (power-loss safe).
            let mut f = fs::File::create(&tmp)?;
            f.write_all(&meta)?;
            f.sync_all()?;
        }
        fs::rename(&tmp, meta_path(&self.dir))?;
        // C4: fsync the directory so the rename itself survives power-loss. If this
        // is lost, read_meta sees the previous round and the last round is redone
        // (bounded, never corrupting) — so a best-effort dir sync is sufficient.
        if let Ok(d) = fs::File::open(&self.dir) {
            let _ = d.sync_all();
        }
        Ok(())
    }

    /// Commit a round whose decided set is the `next` bitset: ascending Win ranks
    /// (`val==WIN`) then ascending Lose ranks (`val==LOSE`), streamed without
    /// materializing any Vec. Byte-identical to `commit_round` on the same sets.
    fn commit_round_bitset(
        &mut self,
        round: u32,
        next: &BitSet,
        val: &ValArray,
    ) -> io::Result<(u64, u64)> {
        let mut n_win = 0u64;
        let mut n_lose = 0u64;
        next.for_each_set(|i| match val.get(i) {
            WIN => n_win += 1,
            LOSE => n_lose += 1,
            _ => {}
        });
        let mut header = Vec::with_capacity(20);
        header.extend_from_slice(&round.to_le_bytes());
        header.extend_from_slice(&n_win.to_le_bytes());
        header.extend_from_slice(&n_lose.to_le_bytes());
        self.file.write_all(&header)?;
        self.len += header.len() as u64;

        const CAP: usize = 1 << 16;
        for &target in &[WIN, LOSE] {
            let mut buf: Vec<u8> = Vec::with_capacity(CAP + 8);
            for (wi, w) in next.words().iter().enumerate() {
                let mut bits = w.load(Ordering::Relaxed);
                let base = (wi as u64) << 6;
                while bits != 0 {
                    let i = base + bits.trailing_zeros() as u64;
                    bits &= bits - 1;
                    if val.get(i) == target {
                        put5(&mut buf, i);
                        if buf.len() >= CAP {
                            self.file.write_all(&buf)?;
                            self.len += buf.len() as u64;
                            buf.clear();
                        }
                    }
                }
            }
            if !buf.is_empty() {
                self.file.write_all(&buf)?;
                self.len += buf.len() as u64;
            }
        }
        self.file.flush()?;
        // C4: force the round's appended ranks to the platters before the marker,
        // so a power-loss (not just SIGKILL/OOM) recovers to this round. The fsync
        // blocks only on the residual dirty backlog (≈dirty_ratio·RAM ≤ ~12.4GB →
        // ≤ ~95s on a 7200rpm HDD, measured), not on the whole append — tail rounds are
        // ~free, only the few large early rounds pay ~70-95s (~1% of the 6×5 run).
        self.file.sync_data()?;
        self.write_marker(round)?;
        Ok((n_win, n_lose))
    }
}

/// Replay a committed stream into a fresh array + per-entry DTM, returning also
/// the last round's `(win_frontier, lose_frontier)` and that round number.
pub fn replay(dir: &Path, r_full: u64) -> io::Result<(ValArray, Vec<u32>, Vec<u64>, Vec<u64>, u32)> {
    let (last_round, committed_len) =
        read_meta(dir).expect("replay: no dtm.meta marker present");
    let mut data = fs::read(stream_path(dir))?;
    data.truncate(committed_len as usize);

    let val = ValArray::new(r_full);
    let mut dtm = vec![u32::MAX; r_full as usize];
    let mut prev_win = Vec::new();
    let mut prev_lose = Vec::new();
    let mut pos = 0usize;
    while pos < data.len() {
        let round = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap());
        pos += 4;
        let nw = u64::from_le_bytes(data[pos..pos + 8].try_into().unwrap()) as usize;
        pos += 8;
        let nl = u64::from_le_bytes(data[pos..pos + 8].try_into().unwrap()) as usize;
        pos += 8;
        let mut win = Vec::with_capacity(nw);
        for _ in 0..nw {
            let x = get5(&data[pos..pos + 5]);
            pos += 5;
            val.set_win(x);
            dtm[x as usize] = round;
            win.push(x);
        }
        let mut lose = Vec::with_capacity(nl);
        for _ in 0..nl {
            let x = get5(&data[pos..pos + 5]);
            pos += 5;
            val.set_lose(x);
            dtm[x as usize] = round;
            lose.push(x);
        }
        prev_win = win;
        prev_lose = lose;
    }
    Ok((val, dtm, prev_win, prev_lose, last_round))
}

// ================================ solver ====================================

/// DTM counting convention for the try-win terminal.
///
/// * `Dtm0` — try-win is a **DTM-0** Win leaf (the off-board move is *not* counted).
///   Win and Lose terminals coexist at distance 0, so Win/Lose nodes coexist at
///   every layer ⇒ two phases per round + the snapshot ordering. Matches
///   [`crate::retro::solve_ram`].
/// * `Depth1` — try-win is a **DTM-1** Win (the off-board move *is* counted), like
///   the animal-shogi solver. The only DTM-0 terminals are no-move losses, so the
///   fixpoint is strictly alternating (Win on odd layers, Lose on even) ⇒ one phase
///   per round and the snapshot is moot. Matches the paper-B "materialized move"
///   counting (initial-position DTM = `Dtm0` + 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Convention {
    Dtm0,
    Depth1,
}

/// Counter-free re-check accounting + final tallies.
#[derive(Debug, Clone, Copy)]
pub struct RunStats {
    pub r_full: u64,
    pub rounds: u32,
    pub n_win: u64,
    pub n_lose: u64,
    pub n_draw: u64,
    pub root_value: u8,
    /// DTM of the initial position (0 if Draw or undecided).
    pub root_dist: u32,
    /// # of unknown lose-candidates whose children were re-scanned.
    pub lose_examines: u64,
    /// # of child states generated across all Lose-step all-Win checks (the
    /// counter-free overhead).
    pub child_gens: u64,
}

/// Owns the value array + combinatorial context for one `(rows, cols)`.
pub struct Run<'a> {
    mr: &'a MirrorRanker,
    rows: usize,
    cols: usize,
    diag: bool,
    r_full: u64,
    val: ValArray,
    examines: AtomicU64,
    child_gens: AtomicU64,
}

impl<'a> Run<'a> {
    fn new(mr: &'a MirrorRanker, rows: usize, cols: usize, diag: bool) -> Run<'a> {
        let r_full = mr.r_full();
        Run {
            mr,
            rows,
            cols,
            diag,
            r_full,
            val: ValArray::new(r_full),
            examines: AtomicU64::new(0),
            child_gens: AtomicU64::new(0),
        }
    }

    pub fn val(&self) -> &ValArray {
        &self.val
    }

    /// Round 0: classify every representative as a terminal (try-win ⇒ Win,
    /// no-legal-move ⇒ Lose) or leave it unknown.
    fn terminals(&self) -> (Vec<u64>, Vec<u64>) {
        let kinds: Vec<(u64, u8)> = (0..self.r_full)
            .into_par_iter()
            .filter_map(|idx| {
                let b = self.mr.mirror_unrank(idx);
                if b.try_win() {
                    return Some((idx, WIN));
                }
                let mut mv: Vec<(usize, usize)> = Vec::new();
                b.moves(&mut mv, self.diag);
                if mv.is_empty() {
                    Some((idx, LOSE))
                } else {
                    None
                }
            })
            .collect();
        let mut win: Vec<u64> = kinds.iter().filter(|x| x.1 == WIN).map(|x| x.0).collect();
        let mut lose: Vec<u64> = kinds.iter().filter(|x| x.1 == LOSE).map(|x| x.0).collect();
        win.sort_unstable();
        lose.sort_unstable();
        (win, lose)
    }

    /// Just the try-win representatives, ascending (for `Depth1` round-1 injection
    /// on resume, where `terminals()`'s lose set is already in the stream).
    fn try_win_set(&self) -> Vec<u64> {
        let mut v: Vec<u64> = (0..self.r_full)
            .into_par_iter()
            .filter(|&idx| self.mr.mirror_unrank(idx).try_win())
            .collect();
        v.sort_unstable();
        v
    }

    /// All predecessor representatives of a frontier (deduped, ascending). Terminal
    /// (try-win) parents are filtered: they never move, so they are not real parents.
    fn preds_of(&self, frontier: &[u64]) -> Vec<u64> {
        let parts: Vec<Vec<u64>> = frontier
            .par_iter()
            .map(|&f| {
                let board = self.mr.mirror_unrank(f);
                let mut pk: Vec<u128> = Vec::new();
                board.predecessors(self.diag, true, &mut pk);
                pk.iter()
                    .map(|&k| self.mr.mirror_rank(&VarBoard::from_key(self.rows, self.cols, k)))
                    .collect()
            })
            .collect();
        let mut cand: Vec<u64> = parts.into_iter().flatten().collect();
        cand.sort_unstable();
        cand.dedup();
        cand
    }

    /// Win step: unknown predecessors of the previous round's lose-frontier.
    fn win_step(&self, prev_lose: &[u64]) -> Vec<u64> {
        let cand = self.preds_of(prev_lose);
        cand.into_iter().filter(|&p| self.val.get(p) == UNKNOWN).collect()
    }

    /// Lose step (counter-free): unknown predecessors of the previous round's
    /// win-frontier whose *every* child is already Win in the snapshot.
    fn lose_step(&self, prev_win: &[u64]) -> Vec<u64> {
        let cand = self.preds_of(prev_win);
        cand.par_iter()
            .filter_map(|&p| {
                if self.val.get(p) != UNKNOWN {
                    return None;
                }
                self.examines.fetch_add(1, Ordering::Relaxed);
                let board = self.mr.mirror_unrank(p);
                let mut mv: Vec<(usize, usize)> = Vec::new();
                board.moves(&mut mv, self.diag);
                let mut all_win = !mv.is_empty();
                let mut gens = 0u64;
                for &(f, t) in &mv {
                    gens += 1;
                    let c = self.mr.mirror_rank(&board.apply_move(f, t));
                    if self.val.get(c) != WIN {
                        all_win = false;
                        break;
                    }
                }
                self.child_gens.fetch_add(gens, Ordering::Relaxed);
                if all_win {
                    Some(p)
                } else {
                    None
                }
            })
            .collect()
    }

    fn set_wins(&self, xs: &[u64]) {
        xs.par_iter().for_each(|&i| self.val.set_win(i));
    }
    fn set_loses(&self, xs: &[u64]) {
        xs.par_iter().for_each(|&i| self.val.set_lose(i));
    }
}

/// Solve `rows × cols` fully in memory over the mirror-rep space under the `Dtm0`
/// convention (`solve_ram`-compatible), writing the DTM stream to `dir`. Resumes
/// if `dir` holds a marker. `stop_after = Some(k)` halts after committing round `k`.
pub fn run<'a>(
    mr: &'a MirrorRanker,
    rows: usize,
    cols: usize,
    diag: bool,
    dir: &Path,
    stop_after: Option<u32>,
) -> io::Result<(Run<'a>, RunStats)> {
    run_conv(mr, rows, cols, diag, dir, stop_after, Convention::Dtm0)
}

/// Like [`run`] but with an explicit [`Convention`]. `Dtm0` reproduces `run`
/// exactly (byte-identical stream/array); `Depth1` seeds the try-win terminals as
/// DTM-1 Wins (off-board move counted) ⇒ strictly alternating layers.
pub fn run_conv<'a>(
    mr: &'a MirrorRanker,
    rows: usize,
    cols: usize,
    diag: bool,
    dir: &Path,
    stop_after: Option<u32>,
    conv: Convention,
) -> io::Result<(Run<'a>, RunStats)> {
    let mut run = Run::new(mr, rows, cols, diag);
    let root = mr.mirror_rank(&VarBoard::initial(rows, cols));
    let mut root_dist = 0u32;
    // Try-win set is only needed to inject the DTM-1 Wins at round 1 (Depth1).
    let mut trywin: Vec<u64> = Vec::new();

    let (mut writer, mut prev_win, mut prev_lose, mut round) = if let Some((_, len)) = read_meta(dir)
    {
        // Resume: rebuild the array + last frontiers from the committed stream.
        let (val, dtm, pw, pl, lr) = replay(dir, run.r_full)?;
        if dtm[root as usize] != u32::MAX {
            root_dist = dtm[root as usize];
        }
        run.val = val;
        if conv == Convention::Depth1 && lr == 0 {
            trywin = run.try_win_set(); // round 1 not yet committed
        }
        let w = StreamWriter::resume(dir, len)?;
        (w, pw, pl, lr)
    } else {
        let mut w = StreamWriter::create(dir)?;
        let (tw, nomove) = run.terminals();
        match conv {
            Convention::Dtm0 => {
                w.commit_round(0, &tw, &nomove)?;
                run.set_wins(&tw);
                run.set_loses(&nomove);
                (w, tw, nomove, 0u32)
            }
            Convention::Depth1 => {
                // Round 0 = no-move losses only; try-wins enter at round 1.
                let empty: Vec<u64> = Vec::new();
                w.commit_round(0, &empty, &nomove)?;
                run.set_loses(&nomove);
                trywin = tw;
                (w, empty, nomove, 0u32)
            }
        }
    };

    loop {
        if let Some(s) = stop_after {
            if round >= s {
                break;
            }
        }
        round += 1;
        // Both steps read the round-(r-1) snapshot; neither writes until both done.
        let mut new_win = run.win_step(&prev_lose);
        let new_lose = run.lose_step(&prev_win);
        // Depth1: the try-win terminals are DTM-1 Wins (off-board move counted).
        if conv == Convention::Depth1 && round == 1 && !trywin.is_empty() {
            new_win.append(&mut trywin);
        }
        // Depth1 structural gate: Win only on odd layers, Lose only on even layers.
        if conv == Convention::Depth1 {
            if round % 2 == 1 {
                assert!(new_lose.is_empty(), "Depth1: Lose decided on odd round {round}");
            } else {
                assert!(new_win.is_empty(), "Depth1: Win decided on even round {round}");
            }
        }
        if new_win.is_empty() && new_lose.is_empty() {
            round -= 1;
            break;
        }
        let mut nw = new_win;
        let mut nl = new_lose;
        nw.sort_unstable();
        nw.dedup();
        nl.sort_unstable();
        if root_dist == 0 && (nw.binary_search(&root).is_ok() || nl.binary_search(&root).is_ok()) {
            root_dist = round;
        }
        writer.commit_round(round, &nw, &nl)?;
        run.set_wins(&nw);
        run.set_loses(&nl);
        prev_win = nw;
        prev_lose = nl;
    }

    let (n_win, n_lose) = run.val.count();
    let stats = RunStats {
        r_full: run.r_full,
        rounds: round,
        n_win,
        n_lose,
        n_draw: run.r_full - n_win - n_lose,
        root_value: run.val.get(root),
        root_dist,
        lose_examines: run.examines.load(Ordering::Relaxed),
        child_gens: run.child_gens.load(Ordering::Relaxed),
    };
    Ok((run, stats))
}

// ============================ bounded solver ===============================

/// Optimization toggles for [`run_bounded_opt`]. All `false` = the verified
/// oracle path (byte-identical to `run_conv`); each flag is a speed lever gated
/// by byte-identity with the 4×5/5×5 oracle.
#[derive(Clone, Copy, Default, Debug)]
pub struct Opts {
    /// Lever 2: generate predecessors from `self` only (the `[self, mirror]`
    /// mirror branch is redundant — `canonical_key` is mirror-invariant, so it
    /// yields the same deduped key set; verified 0 mismatch / 150k boards).
    pub fast_preds: bool,
    /// Lever 3: `mirror_unrank_fast` (estimate-window binary search) instead of
    /// `mirror_unrank` (full-range). Identical board; gated by pointwise equality.
    pub fast_unrank: bool,
    /// Lever 4 (case A): `mirror_rank_fast` (color rank only for the rank-min
    /// side). Identical index; gated by pointwise equality.
    pub fast_rank: bool,
    /// Lever 5: `mirror_unrank_fast2` (k_at coarse index) — supersedes
    /// `fast_unrank` when set. Identical board; gated by pointwise equality.
    pub fast_unrank2: bool,
    /// Lever 6 / ③a: `mirror_rank_fast3` (place_mirror table avoids the 2nd
    /// rank_placement). Requires `MirrorRanker::build_place_mirror` first.
    /// Identical index; gated by pointwise equality.
    pub fast_rank3: bool,
}

/// Dispatch the mirror_unrank variant: Lever 5 (`fast_unrank2`, coarse index) >
/// Lever 3 (`fast_unrank`, window) > base. All return the identical board.
#[inline]
fn unrank(mr: &MirrorRanker, idx: u64, opts: Opts) -> VarBoard {
    if opts.fast_unrank2 {
        mr.mirror_unrank_fast2(idx)
    } else if opts.fast_unrank {
        mr.mirror_unrank_fast(idx)
    } else {
        mr.mirror_unrank(idx)
    }
}

/// Dispatch the mirror_rank variant: ③a (`fast_rank3`, place_mirror) > Lever 4
/// (`fast_rank`, color-prune) > base. All return the identical index.
#[inline]
fn rank_of(mr: &MirrorRanker, b: &VarBoard, opts: Opts) -> u64 {
    if opts.fast_rank3 {
        mr.mirror_rank_fast3(b)
    } else if opts.fast_rank {
        mr.mirror_rank_fast(b)
    } else {
        mr.mirror_rank(b)
    }
}

/// Predecessor reps of frontier rep `f` (try-win parents filtered), via `emit`.
/// `fast` = self-only generation (Lever 2); identical output, ~half the work.
#[inline]
fn gen_preds<E: FnMut(u64)>(
    mr: &MirrorRanker,
    rows: usize,
    cols: usize,
    diag: bool,
    f: u64,
    opts: Opts,
    mut emit: E,
) {
    let board = unrank(mr, f, opts);
    let mut pk: Vec<u128> = Vec::new();
    if opts.fast_preds {
        // Self-only: replicate `predecessors`'s self branch (the mirror branch
        // produces mirror(p), whose canonical_key == canonical_key(p)).
        let q = board.reorient();
        let mut mv: Vec<(usize, usize)> = Vec::new();
        q.moves(&mut mv, diag);
        for &(src, dst) in &mv {
            let p = q.apply_raw(src, dst);
            if !p.try_win() {
                pk.push(p.canonical_key());
            }
        }
        pk.sort_unstable();
        pk.dedup();
    } else {
        board.predecessors(diag, true, &mut pk);
    }
    for k in pk {
        emit(rank_of(mr, &VarBoard::from_key(rows, cols, k), opts));
    }
}

/// Counter-free Lose test: are all children of `p` Win in `val`?
#[inline]
fn all_children_win(
    mr: &MirrorRanker,
    diag: bool,
    p: u64,
    val: &ValArray,
    child_gens: &AtomicU64,
    opts: Opts,
) -> bool {
    let board = unrank(mr, p, opts);
    let mut mv: Vec<(usize, usize)> = Vec::new();
    board.moves(&mut mv, diag);
    if mv.is_empty() {
        return false;
    }
    let mut gens = 0u64;
    let mut all = true;
    for &(f, t) in &mv {
        gens += 1;
        if val.get(rank_of(mr, &board.apply_move(f, t), opts)) != WIN {
            all = false;
            break;
        }
    }
    child_gens.fetch_add(gens, Ordering::Relaxed);
    all
}

/// Read exactly `count` 5-byte records from `r` in bounded blocks (never the whole
/// stream at once), invoking `f` per decoded rep. Keeps RAM constant (one block).
fn read_records<R: Read>(
    r: &mut R,
    count: u64,
    buf: &mut Vec<u8>,
    mut f: impl FnMut(u64),
) -> io::Result<()> {
    const REC: usize = 5;
    const BLK_RECS: usize = 1 << 20; // 1M records = 5MiB per read
    buf.resize(BLK_RECS * REC, 0);
    let mut remaining = count;
    while remaining > 0 {
        let take = remaining.min(BLK_RECS as u64) as usize;
        r.read_exact(&mut buf[..take * REC])?;
        let mut p = 0;
        for _ in 0..take {
            f(get5(&buf[p..p + REC]));
            p += REC;
        }
        remaining -= take as u64;
    }
    Ok(())
}

/// Replay the committed stream into `val` (set_win/set_lose) and leave `frontier`
/// holding only the LAST round's decided reps. Returns `(last_round, root_dist)`.
///
/// **C1**: reads the stream incrementally via `BufReader` (bounded RAM), so it
/// scales to the 6×5 367GB stream. The old `fs::read` loaded the whole file into
/// a `Vec` → OOM at 6×5 (stream ≈194GB already after round 1 ≫ free RAM). Stops
/// exactly at the marker's `committed` length, ignoring any half-written tail
/// past it (which `StreamWriter::resume` then truncates from the file).
fn replay_into(dir: &Path, val: &ValArray, frontier: &BitSet, root: u64) -> io::Result<(u32, u32)> {
    let (last_round, committed) = read_meta(dir).expect("replay_into: no marker");
    let file = fs::File::open(stream_path(dir))?;
    let mut r = io::BufReader::with_capacity(8 << 20, file);
    let mut consumed = 0u64;
    let mut root_dist = 0u32;
    let mut seen = 0u32;
    let mut hdr = [0u8; 20];
    let mut buf: Vec<u8> = Vec::new();
    while consumed < committed {
        r.read_exact(&mut hdr)?;
        let round = u32::from_le_bytes(hdr[0..4].try_into().unwrap());
        let nw = u64::from_le_bytes(hdr[4..12].try_into().unwrap());
        let nl = u64::from_le_bytes(hdr[12..20].try_into().unwrap());
        consumed += 20 + (nw + nl) * 5;
        frontier.clear_all();
        read_records(&mut r, nw, &mut buf, |x| {
            val.set_win(x);
            frontier.set(x);
            if x == root {
                root_dist = round;
            }
        })?;
        read_records(&mut r, nl, &mut buf, |x| {
            val.set_lose(x);
            frontier.set(x);
            if x == root {
                root_dist = round;
            }
        })?;
        seen = round;
    }
    debug_assert_eq!(seen, last_round);
    Ok((last_round, root_dist))
}

/// Bounded-memory retrograde producing **byte-identical** stream/array to
/// [`run_conv`], but with peak RAM = value array + 2 bitsets (`Depth1`) or 3
/// (`Dtm0`) + small chunk transient — independent of frontier size (no OOM).
///
/// Lose pass runs before Win pass and both read the round-(r-1) array, so even
/// `Dtm0` needs no extra snapshot buffer. Determinism: predecessors are OR-ed
/// into bitsets (commutative) and emitted by ascending index (sorted).
pub fn run_bounded(
    mr: &MirrorRanker,
    rows: usize,
    cols: usize,
    diag: bool,
    dir: &Path,
    conv: Convention,
    stop_after: Option<u32>,
) -> io::Result<(ValArray, RunStats)> {
    run_bounded_opt(mr, rows, cols, diag, dir, conv, stop_after, Opts::default())
}

/// Like [`run_bounded`] but with optimization [`Opts`]. `Opts::default()`
/// reproduces `run_bounded` exactly (byte-identical).
pub fn run_bounded_opt(
    mr: &MirrorRanker,
    rows: usize,
    cols: usize,
    diag: bool,
    dir: &Path,
    conv: Convention,
    stop_after: Option<u32>,
    opts: Opts,
) -> io::Result<(ValArray, RunStats)> {
    run_bounded_snap(mr, rows, cols, diag, dir, conv, stop_after, opts, SnapConfig::disabled())
}

/// Like [`run_bounded_opt`] but with **C2 array snapshots** (`snap`). With
/// `SnapConfig::disabled()` it is byte-identical to `run_bounded_opt`. When a
/// snapshot dir is given, round 2 (the heaviest D1 lose-pass, >3h at 6×5) is
/// processed in word-segments and a val+accumulator snapshot is taken every
/// `interval`; a crash there resumes from the snapshot's `(phase, cursor)`
/// instead of replaying the whole 367GB stream or redoing the whole round.
pub fn run_bounded_snap(
    mr: &MirrorRanker,
    rows: usize,
    cols: usize,
    diag: bool,
    dir: &Path,
    conv: Convention,
    stop_after: Option<u32>,
    opts: Opts,
    snap: SnapConfig,
) -> io::Result<(ValArray, RunStats)> {
    let r_full = mr.r_full();
    let root = mr.mirror_rank(&VarBoard::initial(rows, cols));
    let val = ValArray::new(r_full);
    let examines = AtomicU64::new(0);
    let child_gens = AtomicU64::new(0);
    let mut root_dist = 0u32;

    // Bitsets: Dtm0 = [prev, next, cand]; Depth1 = [prev, work].
    let nbits = if conv == Convention::Dtm0 { 3 } else { 2 };
    let bs: Vec<BitSet> = (0..nbits).map(|_| BitSet::new(r_full)).collect();
    let (mut pi, mut oi) = (0usize, 1usize); // prev / other
    let cand_i = 2usize; // Dtm0 only

    let mut writer;
    let mut round: u32;
    // C2: set when resuming mid-round 2 from a snapshot → (round, phase, cursor).
    let mut resume_into: Option<(u32, u8, u64)> = None;
    let mut resumed_from_gen: Option<u8> = None;

    // A snapshot is usable iff the stream marker is exactly one round behind it
    // (i.e. it covers the round currently being computed, not an already-committed
    // or stale one).
    let snap_usable = if snap.enabled() {
        snap.dir.as_deref().and_then(read_snap_meta).filter(|sm| match read_meta(dir) {
            Some((sr, sl)) => sr + 1 == sm.round && sl == sm.committed_len,
            None => false,
        })
    } else {
        None
    };

    if let Some(sm) = snap_usable {
        // C2 resume: load val + accumulator from SSD (~28GB) instead of replaying
        // the ~367GB HDD stream, and continue round `sm.round` from its cursor.
        let sdir = snap.dir.as_deref().unwrap();
        let t = std::time::Instant::now();
        load_snapshot(sdir, sm.gen, &val, &bs[oi])?; // accumulator → bs[oi] (round-2 `next`)
        // Reconstruct prev (= round-1 winners = {val==WIN}; round 2 adds only LOSE).
        let prev = &bs[pi];
        (0..r_full).into_par_iter().for_each(|i| {
            if val.get(i) == WIN {
                prev.set(i);
            }
        });
        eprintln!("[run] C2 restore: val+acc load + prev rebuild in {:.1}s", t.elapsed().as_secs_f64());
        root_dist = sm.root_dist;
        writer = StreamWriter::resume(dir, sm.committed_len)?;
        round = sm.round - 1; // loop does round += 1 → sm.round
        resume_into = Some((sm.round, sm.phase, sm.cursor));
        resumed_from_gen = Some(sm.gen);
        eprintln!(
            "[run] C2 resume from SNAPSHOT: round={} phase={} cursor={}",
            sm.round, sm.phase, sm.cursor
        );
    } else if read_meta(dir).is_some() {
        // Resume: rebuild val + last frontier (into bs[pi]) from the stream.
        let (lr, rd) = replay_into(dir, &val, &bs[pi], root)?;
        eprintln!("[run] resume by STREAM replay up to round {lr}");
        root_dist = rd;
        let (_, len) = read_meta(dir).unwrap();
        writer = StreamWriter::resume(dir, len)?;
        round = lr;
    } else {
        writer = StreamWriter::create(dir)?;
        // Round 0: terminal scan straight into val + frontier (no Vec).
        let prev = &bs[pi];
        (0..r_full).into_par_iter().for_each(|idx| {
            let b = unrank(mr, idx, opts);
            if b.try_win() {
                if conv == Convention::Dtm0 {
                    val.set_win(idx);
                    prev.set(idx);
                }
            } else {
                let mut mv: Vec<(usize, usize)> = Vec::new();
                b.moves(&mut mv, diag);
                if mv.is_empty() {
                    val.set_lose(idx);
                    prev.set(idx);
                }
            }
        });
        writer.commit_round_bitset(0, prev, &val)?;
        round = 0;
    }

    let mut snapper = Snapshotter::start(&snap, resumed_from_gen)?;

    loop {
        if let Some(s) = stop_after {
            if round >= s {
                break;
            }
        }
        round += 1;
        // True only for the single round we resumed into mid-way (round 2): its
        // accumulator is preloaded (don't clear) and it certainly progressed (don't
        // mistake a 0-decision *remaining* segment for the fixpoint).
        let was_resumed = resume_into.map_or(false, |(r, _, _)| r == round);
        let decided = AtomicU64::new(0);
        let next = &bs[oi];
        if !was_resumed {
            next.clear_all();
        }

        if conv == Convention::Dtm0 {
            let prev = &bs[pi];
            let cand = &bs[cand_i];
            cand.clear_all();
            // Lose pass (reads round r-1 array): candidates = preds of Win frontier.
            prev.par_for_each_set(|f| {
                if val.get(f) == WIN {
                    gen_preds(mr, rows, cols, diag, f, opts, |p| {
                        cand.set(p);
                    });
                }
            });
            cand.par_for_each_set(|p| {
                if val.get(p) == UNKNOWN {
                    examines.fetch_add(1, Ordering::Relaxed);
                    if all_children_win(mr, diag, p, &val, &child_gens, opts) {
                        val.set_lose(p);
                        if next.set(p) {
                            decided.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            });
            // Win pass: preds of Lose frontier → Win.
            prev.par_for_each_set(|f| {
                if val.get(f) == LOSE {
                    gen_preds(mr, rows, cols, diag, f, opts, |p| {
                        if val.get(p) == UNKNOWN {
                            val.set_win(p);
                            if next.set(p) {
                                decided.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    });
                }
            });
        } else if round % 2 == 1 {
            // Depth1 Win round: prev is all-Lose; preds → Win. + round-1 try-win.
            let prev = &bs[pi];
            prev.par_for_each_set(|f| {
                gen_preds(mr, rows, cols, diag, f, opts, |p| {
                    if val.get(p) == UNKNOWN {
                        val.set_win(p);
                        if next.set(p) {
                            decided.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                });
            });
            if round == 1 {
                (0..r_full).into_par_iter().for_each(|idx| {
                    if val.get(idx) == UNKNOWN && unrank(mr, idx, opts).try_win() {
                        val.set_win(idx);
                        if next.set(idx) {
                            decided.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                });
            }
        } else {
            // Depth1 Lose round: prev is all-Win; build candidates in `next`
            // (PHASE A), then filter in place to keep Lose-decided (PHASE B).
            // Round 2 (the only >3h round at 6×5) is processed in word-segments
            // so C2 can checkpoint between them and resume from `(phase, cursor)`;
            // every other even round runs un-segmented with no snapshots.
            let prev = &bs[pi];
            let nwords = next.words().len();
            let seg = (nwords / 64).max(1); // ~64 segments = fine checkpoint grain
            let do_ckpt = snapper.is_some() && round == 2;
            // Where this round's scan starts: (PHASE_A, 0) fresh, else the snapshot
            // cursor. If we resumed directly into PHASE_B, PHASE A is already done.
            let (init_phase, init_cursor) = resume_into
                .filter(|(r, _, _)| *r == round)
                .map(|(_, ph, cur)| (ph, cur as usize))
                .unwrap_or((PHASE_A, 0));

            // PHASE A: candidates = preds of prev (round-1 winners).
            if init_phase == PHASE_A {
                let mut w = init_cursor;
                while w < nwords {
                    let w1 = (w + seg).min(nwords);
                    prev.par_for_each_set_range(w, w1, |f| {
                        gen_preds(mr, rows, cols, diag, f, opts, |p| {
                            next.set(p);
                        });
                    });
                    w = w1;
                    if do_ckpt && w < nwords && snapper.as_ref().unwrap().due() {
                        let cl = writer.committed_len();
                        snapper.as_mut().unwrap().write(round, PHASE_A, w as u64, root_dist, cl, &val, next)?;
                    }
                }
            }

            // PHASE B: filter candidates → losers (keep, else clear).
            let b_start = if init_phase == PHASE_B { init_cursor } else { 0 };
            let mut w = b_start;
            while w < nwords {
                let w1 = (w + seg).min(nwords);
                next.par_for_each_set_range(w, w1, |p| {
                    if val.get(p) == UNKNOWN {
                        examines.fetch_add(1, Ordering::Relaxed);
                        if all_children_win(mr, diag, p, &val, &child_gens, opts) {
                            val.set_lose(p);
                            decided.fetch_add(1, Ordering::Relaxed);
                            return;
                        }
                    }
                    next.clear(p);
                });
                w = w1;
                if do_ckpt && w < nwords && snapper.as_ref().unwrap().due() {
                    let cl = writer.committed_len();
                    snapper.as_mut().unwrap().write(round, PHASE_B, w as u64, root_dist, cl, &val, next)?;
                }
            }
        }

        // A resumed round (round 2) certainly progressed even if its *remaining*
        // segments decided nothing — don't mistake that for the fixpoint.
        if decided.load(Ordering::Relaxed) == 0 && !was_resumed {
            round -= 1;
            break;
        }
        writer.commit_round_bitset(round, next, &val)?;
        // The round is now durable in the stream → any C2 snapshot for it is stale.
        if round == 2 {
            if let Some(s) = &snapper {
                s.clear();
            }
        }
        if root_dist == 0 && next.get(root) {
            root_dist = round;
        }
        std::mem::swap(&mut pi, &mut oi);
    }

    let (n_win, n_lose) = val.count();
    let stats = RunStats {
        r_full,
        rounds: round,
        n_win,
        n_lose,
        n_draw: r_full - n_win - n_lose,
        root_value: val.get(root),
        root_dist,
        lose_examines: examines.load(Ordering::Relaxed),
        child_gens: child_gens.load(Ordering::Relaxed),
    };
    Ok((val, stats))
}

// ===================== paper-B reachable projection =========================
// Project the full-space (R_full mirror-rep) solution onto the *reachable* set to
// compare against paper B (山本・保木2022) Table 8 (reachable W/L/D) and Table A.1
// (per-ply distribution). See nocca/RUN_6x5.md / the plan for the normalization.

/// Stats from the forward reachability BFS (mirror_rank index space).
pub struct BfsStats {
    pub reachable: u64,     // = visited.count()
    pub try_win_term: u64,  // reachable try-win terminals (not expanded)
    pub no_move_term: u64,  // reachable no-legal-move terminals
    pub nonterm: u64,       // reachable expanded (non-terminal) nodes
    pub total_moves: u64,   // Σ legal moves over non-terminal nodes (→ avg branching)
    pub max_depth: u32,     // BFS layers from the initial position
}

fn ckpt_dump_one(path: &Path, words: &[AtomicU64]) -> io::Result<()> {
    let f = fs::File::create(path)?;
    let mut bw = io::BufWriter::with_capacity(8 << 20, f);
    dump_atomic_words(words, &mut bw)?;
    let f = bw.into_inner().map_err(|e| e.into_error())?;
    f.sync_data()?;
    Ok(())
}

fn ckpt_load_one(path: &Path, words: &[AtomicU64]) -> io::Result<()> {
    let f = fs::File::open(path)?;
    load_atomic_words(words, &mut io::BufReader::with_capacity(8 << 20, f))
}

/// 2-generation, atomically-published checkpoint for the reachability BFS:
/// `visited` + `cur` (frontier) + meta (depth, the 4 stat counters), committed by
/// an atomic `bfs.ckpt` rename. Mirrors the C2 snapshot/marker discipline. A
/// layer-boundary dump bounds power-loss rollback for the multi-hour 6×5 BFS.
struct BfsCkpt {
    dir: PathBuf,
    interval: std::time::Duration,
    last: std::time::Instant,
    gen: u8,
}

impl BfsCkpt {
    fn due(&self) -> bool {
        self.last.elapsed() >= self.interval
    }

    fn write(
        &mut self,
        visited: &BitSet,
        cur: &BitSet,
        nxt: &BitSet,
        depth: u32,
        st: [u64; 4],
        cursor: u64,
    ) -> io::Result<()> {
        let g = self.gen ^ 1;
        ckpt_dump_one(&self.dir.join(format!("bfs.{g}.visited")), visited.words())?;
        ckpt_dump_one(&self.dir.join(format!("bfs.{g}.cur")), cur.words())?;
        ckpt_dump_one(&self.dir.join(format!("bfs.{g}.nxt")), nxt.words())?; // intra-layer partial
        let mut meta = Vec::with_capacity(44);
        meta.extend_from_slice(&depth.to_le_bytes());
        for v in st {
            meta.extend_from_slice(&v.to_le_bytes());
        }
        meta.extend_from_slice(&cursor.to_le_bytes()); // word index processed in layer `depth`
        let mtmp = self.dir.join(format!("bfs.{g}.meta.tmp"));
        {
            let mut f = fs::File::create(&mtmp)?;
            f.write_all(&meta)?;
            f.sync_all()?;
        }
        fs::rename(&mtmp, self.dir.join(format!("bfs.{g}.meta")))?;
        let ctmp = self.dir.join("bfs.ckpt.tmp");
        {
            let mut f = fs::File::create(&ctmp)?;
            f.write_all(&[g])?;
            f.sync_all()?;
        }
        fs::rename(&ctmp, self.dir.join("bfs.ckpt"))?;
        if let Ok(d) = fs::File::open(&self.dir) {
            let _ = d.sync_all();
        }
        self.gen = g;
        self.last = std::time::Instant::now();
        Ok(())
    }

    fn clear(&self) {
        let _ = fs::remove_file(self.dir.join("bfs.ckpt"));
        for g in 0..2u8 {
            let _ = fs::remove_file(self.dir.join(format!("bfs.{g}.visited")));
            let _ = fs::remove_file(self.dir.join(format!("bfs.{g}.cur")));
            let _ = fs::remove_file(self.dir.join(format!("bfs.{g}.nxt")));
            let _ = fs::remove_file(self.dir.join(format!("bfs.{g}.meta")));
        }
    }
}

/// Reload a BFS checkpoint into `visited`/`cur`/`nxt`; returns
/// `(depth, stats, cursor, gen)`. `cursor` = word index already processed in layer
/// `depth` (0 = at a layer boundary; >0 = mid-layer, `nxt` holds partial children).
fn bfs_ckpt_load(
    dir: &Path,
    visited: &BitSet,
    cur: &BitSet,
    nxt: &BitSet,
) -> Option<(u32, [u64; 4], u64, u8)> {
    let g = *fs::read(dir.join("bfs.ckpt")).ok()?.first()?;
    let m = fs::read(dir.join(format!("bfs.{g}.meta"))).ok()?;
    if m.len() < 44 {
        return None;
    }
    let depth = u32::from_le_bytes(m[0..4].try_into().unwrap());
    let mut st = [0u64; 4];
    for (i, s) in st.iter_mut().enumerate() {
        *s = u64::from_le_bytes(m[4 + i * 8..12 + i * 8].try_into().unwrap());
    }
    let cursor = u64::from_le_bytes(m[36..44].try_into().unwrap());
    ckpt_load_one(&dir.join(format!("bfs.{g}.visited")), visited.words()).ok()?;
    ckpt_load_one(&dir.join(format!("bfs.{g}.cur")), cur.words()).ok()?;
    ckpt_load_one(&dir.join(format!("bfs.{g}.nxt")), nxt.words()).ok()?;
    Some((depth, st, cursor, g))
}

/// Forward BFS from the initial position over the **mirror_rank** index space,
/// returning the reachable representative set as a `BitSet` (1 bit / rep) plus
/// census stats. Reuses `mirror_unrank_fast2` (L5) + `moves`/`apply_move` +
/// `mirror_rank` (same edge primitives as `all_children_win`). Terminals
/// (try-win or no legal move) are counted but not expanded.
///
/// `expand_try`: when **true** a try-win position is counted *and* expanded by its
/// in-board moves (paper-B reachability — the player may decline the try and play
/// on, so its successors are reachable). When **false** a try-win is a terminal
/// (game-reachable-under-try-type; matches the disk method's reachable count).
///
/// `ckpt = Some((work_dir, interval))` enables layer-boundary checkpointing
/// (resume the same call after a crash; rollback ≤ the heaviest layer's time).
pub fn reachable_bfs(
    mr: &MirrorRanker,
    rows: usize,
    cols: usize,
    diag: bool,
    expand_try: bool,
    ckpt: Option<(&Path, std::time::Duration)>,
) -> (BitSet, BfsStats) {
    let r_full = mr.r_full();
    let visited = BitSet::new(r_full);
    let mut cur = BitSet::new(r_full);
    let mut nxt = BitSet::new(r_full);
    let root = mr.mirror_rank(&VarBoard::initial(rows, cols));

    let try_win_term = AtomicU64::new(0);
    let no_move_term = AtomicU64::new(0);
    let nonterm = AtomicU64::new(0);
    let total_moves = AtomicU64::new(0);
    let mut depth = 0u32;

    // Checkpoint (optional): intra-layer cursor so rollback ≤ interval regardless of
    // layer size. Reload if present, else fresh. `resume_cursor`>0 = mid-layer resume.
    let mut cp: Option<BfsCkpt> = None;
    let mut resume_cursor: u64 = 0;
    if let Some((dir, interval)) = ckpt {
        let _ = fs::create_dir_all(dir);
        let start_gen = if let Some((d, st, cur_w, g)) = bfs_ckpt_load(dir, &visited, &cur, &nxt) {
            depth = d;
            try_win_term.store(st[0], Ordering::Relaxed);
            no_move_term.store(st[1], Ordering::Relaxed);
            nonterm.store(st[2], Ordering::Relaxed);
            total_moves.store(st[3], Ordering::Relaxed);
            resume_cursor = cur_w;
            eprintln!(
                "[bfs] RESUME from checkpoint: depth={d} cursor={cur_w} visited={} frontier={}",
                visited.count(), cur.count()
            );
            g
        } else {
            visited.set(root);
            cur.set(root);
            1
        };
        cp = Some(BfsCkpt { dir: dir.to_path_buf(), interval, last: std::time::Instant::now(), gen: start_gen });
    } else {
        visited.set(root);
        cur.set(root);
    }
    let t0 = std::time::Instant::now();

    loop {
        let nwords = cur.words().len();
        // Mid-layer resume keeps the partial `nxt`; a fresh layer clears it.
        let start_w = if resume_cursor > 0 {
            resume_cursor as usize
        } else {
            nxt.clear_all();
            0
        };
        resume_cursor = 0; // only the first iteration after a resume may be mid-layer
        eprintln!(
            "[bfs] layer {depth} (from word {start_w}/{nwords}): frontier={} visited={} {:.0}s",
            cur.count(), visited.count(), t0.elapsed().as_secs_f64()
        );

        // Process `cur` in word-segments so a checkpoint can land mid-layer.
        let seg = (nwords / 64).max(1);
        let mut w = start_w;
        while w < nwords {
            let w1 = (w + seg).min(nwords);
            cur.par_for_each_set_range(w, w1, |idx| {
                let b = mr.mirror_unrank_fast2(idx);
                let is_try = b.try_win();
                if is_try {
                    try_win_term.fetch_add(1, Ordering::Relaxed);
                    if !expand_try {
                        return; // try-win is terminal (game-reachable / disk-style)
                    }
                }
                let mut mv: Vec<(usize, usize)> = Vec::new();
                b.moves(&mut mv, diag);
                if mv.is_empty() {
                    if !is_try {
                        no_move_term.fetch_add(1, Ordering::Relaxed); // pure stalemate
                    }
                    return;
                }
                if !is_try {
                    nonterm.fetch_add(1, Ordering::Relaxed);
                    total_moves.fetch_add(mv.len() as u64, Ordering::Relaxed);
                }
                for &(f, t) in &mv {
                    let c = mr.mirror_rank(&b.apply_move(f, t));
                    if visited.set(c) {
                        nxt.set(c); // newly discovered → next frontier (atomic, dedup)
                    }
                }
            });
            w = w1;
            if w < nwords {
                if let Some(c) = cp.as_mut() {
                    if c.due() {
                        let st = [
                            try_win_term.load(Ordering::Relaxed),
                            no_move_term.load(Ordering::Relaxed),
                            nonterm.load(Ordering::Relaxed),
                            total_moves.load(Ordering::Relaxed),
                        ];
                        if let Err(e) = c.write(&visited, &cur, &nxt, depth, st, w as u64) {
                            eprintln!("[bfs] checkpoint write FAILED (continuing): {e}");
                        }
                    }
                }
            }
        }
        // Layer complete: `nxt` = all reps newly discovered this layer.
        if nxt.count() == 0 {
            break;
        }
        std::mem::swap(&mut cur, &mut nxt);
        depth += 1;
        // Layer-boundary checkpoint (cursor=0; `nxt` stale, cleared on resume).
        if let Some(c) = cp.as_mut() {
            if c.due() {
                let st = [
                    try_win_term.load(Ordering::Relaxed),
                    no_move_term.load(Ordering::Relaxed),
                    nonterm.load(Ordering::Relaxed),
                    total_moves.load(Ordering::Relaxed),
                ];
                if let Err(e) = c.write(&visited, &cur, &nxt, depth, st, 0) {
                    eprintln!("[bfs] checkpoint write FAILED (continuing): {e}");
                }
            }
        }
    }
    if let Some(c) = cp.as_ref() {
        c.clear(); // BFS complete → checkpoint no longer needed
    }

    let stats = BfsStats {
        reachable: visited.count(),
        try_win_term: try_win_term.load(Ordering::Relaxed),
        no_move_term: no_move_term.load(Ordering::Relaxed),
        nonterm: nonterm.load(Ordering::Relaxed),
        total_moves: total_moves.load(Ordering::Relaxed),
        max_depth: depth,
    };
    (visited, stats)
}

/// `BitSet` over R_full marking the **self-symmetric** reps (`board == mirror`).
/// Used to un-fold mirror reps to paper B's pseudo-reachable counts
/// (`pseudo = 2·mirror − self_sym`). `count()` should equal `S_full`.
pub fn self_sym_bitset(mr: &MirrorRanker) -> BitSet {
    let r_full = mr.r_full();
    let ss = BitSet::new(r_full);
    (0..r_full).into_par_iter().for_each(|idx| {
        let b = mr.mirror_unrank_fast2(idx);
        if b.key() == b.mirror().key() {
            ss.set(idx);
        }
    });
    ss
}

/// Intersection popcount `|a ∩ b|`.
pub fn bitset_intersection_count(a: &BitSet, b: &BitSet) -> u64 {
    a.words()
        .par_iter()
        .zip(b.words().par_iter())
        .map(|(x, y)| (x.load(Ordering::Relaxed) & y.load(Ordering::Relaxed)).count_ones() as u64)
        .sum()
}

/// One DTM layer's reachable projection: counts of reps decided at this round that
/// are reachable (`m*`) and additionally self-symmetric (`s*`), split win/lose.
pub struct ProjRow {
    pub round: u32,
    pub mwin: u64,
    pub mlose: u64,
    pub swin: u64,
    pub slose: u64,
}

/// Single streaming pass over the committed DTM stream: for every decided rank,
/// tally whether it is reachable (and self-symmetric), per round. Bounded RAM
/// (one block + the two bitsets supplied by the caller).
pub fn project_stream(dir: &Path, reachable: &BitSet, self_sym: &BitSet) -> io::Result<Vec<ProjRow>> {
    let (_, committed) =
        read_meta(dir).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no marker"))?;
    let file = fs::File::open(stream_path(dir))?;
    let mut r = io::BufReader::with_capacity(8 << 20, file);
    let mut consumed = 0u64;
    let mut hdr = [0u8; 20];
    let mut buf: Vec<u8> = Vec::new();
    let mut rows = Vec::new();
    while consumed < committed {
        r.read_exact(&mut hdr)?;
        let round = u32::from_le_bytes(hdr[0..4].try_into().unwrap());
        let nw = u64::from_le_bytes(hdr[4..12].try_into().unwrap());
        let nl = u64::from_le_bytes(hdr[12..20].try_into().unwrap());
        consumed += 20 + (nw + nl) * 5;
        let (mut mwin, mut swin) = (0u64, 0u64);
        read_records(&mut r, nw, &mut buf, |x| {
            if reachable.get(x) {
                mwin += 1;
                if self_sym.get(x) {
                    swin += 1;
                }
            }
        })?;
        let (mut mlose, mut slose) = (0u64, 0u64);
        read_records(&mut r, nl, &mut buf, |x| {
            if reachable.get(x) {
                mlose += 1;
                if self_sym.get(x) {
                    slose += 1;
                }
            }
        })?;
        rows.push(ProjRow { round, mwin, mlose, swin, slose });
    }
    Ok(rows)
}

// ================================ tests =====================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::retro::solve_ram;
    use crate::solver::Value;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicUsize;

    static DIRSEQ: AtomicUsize = AtomicUsize::new(0);

    fn tmpdir(tag: &str) -> PathBuf {
        let n = DIRSEQ.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!(
            "inmem_retro_{}_{}_{}_{}",
            tag,
            std::process::id(),
            n,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&d);
        d
    }

    /// Known disk+solve_ram agreed values (STATUS.md table).
    fn known(rows: usize, cols: usize) -> (Value, u32, usize, usize, usize, usize) {
        match (rows, cols) {
            (3, 2) => (Value::Win, 10, 282, 194, 72, 16),
            (3, 3) => (Value::Win, 8, 17126, 12629, 4376, 121),
            (4, 3) => (Value::Win, 16, 88093, 61883, 24343, 1867),
            (4, 4) => (Value::Win, 20, 9700372, 7097957, 2557055, 45360),
            _ => panic!("no known value for {rows}x{cols}"),
        }
    }

    /// Per-node reachable oracle keyed by mirror_rank (a transcription of
    /// `solve_ram` that also exports per-node value/dist), plus the aggregate.
    #[allow(clippy::type_complexity)]
    /// `win_term_depth` = the DTM assigned to try-win terminals: 0 for `Dtm0`
    /// (= `solve_ram`), 1 for `Depth1`. No-move losses are always DTM 0. Lose
    /// seeds are enqueued before win seeds so FIFO distance order stays monotone.
    fn oracle(
        rows: usize,
        cols: usize,
        diag: bool,
        mr: &MirrorRanker,
        win_term_depth: u32,
    ) -> (HashMap<u64, (u8, u32)>, (u8, u32, usize, usize, usize, usize)) {
        let init = VarBoard::initial(rows, cols).canonical_key();
        let mut index: HashMap<u128, u32> = HashMap::new();
        let mut nodes: Vec<u128> = vec![init];
        index.insert(init, 0);
        let mut mv: Vec<(usize, usize)> = Vec::new();
        let mut head = 0usize;
        while head < nodes.len() {
            let b = VarBoard::from_key(rows, cols, nodes[head]);
            head += 1;
            if b.try_win() {
                continue;
            }
            b.moves(&mut mv, diag);
            for &(f, t) in &mv {
                let ck = b.apply_move(f, t).canonical_key();
                if !index.contains_key(&ck) {
                    index.insert(ck, nodes.len() as u32);
                    nodes.push(ck);
                }
            }
        }
        let n = nodes.len();
        let mut children: Vec<Vec<u32>> = vec![Vec::new(); n];
        let mut seed = vec![UNKNOWN; n];
        for i in 0..n {
            let b = VarBoard::from_key(rows, cols, nodes[i]);
            if b.try_win() {
                seed[i] = WIN;
                continue;
            }
            b.moves(&mut mv, diag);
            if mv.is_empty() {
                seed[i] = LOSE;
                continue;
            }
            let mut ch: Vec<u32> = mv
                .iter()
                .map(|&(f, t)| index[&b.apply_move(f, t).canonical_key()])
                .collect();
            ch.sort_unstable();
            ch.dedup();
            children[i] = ch;
        }
        let mut preds: Vec<Vec<u32>> = vec![Vec::new(); n];
        for (i, ch) in children.iter().enumerate() {
            for &c in ch {
                preds[c as usize].push(i as u32);
            }
        }
        let out_deg: Vec<u32> = children.iter().map(|c| c.len() as u32).collect();
        let mut value = vec![UNKNOWN; n];
        let mut dist = vec![0u32; n];
        let mut win_count = vec![0u32; n];
        let mut q: std::collections::VecDeque<u32> = std::collections::VecDeque::new();
        // Lose terminals (dist 0) first, then try-win terminals (dist
        // win_term_depth) — keeps the FIFO non-decreasing in distance.
        for i in 0..n {
            if seed[i] == LOSE {
                value[i] = LOSE;
                dist[i] = 0;
                q.push_back(i as u32);
            }
        }
        for i in 0..n {
            if seed[i] == WIN {
                value[i] = WIN;
                dist[i] = win_term_depth;
                q.push_back(i as u32);
            }
        }
        while let Some(ni) = q.pop_front() {
            let ni = ni as usize;
            let d = dist[ni];
            match value[ni] {
                LOSE => {
                    for &p in &preds[ni] {
                        let p = p as usize;
                        if value[p] == UNKNOWN {
                            value[p] = WIN;
                            dist[p] = d + 1;
                            q.push_back(p as u32);
                        }
                    }
                }
                WIN => {
                    for &p in &preds[ni] {
                        let p = p as usize;
                        if value[p] == UNKNOWN {
                            win_count[p] += 1;
                            if win_count[p] == out_deg[p] {
                                value[p] = LOSE;
                                dist[p] = d + 1;
                                q.push_back(p as u32);
                            }
                        }
                    }
                }
                _ => unreachable!(),
            }
        }
        let n_win = value.iter().filter(|&&v| v == WIN).count();
        let n_lose = value.iter().filter(|&&v| v == LOSE).count();
        let n_draw = n - n_win - n_lose;
        let mut map: HashMap<u64, (u8, u32)> = HashMap::with_capacity(n);
        for i in 0..n {
            let idx = mr.mirror_rank(&VarBoard::from_key(rows, cols, nodes[i]));
            map.insert(idx, (value[i], dist[i]));
        }
        let agg = (value[0], dist[0], n, n_win, n_lose, n_draw);
        (map, agg)
    }

    fn check_correct(rows: usize, cols: usize) {
        let mr = MirrorRanker::build(rows, cols);
        let dir = tmpdir(&format!("corr_{rows}x{cols}"));
        let (run, stats) = run(&mr, rows, cols, true, &dir, None).unwrap();

        // 1) per-node reachable oracle, cross-checked against solve_ram + STATUS.
        let (omap, (orv, ord, on, ow, ol, od)) = oracle(rows, cols, true, &mr, 0);
        let sr = solve_ram(rows, cols, true);
        let sr_v = match sr.value {
            Value::Win => WIN,
            Value::Loss => LOSE,
            Value::Draw => UNKNOWN,
        };
        assert_eq!((orv, ord as usize), (sr_v, sr.dist as usize), "{rows}x{cols}: oracle vs solve_ram value/dist");
        assert_eq!((on, ow, ol, od), (sr.n_total, sr.n_win, sr.n_loss, sr.n_draw), "{rows}x{cols}: oracle vs solve_ram aggregate");
        let (kv, kd, kt, kw, kl, kdr) = known(rows, cols);
        let kv = match kv { Value::Win => WIN, Value::Loss => LOSE, Value::Draw => UNKNOWN };
        assert_eq!((sr_v, sr.dist as usize, sr.n_total, sr.n_win, sr.n_loss, sr.n_draw), (kv, kd as usize, kt, kw, kl, kdr), "{rows}x{cols}: solve_ram vs STATUS known table");

        // 2) array rebuilt from the stream is byte-identical (D4-a replay).
        let (val2, dtm, _, _, _) = replay(&dir, mr.r_full()).unwrap();
        assert_eq!(val2.to_bytes(), run.val().to_bytes(), "{rows}x{cols}: stream replay != live array");

        // 3) in-memory full-space result, projected to the reachable set, equals
        //    the oracle per node (value + DTM).
        let mut rw = 0usize;
        let mut rl = 0usize;
        for (&idx, &(ov, odst)) in &omap {
            let iv = run.val().get(idx);
            assert_eq!(iv, ov, "{rows}x{cols}: value mismatch at idx {idx}");
            if ov == WIN {
                rw += 1;
                assert_eq!(dtm[idx as usize], odst, "{rows}x{cols}: win DTM mismatch at {idx}");
            } else if ov == LOSE {
                rl += 1;
                assert_eq!(dtm[idx as usize], odst, "{rows}x{cols}: lose DTM mismatch at {idx}");
            }
        }
        assert_eq!((rw, rl), (ow, ol), "{rows}x{cols}: reachable W/L tally");

        // 4) root.
        let root = mr.mirror_rank(&VarBoard::initial(rows, cols));
        assert_eq!(run.val().get(root), sr_v, "{rows}x{cols}: root value");
        if sr_v != UNKNOWN {
            assert_eq!(dtm[root as usize], sr.dist, "{rows}x{cols}: root DTM");
        }

        // sanity: full-space tallies are a superset of the reachable ones.
        assert!(stats.n_win >= ow as u64 && stats.n_lose >= ol as u64);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn correct_small() {
        check_correct(3, 2);
        check_correct(3, 3);
        check_correct(4, 3);
    }

    #[test]
    #[ignore] // ~minutes: R_full(4×4)=14,995,596 reps scanned at round 0.
    fn correct_4x4() {
        check_correct(4, 4);
    }

    fn run_threads(rows: usize, cols: usize, threads: usize) -> (Vec<u8>, Vec<u8>) {
        let mr = MirrorRanker::build(rows, cols);
        let dir = tmpdir(&format!("det_{rows}x{cols}_{threads}"));
        let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).build().unwrap();
        let arr = pool.install(|| {
            let (r, _) = run(&mr, rows, cols, true, &dir, None).unwrap();
            r.val().to_bytes()
        });
        let stream = fs::read(stream_path(&dir)).unwrap();
        let _ = fs::remove_dir_all(&dir);
        (arr, stream)
    }

    fn check_determinism(rows: usize, cols: usize) {
        let mut arrs = Vec::new();
        let mut streams = Vec::new();
        for &t in &[1usize, 4, 16, 56] {
            let (a, s) = run_threads(rows, cols, t);
            arrs.push(a);
            streams.push(s);
        }
        for i in 1..arrs.len() {
            assert_eq!(arrs[0], arrs[i], "{rows}x{cols}: array differs across thread counts");
            assert_eq!(streams[0], streams[i], "{rows}x{cols}: DTM stream differs across thread counts");
        }
    }

    #[test]
    fn determinism_small() {
        check_determinism(3, 3);
        check_determinism(4, 3);
    }

    #[test]
    #[ignore]
    fn determinism_4x4() {
        check_determinism(4, 4);
    }

    fn check_resume(rows: usize, cols: usize, stop: u32) {
        let mr = MirrorRanker::build(rows, cols);
        // full run
        let dir_a = tmpdir(&format!("res_a_{rows}x{cols}"));
        let (run_a, _) = run(&mr, rows, cols, true, &dir_a, None).unwrap();
        let arr_a = run_a.val().to_bytes();
        let stream_a = fs::read(stream_path(&dir_a)).unwrap();

        // stop early, then resume to completion
        let dir_b = tmpdir(&format!("res_b_{rows}x{cols}"));
        let _ = run(&mr, rows, cols, true, &dir_b, Some(stop)).unwrap();
        assert!(read_meta(&dir_b).is_some());
        let (run_b, _) = run(&mr, rows, cols, true, &dir_b, None).unwrap();

        assert_eq!(arr_a, run_b.val().to_bytes(), "{rows}x{cols}: resume array != one-shot");
        assert_eq!(stream_a, fs::read(stream_path(&dir_b)).unwrap(), "{rows}x{cols}: resume stream != one-shot");
        let _ = fs::remove_dir_all(&dir_a);
        let _ = fs::remove_dir_all(&dir_b);
    }

    #[test]
    fn resume_matches_oneshot() {
        for stop in [0u32, 1, 3] {
            check_resume(3, 3, stop);
            check_resume(4, 3, stop);
        }
    }

    /// Resume coverage for the **bounded** path (`run_bounded_opt` → `replay_into`,
    /// C1 streaming reader). `check_resume`/`resume_matches_oneshot` only exercise
    /// the unbounded `run_conv`/`replay`, so the production codepath (`replay_into`)
    /// had no test until this. Both conventions, byte-identical array + stream.
    fn check_bounded_resume(rows: usize, cols: usize, conv: Convention, stop: u32) {
        let mr = MirrorRanker::build(rows, cols);
        let dir_a = tmpdir(&format!("bres_a_{rows}x{cols}"));
        let (val_a, st_a) = run_bounded(&mr, rows, cols, true, &dir_a, conv, None).unwrap();
        let arr_a = val_a.to_bytes();
        let stream_a = fs::read(stream_path(&dir_a)).unwrap();

        let dir_b = tmpdir(&format!("bres_b_{rows}x{cols}"));
        let _ = run_bounded(&mr, rows, cols, true, &dir_b, conv, Some(stop)).unwrap();
        assert!(read_meta(&dir_b).is_some());
        let (val_b, st_b) = run_bounded(&mr, rows, cols, true, &dir_b, conv, None).unwrap();

        assert_eq!(arr_a, val_b.to_bytes(), "{rows}x{cols} {conv:?} stop{stop}: array != one-shot");
        assert_eq!(
            stream_a,
            fs::read(stream_path(&dir_b)).unwrap(),
            "{rows}x{cols} {conv:?} stop{stop}: stream != one-shot"
        );
        assert_eq!(
            (st_a.root_value, st_a.root_dist, st_a.n_win, st_a.n_lose, st_a.n_draw),
            (st_b.root_value, st_b.root_dist, st_b.n_win, st_b.n_lose, st_b.n_draw),
            "{rows}x{cols} {conv:?} stop{stop}: stats != one-shot"
        );
        let _ = fs::remove_dir_all(&dir_a);
        let _ = fs::remove_dir_all(&dir_b);
    }

    #[test]
    fn bounded_resume_matches_oneshot() {
        for conv in [Convention::Dtm0, Convention::Depth1] {
            for stop in [0u32, 1, 3] {
                check_bounded_resume(3, 3, conv, stop);
                check_bounded_resume(4, 3, conv, stop);
                check_bounded_resume(4, 4, conv, stop);
            }
        }
    }

    /// C2 end-to-end: snapshot a mid-round-2 state, *simulate a crash* (after
    /// `stop_n` snapshots), then resume from the snapshot and require the value
    /// array + DTM stream + tallies to match a fresh no-snapshot run exactly.
    /// `stop_n` sweeps both PHASE A (low) and PHASE B (high) crash points.
    fn check_snapshot_resume(rows: usize, cols: usize, stop_n: u32) {
        let mr = MirrorRanker::build(rows, cols);
        let conv = Convention::Depth1; // snapshots only fire on the D1 round-2 lose-pass

        let dir_ref = tmpdir(&format!("snres_ref_{rows}x{cols}"));
        let (val_ref, st_ref) = run_bounded(&mr, rows, cols, true, &dir_ref, conv, None).unwrap();
        let arr_ref = val_ref.to_bytes();
        let stream_ref = fs::read(stream_path(&dir_ref)).unwrap();

        let dir = tmpdir(&format!("snres_{rows}x{cols}"));
        let snap_dir = tmpdir(&format!("snres_snap_{rows}x{cols}"));
        // interval 0 ⇒ a snapshot at every segment boundary; crash after stop_n.
        let mut cfg = SnapConfig::new(snap_dir.clone(), 0);
        cfg.test_stop_after_snaps = Some(stop_n);
        let res =
            run_bounded_snap(&mr, rows, cols, true, &dir, conv, None, Opts::default(), cfg);
        match res {
            Err(e) if e.to_string().contains(SIM_CRASH) => {} // crashed mid-round-2 as intended
            Ok(_) => {
                // round 2 produced < stop_n snapshots; nothing to resume — skip.
                let _ = fs::remove_dir_all(&dir_ref);
                let _ = fs::remove_dir_all(&dir);
                let _ = fs::remove_dir_all(&snap_dir);
                return;
            }
            Err(e) => panic!("{rows}x{cols} stop{stop_n}: unexpected error {e}"),
        }
        let sm = read_snap_meta(&snap_dir).expect("snapshot present after sim-crash");
        assert_eq!(sm.round, 2, "snapshot must be for round 2");

        // Resume (snapshots still enabled but a huge interval ⇒ no further snaps).
        let cfg2 = SnapConfig::new(snap_dir.clone(), 1_000_000);
        let (val_b, st_b) =
            run_bounded_snap(&mr, rows, cols, true, &dir, conv, None, Opts::default(), cfg2).unwrap();

        assert_eq!(arr_ref, val_b.to_bytes(), "{rows}x{cols} stop{stop_n} ph{}: array != fresh", sm.phase);
        assert_eq!(
            stream_ref,
            fs::read(stream_path(&dir)).unwrap(),
            "{rows}x{cols} stop{stop_n} ph{}: stream != fresh",
            sm.phase
        );
        assert_eq!(
            (st_ref.root_value, st_ref.root_dist, st_ref.n_win, st_ref.n_lose, st_ref.n_draw),
            (st_b.root_value, st_b.root_dist, st_b.n_win, st_b.n_lose, st_b.n_draw),
            "{rows}x{cols} stop{stop_n}: tallies != fresh"
        );
        let _ = fs::remove_dir_all(&dir_ref);
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&snap_dir);
    }

    #[test]
    fn snapshot_resume_matches_oneshot() {
        // 4×4 round 2 has ~64 word-segments per phase: low stop_n ⇒ crash in
        // PHASE A, high ⇒ PHASE B. (interval 0 ⇒ a snapshot every segment.)
        for stop_n in [1u32, 2, 4, 40, 90] {
            check_snapshot_resume(4, 4, stop_n);
        }
    }

    /// Heavier C2 crash→resume coverage (5×4, fsync-per-segment ⇒ minutes).
    #[test]
    #[ignore]
    fn snapshot_resume_5x4() {
        for stop_n in [1u32, 3, 40, 90, 140] {
            check_snapshot_resume(5, 4, stop_n);
        }
    }

    /// C2 Step 1: streaming snapshot dump/load round-trips exactly for both the
    /// 2-bit value array and the 1-bit frontier/accumulator bitset.
    #[test]
    fn snapshot_words_roundtrip() {
        use std::io::Cursor;
        let n = 100_003u64; // not word-aligned, exercises the tail
        let va = ValArray::new(n);
        for i in (0..n).step_by(3) {
            va.set_win(i);
        }
        for i in (1..n).step_by(7) {
            va.set_lose(i);
        }
        let mut buf = Vec::new();
        dump_atomic_words(va.words(), &mut buf).unwrap();
        let vb = ValArray::new(n);
        load_atomic_words(vb.words(), &mut Cursor::new(&buf)).unwrap();
        assert_eq!(va.to_bytes(), vb.to_bytes(), "ValArray snapshot round-trip");

        let ba = BitSet::new(n);
        for i in (0..n).step_by(5) {
            ba.set(i);
        }
        let mut buf2 = Vec::new();
        dump_atomic_words(ba.words(), &mut buf2).unwrap();
        let bb = BitSet::new(n);
        load_atomic_words(bb.words(), &mut Cursor::new(&buf2)).unwrap();
        for i in 0..n {
            assert_eq!(ba.get(i), bb.get(i), "BitSet snapshot round-trip @ {i}");
        }
    }

    /// Independent cross-check of `reachable_bfs` (mirror_rank BitSet) against a
    /// plain `HashSet<canonical_key>` forward BFS — for BOTH try-win modes. Both
    /// fold mirror pairs to one rep, so the reachable orbit COUNTS must match.
    #[test]
    fn reach_bfs_matches_hashset() {
        use std::collections::HashSet;
        fn hashset_reach(rows: usize, cols: usize, diag: bool, expand_try: bool) -> u64 {
            let root = VarBoard::initial(rows, cols);
            let mut visited: HashSet<u128> = HashSet::new();
            visited.insert(root.canonical_key());
            let mut frontier = vec![root];
            while !frontier.is_empty() {
                let mut next = Vec::new();
                for b in &frontier {
                    if b.try_win() && !expand_try {
                        continue;
                    }
                    let mut mv: Vec<(usize, usize)> = Vec::new();
                    b.moves(&mut mv, diag);
                    for &(f, t) in &mv {
                        let c = b.apply_move(f, t);
                        if visited.insert(c.canonical_key()) {
                            next.push(c);
                        }
                    }
                }
                frontier = next;
            }
            visited.len() as u64
        }
        for &(r, c) in &[(3usize, 3usize), (4, 3), (4, 4)] {
            let mr = MirrorRanker::build(r, c);
            for expand in [false, true] {
                let (vis, st) = reachable_bfs(&mr, r, c, true, expand, None);
                let hs = hashset_reach(r, c, true, expand);
                assert_eq!(
                    st.reachable, hs,
                    "{r}x{c} expand_try={expand}: mirror_rank BFS {} != hashset {hs}",
                    st.reachable
                );
                assert_eq!(vis.count(), st.reachable, "{r}x{c}: bitset count mismatch");
            }
        }
    }

    /// Counter-free re-check cost (verification 3): print measured totals so the
    /// 6×5 extrapolation can be recorded. Run with `--nocapture`.
    #[test]
    fn recheck_cost() {
        for &(r, c) in &[(3usize, 2usize), (3, 3), (4, 3)] {
            let mr = MirrorRanker::build(r, c);
            let dir = tmpdir(&format!("cost_{r}x{c}"));
            let (_, st) = run(&mr, r, c, true, &dir, None).unwrap();
            println!(
                "COST {r}x{c}: R_full={} rounds={} n_win={} n_lose={} n_draw={} lose_examines={} child_gens={} gens_per_rep={:.4}",
                st.r_full, st.rounds, st.n_win, st.n_lose, st.n_draw,
                st.lose_examines, st.child_gens,
                st.child_gens as f64 / st.r_full as f64
            );
            let _ = fs::remove_dir_all(&dir);
        }
    }

    /// Single-thread throughput of one counter-free child-generation
    /// (`apply_move` + `mirror_rank` + array read) — the unit of the Lose-step
    /// re-check cost. Run with `--nocapture` to get ns/op for the 6×5 estimate.
    #[test]
    #[ignore]
    fn childgen_throughput() {
        let (rows, cols) = (4usize, 4usize);
        let mr = MirrorRanker::build(rows, cols);
        let val = ValArray::new(mr.r_full());
        // sample a spread of representatives and sum their child-gen work
        let n_reps = 200_000u64;
        let step = (mr.r_full() / n_reps).max(1);
        let mut mv: Vec<(usize, usize)> = Vec::new();
        let mut gens: u64 = 0;
        let mut sink: u64 = 0;
        let t0 = std::time::Instant::now();
        let mut idx = 0u64;
        while idx < mr.r_full() {
            let b = mr.mirror_unrank(idx);
            b.moves(&mut mv, true);
            for &(f, t) in &mv {
                let c = mr.mirror_rank(&b.apply_move(f, t));
                sink ^= val.get(c) as u64 ^ c;
                gens += 1;
            }
            idx += step;
        }
        let el = t0.elapsed().as_secs_f64();
        println!(
            "THROUGHPUT 4x4 1-thread: child_gens={} elapsed={:.3}s ns/gen={:.1} (sink={})",
            gens,
            el,
            el * 1e9 / gens as f64,
            sink
        );
    }

    #[test]
    #[ignore]
    fn recheck_cost_4x4() {
        let (r, c) = (4usize, 4usize);
        let mr = MirrorRanker::build(r, c);
        let dir = tmpdir("cost_4x4");
        let (_, st) = run(&mr, r, c, true, &dir, None).unwrap();
        println!(
            "COST {r}x{c}: R_full={} rounds={} n_win={} n_lose={} n_draw={} lose_examines={} child_gens={} gens_per_rep={:.4}",
            st.r_full, st.rounds, st.n_win, st.n_lose, st.n_draw,
            st.lose_examines, st.child_gens,
            st.child_gens as f64 / st.r_full as f64
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// **NAIVE ordering** (the broken variant the snapshot avoids): within each
    /// round, write the Win step's results FIRST, then run the Lose step reading
    /// those same-round Win writes. Returns `(value bytes, per-node DTM)`. This is
    /// single-threaded — the bug is a sequencing issue, not a data race.
    fn solve_naive(mr: &MirrorRanker, rows: usize, cols: usize, diag: bool) -> (Vec<u8>, Vec<u32>) {
        let run = Run::new(mr, rows, cols, diag);
        let r_full = mr.r_full();
        let mut dtm = vec![u32::MAX; r_full as usize];
        let (w0, l0) = run.terminals();
        run.set_wins(&w0);
        run.set_loses(&l0);
        for &i in w0.iter().chain(l0.iter()) {
            dtm[i as usize] = 0;
        }
        let mut prev_win = w0;
        let mut prev_lose = l0;
        let mut round = 0u32;
        loop {
            round += 1;
            let new_win = run.win_step(&prev_lose);
            run.set_wins(&new_win); // <-- write BEFORE lose_step (the bug)
            for &i in &new_win {
                dtm[i as usize] = round;
            }
            let new_lose = run.lose_step(&prev_win); // reads array incl. this round's Win
            run.set_loses(&new_lose);
            for &i in &new_lose {
                dtm[i as usize] = round;
            }
            if new_win.is_empty() && new_lose.is_empty() {
                break;
            }
            prev_win = new_win;
            prev_lose = new_lose;
        }
        (run.val().to_bytes(), dtm)
    }

    /// Empirically demonstrate that the naive ordering keeps VALUES correct but
    /// produces WRONG DTM (shaved by ≥1 ply), proving the snapshot is a
    /// sequential-logic requirement (reproduces at a single thread).
    /// Runs naive vs correct on one size; asserts VALUES are unaffected and
    /// returns the number of reachable nodes whose DTM the naive ordering
    /// corrupts (always shaved, never inflated).
    fn naive_dtm_diffs(rows: usize, cols: usize) -> usize {
        let mr = MirrorRanker::build(rows, cols);
        let dir = tmpdir(&format!("naive_{rows}x{cols}"));
        let (run_ok, _) = run(&mr, rows, cols, true, &dir, None).unwrap();
        let (_, dtm_ok, _, _, _) = replay(&dir, mr.r_full()).unwrap();
        let ok_bytes = run_ok.val().to_bytes();
        let (naive_bytes, dtm_naive) = solve_naive(&mr, rows, cols, true);
        let (omap, _) = oracle(rows, cols, true, &mr, 0);

        // VALUE: naive must still be byte-identical (the win/lose partition is
        // unaffected; ok_bytes is verified against the oracle in `correct_small`).
        assert_eq!(naive_bytes, ok_bytes, "{rows}x{cols}: naive changed the value array");

        let mut diffs = 0usize;
        let mut max_short = 0i64;
        let mut first = None;
        for (&idx, &(ov, od)) in &omap {
            if ov == UNKNOWN {
                continue; // draws carry no DTM
            }
            let i = idx as usize;
            assert_eq!(dtm_ok[i], od, "{rows}x{cols}: snapshot DTM != oracle at {idx}");
            if dtm_naive[i] != od {
                diffs += 1;
                let short = od as i64 - dtm_naive[i] as i64;
                assert!(short >= 1, "{rows}x{cols}: naive DTM inflated at {idx}");
                max_short = max_short.max(short);
                if first.is_none() {
                    first = Some((ov, od, dtm_naive[i]));
                }
            }
        }
        let root = mr.mirror_rank(&VarBoard::initial(rows, cols));
        println!(
            "NAIVE {rows}x{cols}: dtm_diffs={diffs} max_ply_short={max_short} root_dtm correct={} naive={} first_diff(val,ok,naive)={:?}",
            dtm_ok[root as usize], dtm_naive[root as usize], first
        );
        let _ = fs::remove_dir_all(&dir);
        diffs
    }

    /// Empirically: the naive write-Win-then-run-Lose ordering keeps VALUES exact
    /// but CORRUPTS DTM (shaved ≥1 ply), proving the snapshot is a sequential-logic
    /// requirement. (3×2 is too small to contain the motif; it appears from 3×3 up.)
    #[test]
    fn naive_ordering_breaks_dtm() {
        assert_eq!(naive_dtm_diffs(3, 2), 0); // too small: no divergence
        let d33 = naive_dtm_diffs(3, 3);
        let d43 = naive_dtm_diffs(4, 3);
        assert!(d33 > 0 && d43 > 0, "naive ordering must corrupt DTM from 3×3 up");
    }

    #[test]
    #[ignore] // ~minute: builds the 4×4 reachable oracle + two full solves.
    fn naive_ordering_breaks_dtm_4x4() {
        assert!(naive_dtm_diffs(4, 4) > 0);
    }

    // ===================== Depth1 (手数1規約) verification ====================

    /// Depth1 correctness: per-node value+DTM vs the Depth1 oracle, full-space
    /// Win=odd/Lose=even parity gate, value partition == solve_ram, and root
    /// DTM == solve_ram(D0) + 1 (initial position is try-decided).
    fn check_d1(rows: usize, cols: usize) {
        let mr = MirrorRanker::build(rows, cols);
        let dir = tmpdir(&format!("d1_{rows}x{cols}"));
        let (run, stats) =
            run_conv(&mr, rows, cols, true, &dir, None, Convention::Depth1).unwrap();

        // Depth1 reachable oracle (try-win seeded at DTM 1) + solve_ram (D0).
        let (omap, (orv, ord, on, ow, ol, od)) = oracle(rows, cols, true, &mr, 1);
        let sr = solve_ram(rows, cols, true);
        let sr_v = match sr.value {
            Value::Win => WIN,
            Value::Loss => LOSE,
            Value::Draw => UNKNOWN,
        };
        // Value partition is convention-independent ⇒ identical to solve_ram.
        assert_eq!(
            (on, ow, ol, od),
            (sr.n_total, sr.n_win, sr.n_loss, sr.n_draw),
            "{rows}x{cols}: D1 oracle W/L/D vs solve_ram"
        );
        assert_eq!(orv, sr_v, "{rows}x{cols}: D1 root value");

        // Stream replay byte-identical, then per-node value+DTM vs oracle.
        let (val2, dtm, _, _, _) = replay(&dir, mr.r_full()).unwrap();
        assert_eq!(val2.to_bytes(), run.val().to_bytes(), "{rows}x{cols}: D1 replay");
        let (mut rw, mut rl) = (0usize, 0usize);
        for (&idx, &(ov, odst)) in &omap {
            assert_eq!(run.val().get(idx), ov, "{rows}x{cols}: D1 value at {idx}");
            if ov == WIN {
                rw += 1;
                assert_eq!(dtm[idx as usize], odst, "{rows}x{cols}: D1 win DTM at {idx}");
            } else if ov == LOSE {
                rl += 1;
                assert_eq!(dtm[idx as usize], odst, "{rows}x{cols}: D1 lose DTM at {idx}");
            }
        }
        assert_eq!((rw, rl), (ow, ol), "{rows}x{cols}: D1 reachable W/L tally");

        // Structural parity gate over the WHOLE R_full: Win⇒odd, Lose⇒even.
        for idx in 0..mr.r_full() {
            match run.val().get(idx) {
                WIN => assert!(
                    dtm[idx as usize] % 2 == 1,
                    "{rows}x{cols}: D1 Win at {idx} has even DTM {}",
                    dtm[idx as usize]
                ),
                LOSE => assert!(
                    dtm[idx as usize] % 2 == 0,
                    "{rows}x{cols}: D1 Lose at {idx} has odd DTM {}",
                    dtm[idx as usize]
                ),
                _ => {}
            }
        }

        // Root: value matches; DTM is exactly the D0 distance + 1.
        assert_eq!(stats.root_value, sr_v, "{rows}x{cols}: D1 root value (stats)");
        assert_eq!(ord, stats.root_dist, "{rows}x{cols}: D1 root DTM oracle vs run");
        if sr_v == WIN {
            assert_eq!(stats.root_dist, sr.dist + 1, "{rows}x{cols}: D1 root DTM = D0 + 1");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn correct_d1_small() {
        check_d1(3, 2);
        check_d1(3, 3);
        check_d1(4, 3);
    }

    #[test]
    #[ignore]
    fn correct_d1_4x4() {
        check_d1(4, 4);
    }

    fn run_threads_conv(rows: usize, cols: usize, threads: usize, conv: Convention) -> (Vec<u8>, Vec<u8>) {
        let mr = MirrorRanker::build(rows, cols);
        let dir = tmpdir(&format!("detc_{rows}x{cols}_{threads}"));
        let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).build().unwrap();
        let arr = pool.install(|| {
            let (r, _) = run_conv(&mr, rows, cols, true, &dir, None, conv).unwrap();
            r.val().to_bytes()
        });
        let stream = fs::read(stream_path(&dir)).unwrap();
        let _ = fs::remove_dir_all(&dir);
        (arr, stream)
    }

    fn check_determinism_d1(rows: usize, cols: usize) {
        let mut arrs = Vec::new();
        let mut streams = Vec::new();
        for &t in &[1usize, 4, 16, 56] {
            let (a, s) = run_threads_conv(rows, cols, t, Convention::Depth1);
            arrs.push(a);
            streams.push(s);
        }
        for i in 1..arrs.len() {
            assert_eq!(arrs[0], arrs[i], "{rows}x{cols}: D1 array differs across thread counts");
            assert_eq!(streams[0], streams[i], "{rows}x{cols}: D1 stream differs across thread counts");
        }
    }

    #[test]
    fn determinism_d1_small() {
        check_determinism_d1(3, 3);
        check_determinism_d1(4, 3);
    }

    #[test]
    #[ignore]
    fn determinism_d1_4x4() {
        check_determinism_d1(4, 4);
    }

    /// Naive ordering under a given convention (write Win before computing Lose).
    fn solve_naive_conv(
        mr: &MirrorRanker,
        rows: usize,
        cols: usize,
        diag: bool,
        conv: Convention,
    ) -> Vec<u32> {
        let run = Run::new(mr, rows, cols, diag);
        let r_full = mr.r_full();
        let mut dtm = vec![u32::MAX; r_full as usize];
        let (tw, nomove) = run.terminals();
        let mut trywin = Vec::new();
        let (mut prev_win, mut prev_lose) = match conv {
            Convention::Dtm0 => {
                run.set_wins(&tw);
                run.set_loses(&nomove);
                for &i in tw.iter().chain(nomove.iter()) {
                    dtm[i as usize] = 0;
                }
                (tw, nomove)
            }
            Convention::Depth1 => {
                run.set_loses(&nomove);
                for &i in &nomove {
                    dtm[i as usize] = 0;
                }
                trywin = tw;
                (Vec::new(), nomove)
            }
        };
        let mut round = 0u32;
        loop {
            round += 1;
            let mut new_win = run.win_step(&prev_lose);
            if conv == Convention::Depth1 && round == 1 && !trywin.is_empty() {
                new_win.append(&mut trywin);
                new_win.sort_unstable();
                new_win.dedup();
            }
            run.set_wins(&new_win); // NAIVE: write Win BEFORE lose_step
            for &i in &new_win {
                dtm[i as usize] = round;
            }
            let new_lose = run.lose_step(&prev_win); // reads array incl. this round's Win
            run.set_loses(&new_lose);
            for &i in &new_lose {
                dtm[i as usize] = round;
            }
            if new_win.is_empty() && new_lose.is_empty() {
                break;
            }
            prev_win = new_win;
            prev_lose = new_lose;
        }
        dtm
    }

    /// Under Depth1 the layers strictly alternate (one phase per round), so the
    /// naive ordering CANNOT corrupt DTM — confirming the snapshot is unnecessary
    /// for this convention (sharp contrast with `naive_ordering_breaks_dtm`).
    #[test]
    fn d1_snapshot_not_needed() {
        for &(r, c) in &[(3usize, 2usize), (3, 3), (4, 3)] {
            let mr = MirrorRanker::build(r, c);
            let dir = tmpdir(&format!("d1snap_{r}x{c}"));
            let _ = run_conv(&mr, r, c, true, &dir, None, Convention::Depth1).unwrap();
            let (_, dtm_ok, _, _, _) = replay(&dir, mr.r_full()).unwrap();
            let dtm_naive = solve_naive_conv(&mr, r, c, true, Convention::Depth1);
            let (omap, _) = oracle(r, c, true, &mr, 1);
            let mut diffs = 0usize;
            for (&idx, &(ov, _)) in &omap {
                if ov != UNKNOWN && dtm_naive[idx as usize] != dtm_ok[idx as usize] {
                    diffs += 1;
                }
            }
            println!("D1 SNAPSHOT-FREE {r}x{c}: naive_vs_correct_dtm_diffs={diffs}");
            assert_eq!(diffs, 0, "{r}x{c}: Depth1 naive ordering must not corrupt DTM");
            let _ = fs::remove_dir_all(&dir);
        }
    }

    /// Q3 characterization: distribution of (D1 − D0) DTM offsets over the
    /// reachable set, split by Win/Lose, plus the count of nodes whose offset is
    /// NOT a clean 0/1 bit (a proxy for the optimal line switching conventions).
    fn report_offset(rows: usize, cols: usize) {
        let mr = MirrorRanker::build(rows, cols);
        let (omap0, _) = oracle(rows, cols, true, &mr, 0);
        let (omap1, _) = oracle(rows, cols, true, &mr, 1);
        let mut win_off: std::collections::BTreeMap<i64, usize> = std::collections::BTreeMap::new();
        let mut lose_off: std::collections::BTreeMap<i64, usize> = std::collections::BTreeMap::new();
        let mut switched = 0usize;
        for (&idx, &(v0, d0)) in &omap0 {
            let (v1, d1) = omap1[&idx];
            assert_eq!(v0, v1, "{rows}x{cols}: value differs across conventions at {idx}");
            if v0 == UNKNOWN {
                continue;
            }
            let off = d1 as i64 - d0 as i64;
            if v0 == WIN {
                *win_off.entry(off).or_insert(0) += 1;
            } else {
                *lose_off.entry(off).or_insert(0) += 1;
            }
            if off != 0 && off != 1 {
                switched += 1;
            }
        }
        println!(
            "OFFSET {rows}x{cols}: win{:?} lose{:?} non01_offsets={switched}",
            win_off, lose_off
        );
    }

    #[test]
    fn d1_d0_offset_small() {
        report_offset(3, 2);
        report_offset(3, 3);
        report_offset(4, 3);
    }

    #[test]
    #[ignore]
    fn d1_d0_offset_4x4() {
        report_offset(4, 4);
    }

    // ===================== 4×5 full runs (SSD, ignored) ======================
    //
    // n_win/n_lose/n_draw are FULL-SPACE (R_full), NOT the reachable counts;
    // the published 4×5 result is the root (Win/DTM22 for D0, Win/DTM23 for D1),
    // which is asserted.

    fn run_4x5(conv: Convention, expect_root_dtm: u32, tag: &str) {
        let mr = MirrorRanker::build(4, 5);
        let dir = PathBuf::from(format!("/tmp/nocca_inmem4x5/{tag}"));
        let _ = fs::remove_dir_all(&dir);
        let (_, st) = run_conv(&mr, 4, 5, true, &dir, None, conv).unwrap();
        println!(
            "4x5 {tag}: R_full={} rounds={} fullspace[W={} L={} D={}] root_val={} root_dtm={} lose_examines={} child_gens={} gens/rep={:.4}",
            st.r_full, st.rounds, st.n_win, st.n_lose, st.n_draw,
            st.root_value, st.root_dist, st.lose_examines, st.child_gens,
            st.child_gens as f64 / st.r_full as f64
        );
        assert_eq!(st.root_value, WIN, "4x5 {tag}: root must be Win");
        assert_eq!(st.root_dist, expect_root_dtm, "4x5 {tag}: root DTM");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore]
    fn run_4x5_d0() {
        run_4x5(Convention::Dtm0, 22, "d0");
    }

    #[test]
    #[ignore]
    fn run_4x5_d1() {
        run_4x5(Convention::Depth1, 23, "d1");
    }

    // ================== memory-budget measurements (for 6×5) =================

    /// Memory-safe terminal census over the full R_full space (counters only,
    /// no Vec materialization). Returns `(try_win, no_move, r_full)`.
    fn count_terminals(mr: &MirrorRanker, diag: bool) -> (u64, u64, u64) {
        let (tw, nm) = (0..mr.r_full())
            .into_par_iter()
            .map(|idx| {
                let b = mr.mirror_unrank(idx);
                if b.try_win() {
                    (1u64, 0u64)
                } else {
                    let mut mv: Vec<(usize, usize)> = Vec::new();
                    b.moves(&mut mv, diag);
                    if mv.is_empty() {
                        (0u64, 1u64)
                    } else {
                        (0u64, 0u64)
                    }
                }
            })
            .reduce(|| (0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
        (tw, nm, mr.r_full())
    }

    /// Parse only the per-round headers of a committed DTM stream (seeks past the
    /// rank payloads): `(round, n_win, n_lose)` for each round.
    fn stream_round_sizes(dir: &Path) -> Vec<(u32, u64, u64)> {
        use std::io::Read;
        let (_, committed) = read_meta(dir).unwrap();
        let mut f = fs::File::open(stream_path(dir)).unwrap();
        let mut out = Vec::new();
        let mut pos = 0u64;
        let mut hdr = [0u8; 20];
        while pos + 20 <= committed {
            f.seek(SeekFrom::Start(pos)).unwrap();
            f.read_exact(&mut hdr).unwrap();
            let round = u32::from_le_bytes(hdr[0..4].try_into().unwrap());
            let nw = u64::from_le_bytes(hdr[4..12].try_into().unwrap());
            let nl = u64::from_le_bytes(hdr[12..20].try_into().unwrap());
            out.push((round, nw, nl));
            pos += 20 + 5 * (nw + nl);
        }
        out
    }

    /// Terminal census + per-round frontier profile, for the 6×5 memory budget.
    /// Run a subset via the size filter env `NOCCA_MEMPROF_SIZES` if needed.
    #[test]
    #[ignore]
    fn mem_profile() {
        for &(r, c) in &[(3usize, 2usize), (3, 3), (4, 3), (4, 4), (4, 5)] {
            let mr = MirrorRanker::build(r, c);
            let (tw, nm, rf) = count_terminals(&mr, true);
            println!(
                "TERM {r}x{c}: R_full={rf} try_win={tw} ({:.4}) no_move={nm} ({:.4}) term_frac={:.4}",
                tw as f64 / rf as f64,
                nm as f64 / rf as f64,
                (tw + nm) as f64 / rf as f64
            );
        }
        // Per-round frontier profile (≤4×4 fit; find the peak round).
        for &(r, c) in &[(4usize, 3usize), (4, 4)] {
            let mr = MirrorRanker::build(r, c);
            let dir = tmpdir(&format!("prof_{r}x{c}"));
            let _ = run_conv(&mr, r, c, true, &dir, None, Convention::Dtm0).unwrap();
            let sizes = stream_round_sizes(&dir);
            let peak = sizes.iter().map(|s| s.1 + s.2).max().unwrap();
            let r0 = sizes[0].1 + sizes[0].2;
            println!(
                "PROFILE {r}x{c}: R_full={} rounds={} round0={} ({:.4}) peak_round={} ({:.4})",
                mr.r_full(),
                sizes.len(),
                r0,
                r0 as f64 / mr.r_full() as f64,
                peak,
                peak as f64 / mr.r_full() as f64
            );
            let _ = fs::remove_dir_all(&dir);
        }
    }

    // ===================== bounded == unbounded (byte) ======================

    /// `run_bounded` must produce a byte-identical array + DTM stream to the
    /// verified `run_conv`, for both conventions. (For Dtm0 this confirms the
    /// "Lose-pass first ⇒ no snapshot buffer needed" claim.)
    fn check_bounded_eq(rows: usize, cols: usize, conv: Convention) {
        let mr = MirrorRanker::build(rows, cols);
        let da = tmpdir(&format!("unb_{rows}x{cols}"));
        let (run_u, _) = run_conv(&mr, rows, cols, true, &da, None, conv).unwrap();
        let arr_u = run_u.val().to_bytes();
        let stream_u = fs::read(stream_path(&da)).unwrap();

        let db = tmpdir(&format!("bnd_{rows}x{cols}"));
        let (val_b, _) = run_bounded(&mr, rows, cols, true, &db, conv, None).unwrap();
        assert_eq!(arr_u, val_b.to_bytes(), "{rows}x{cols} {conv:?}: array mismatch");
        assert_eq!(
            stream_u,
            fs::read(stream_path(&db)).unwrap(),
            "{rows}x{cols} {conv:?}: stream mismatch"
        );
        let _ = fs::remove_dir_all(&da);
        let _ = fs::remove_dir_all(&db);
    }

    #[test]
    fn bounded_matches_unbounded() {
        for &conv in &[Convention::Dtm0, Convention::Depth1] {
            check_bounded_eq(3, 2, conv);
            check_bounded_eq(3, 3, conv);
            check_bounded_eq(4, 3, conv);
        }
    }

    #[test]
    #[ignore]
    fn bounded_matches_unbounded_4x4() {
        for &conv in &[Convention::Dtm0, Convention::Depth1] {
            check_bounded_eq(4, 4, conv);
        }
    }

    /// Determinism of the bounded solver across thread counts (D1, the run path).
    #[test]
    fn bounded_determinism_small() {
        for &(r, c) in &[(3usize, 3usize), (4, 3)] {
            let mr = MirrorRanker::build(r, c);
            let mut arrs = Vec::new();
            let mut streams = Vec::new();
            for &t in &[1usize, 4, 16, 56] {
                let dir = tmpdir(&format!("bdet_{r}x{c}_{t}"));
                let pool = rayon::ThreadPoolBuilder::new().num_threads(t).build().unwrap();
                let arr = pool.install(|| {
                    run_bounded(&mr, r, c, true, &dir, Convention::Depth1, None).unwrap().0.to_bytes()
                });
                arrs.push(arr);
                streams.push(fs::read(stream_path(&dir)).unwrap());
                let _ = fs::remove_dir_all(&dir);
            }
            for i in 1..arrs.len() {
                assert_eq!(arrs[0], arrs[i], "{r}x{c}: bounded array differs by threads");
                assert_eq!(streams[0], streams[i], "{r}x{c}: bounded stream differs by threads");
            }
        }
    }

    // ===================== 4×5 bounded runs (SSD, ignored) ==================

    fn run_4x5_bounded(conv: Convention, expect_dtm: u32, tag: &str) {
        let mr = MirrorRanker::build(4, 5);
        let dir = PathBuf::from(format!("/tmp/nocca_inmem4x5/{tag}"));
        let _ = fs::remove_dir_all(&dir);
        let t0 = std::time::Instant::now();
        let (_val, st) = run_bounded(&mr, 4, 5, true, &dir, conv, None).unwrap();
        println!(
            "4x5 bounded {tag}: rounds={} elapsed={:.1}s fullspace[W={} L={} D={}] root_val={} root_dtm={} lose_examines={} child_gens={} gens/rep={:.4}",
            st.rounds, t0.elapsed().as_secs_f64(), st.n_win, st.n_lose, st.n_draw,
            st.root_value, st.root_dist, st.lose_examines, st.child_gens,
            st.child_gens as f64 / st.r_full as f64
        );
        assert_eq!(st.root_value, WIN, "4x5 {tag}: root Win");
        assert_eq!(st.root_dist, expect_dtm, "4x5 {tag}: root DTM");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore]
    fn run_4x5_bounded_d0() {
        run_4x5_bounded(Convention::Dtm0, 22, "bd0");
    }

    #[test]
    #[ignore]
    fn run_4x5_bounded_d1() {
        run_4x5_bounded(Convention::Depth1, 23, "bd1");
    }

    // ============ 5×5 oracle (in-memory, SSD, bounded) — step 0 =============
    //
    // 5×5 R_full = 14,664,567,016 (> u32 by 3.4×) ⇒ a genuine >u32 run-path test.
    // Output goes to a scratch dir. Verification is
    // memory-safe: aggregate W/L/D + root + parity from stream headers only (the
    // per-node oracle/replay would overflow u32 / OOM at this scale).

    fn run_5x5_bounded(conv: Convention, tag: &str) {
        let mr = MirrorRanker::build(5, 5);
        let dir = PathBuf::from(format!("/tmp/nocca_inmem5x5/{tag}"));
        let _ = fs::remove_dir_all(&dir);
        let t0 = std::time::Instant::now();
        let (_val, st) = run_bounded(&mr, 5, 5, true, &dir, conv, None).unwrap();
        let el = t0.elapsed().as_secs_f64();
        // DTM distribution + parity from stream headers (memory-safe).
        let sizes = stream_round_sizes(&dir);
        println!(
            "5x5 {tag}: rounds={} elapsed={:.0}s ({:.2}h) fullspace[W={} L={} D={}] root_val={} root_dtm={} lose_examines={} child_gens={} gens/rep={:.4}",
            st.rounds, el, el / 3600.0, st.n_win, st.n_lose, st.n_draw,
            st.root_value, st.root_dist, st.lose_examines, st.child_gens,
            st.child_gens as f64 / st.r_full as f64
        );
        for (rnd, nw, nl) in &sizes {
            println!("  DTM r{rnd}: win={nw} lose={nl}");
        }
        if conv == Convention::Depth1 {
            for &(rnd, nw, nl) in &sizes {
                if rnd % 2 == 1 {
                    assert_eq!(nl, 0, "5x5 D1: lose decided on ODD round {rnd}");
                } else {
                    assert_eq!(nw, 0, "5x5 D1: win decided on EVEN round {rnd}");
                }
            }
            println!("5x5 D1 PARITY OK (win=odd, lose=even) over {} rounds", sizes.len());
        }
        // full-space sanity: W+L+D == R_full
        assert_eq!(st.n_win + st.n_lose + st.n_draw, st.r_full, "5x5 {tag}: W+L+D != R_full");
        assert_eq!(st.root_value, WIN, "5x5 {tag}: root must be Win");
        let _ = fs::remove_dir_all(&dir); // free SSD (~73GB stream)
    }

    #[test]
    #[ignore]
    fn run_5x5_bounded_d0() {
        run_5x5_bounded(Convention::Dtm0, "d0");
    }

    #[test]
    #[ignore]
    fn run_5x5_bounded_d1() {
        run_5x5_bounded(Convention::Depth1, "d1");
    }

    /// 5×5 MILESTONE (Levers 2+3 stacked, >u32 scale). Asserts the exact recorded
    /// 5×5 oracle: root DTM, W/L/D, rounds, D1 parity, W+L+D=R_full. Byte-identity
    /// is already guaranteed by the component gates (pointwise unrank_fast==unrank
    /// sampled at 5×5, L2 set-identity, ≤4×5 byte-identity); this confirms the
    /// stacked levers hold end-to-end at the integer-width-stressing scale.
    #[test]
    #[ignore]
    fn lever_milestone_5x5() {
        let opts = Opts { fast_preds: true, fast_unrank: true, ..Default::default() };
        let mr = MirrorRanker::build(5, 5);
        let rf = mr.r_full();
        assert_eq!(rf, 14_664_567_016, "5x5 R_full");
        let (ow, ol, od) = (10_910_358_067u64, 3_732_358_893u64, 21_850_056u64);
        for &(conv, tag, exp_dtm, exp_rounds) in &[
            (Convention::Dtm0, "d0", 32u32, 53u32),
            (Convention::Depth1, "d1", 33u32, 54u32),
        ] {
            let dir = PathBuf::from(format!("/tmp/nocca_inmem5x5/ms_{tag}"));
            let _ = fs::remove_dir_all(&dir);
            let t = std::time::Instant::now();
            let (_v, st) = run_bounded_opt(&mr, 5, 5, true, &dir, conv, None, opts).unwrap();
            let el = t.elapsed().as_secs_f64();
            let sizes = stream_round_sizes(&dir);
            println!(
                "MILESTONE L2+L3 5x5 {tag}: {el:.0}s ({:.2}h) root={}/{} rounds={} W={} L={} D={}",
                el / 3600.0, st.root_value, st.root_dist, st.rounds, st.n_win, st.n_lose, st.n_draw
            );
            assert_eq!((st.root_value, st.root_dist), (WIN, exp_dtm), "5x5 {tag}: root oracle");
            assert_eq!((st.n_win, st.n_lose, st.n_draw), (ow, ol, od), "5x5 {tag}: W/L/D oracle");
            assert_eq!(st.n_win + st.n_lose + st.n_draw, rf, "5x5 {tag}: W+L+D!=R_full");
            assert_eq!(st.rounds, exp_rounds, "5x5 {tag}: rounds oracle");
            if conv == Convention::Depth1 {
                for &(rnd, nw, nl) in &sizes {
                    if rnd % 2 == 1 {
                        assert_eq!(nl, 0, "5x5 D1 parity: lose on odd {rnd}");
                    } else {
                        assert_eq!(nw, 0, "5x5 D1 parity: win on even {rnd}");
                    }
                }
                println!("MILESTONE 5x5 D1 PARITY OK over {} rounds", sizes.len());
            }
            println!("MILESTONE L2+L3 5x5 {tag}: ORACLE-IDENTICAL ✓");
            let _ = fs::remove_dir_all(&dir);
        }
    }

    // ===================== speed levers — byte-identity gating ===============

    /// Run base (`Opts::default()`) and `opts`, assert array + stream byte-equal.
    fn check_opt_eq(rows: usize, cols: usize, conv: Convention, opts: Opts) {
        let mut mr = MirrorRanker::build(rows, cols);
        if opts.fast_rank3 {
            mr.build_place_mirror();
        }
        let da = tmpdir(&format!("optb_{rows}x{cols}"));
        let (va, _) = run_bounded_opt(&mr, rows, cols, true, &da, conv, None, Opts::default()).unwrap();
        let db = tmpdir(&format!("optf_{rows}x{cols}"));
        let (vb, _) = run_bounded_opt(&mr, rows, cols, true, &db, conv, None, opts).unwrap();
        assert_eq!(va.to_bytes(), vb.to_bytes(), "{rows}x{cols} {conv:?} {opts:?}: ARRAY mismatch");
        assert_eq!(
            fs::read(stream_path(&da)).unwrap(),
            fs::read(stream_path(&db)).unwrap(),
            "{rows}x{cols} {conv:?} {opts:?}: STREAM mismatch"
        );
        let _ = fs::remove_dir_all(&da);
        let _ = fs::remove_dir_all(&db);
    }

    #[test]
    fn lever2_matches_base_small() {
        let o = Opts { fast_preds: true, ..Default::default() };
        for &conv in &[Convention::Dtm0, Convention::Depth1] {
            check_opt_eq(3, 2, conv, o);
            check_opt_eq(3, 3, conv, o);
            check_opt_eq(4, 3, conv, o);
        }
    }

    #[test]
    #[ignore]
    fn lever2_matches_base_4x4() {
        let o = Opts { fast_preds: true, ..Default::default() };
        for &conv in &[Convention::Dtm0, Convention::Depth1] {
            check_opt_eq(4, 4, conv, o);
        }
    }

    /// 4×5 byte-identity + speedup of Lever 2 (base vs fast_preds), D0 and D1.
    /// Generic 4×5 byte-identity + speedup of `opts` vs base, D0 and D1.
    fn bench_opt_4x5(opts: Opts, label: &str) {
        let mut mr = MirrorRanker::build(4, 5);
        if opts.fast_rank3 {
            mr.build_place_mirror();
        }
        for &(conv, tag) in &[(Convention::Dtm0, "d0"), (Convention::Depth1, "d1")] {
            let db = PathBuf::from(format!("/tmp/nocca_lever/base_{tag}"));
            let _ = fs::remove_dir_all(&db);
            let t = std::time::Instant::now();
            let (vb, sb) = run_bounded_opt(&mr, 4, 5, true, &db, conv, None, Opts::default()).unwrap();
            let base_t = t.elapsed().as_secs_f64();
            let df = PathBuf::from(format!("/tmp/nocca_lever/fast_{tag}"));
            let _ = fs::remove_dir_all(&df);
            let t = std::time::Instant::now();
            let (vf, sf) = run_bounded_opt(&mr, 4, 5, true, &df, conv, None, opts).unwrap();
            let fast_t = t.elapsed().as_secs_f64();
            assert_eq!(vb.to_bytes(), vf.to_bytes(), "{label} 4x5 {tag}: ARRAY mismatch");
            assert_eq!(fs::read(stream_path(&db)).unwrap(), fs::read(stream_path(&df)).unwrap(), "{label} 4x5 {tag}: STREAM mismatch");
            assert_eq!((sb.root_dist, sb.n_win, sb.n_lose), (sf.root_dist, sf.n_win, sf.n_lose));
            println!(
                "{label} 4x5 {tag}: BYTE-IDENTICAL ✓  base={base_t:.0}s fast={fast_t:.0}s speedup={:.3}x ({:.1}%)",
                base_t / fast_t,
                (base_t - fast_t) / base_t * 100.0
            );
            let _ = fs::remove_dir_all(&db);
            let _ = fs::remove_dir_all(&df);
        }
    }

    #[test]
    #[ignore]
    fn lever2_4x5() {
        bench_opt_4x5(Opts { fast_preds: true, ..Default::default() }, "LEVER2");
    }

    #[test]
    fn lever3_matches_base_small() {
        let l3 = Opts { fast_unrank: true, ..Default::default() };
        let l23 = Opts { fast_preds: true, fast_unrank: true, ..Default::default() };
        for &conv in &[Convention::Dtm0, Convention::Depth1] {
            check_opt_eq(3, 2, conv, l3);
            check_opt_eq(3, 3, conv, l3);
            check_opt_eq(4, 3, conv, l3);
            check_opt_eq(4, 3, conv, l23); // stacked L2+L3
        }
    }

    #[test]
    #[ignore]
    fn lever3_matches_base_4x4() {
        let l3 = Opts { fast_unrank: true, ..Default::default() };
        let l23 = Opts { fast_preds: true, fast_unrank: true, ..Default::default() };
        for &conv in &[Convention::Dtm0, Convention::Depth1] {
            check_opt_eq(4, 4, conv, l3);
            check_opt_eq(4, 4, conv, l23);
        }
    }

    /// 4×5 byte-identity + speedup: Lever 3 alone, and Lever 2+3 stacked.
    #[test]
    #[ignore]
    fn lever3_4x5() {
        bench_opt_4x5(Opts { fast_unrank: true, ..Default::default() }, "LEVER3");
        bench_opt_4x5(Opts { fast_preds: true, fast_unrank: true, ..Default::default() }, "LEVER2+3");
    }

    #[test]
    fn lever4_matches_base_small() {
        let l4 = Opts { fast_rank: true, ..Default::default() };
        let all = Opts { fast_preds: true, fast_unrank: true, fast_rank: true, ..Default::default() };
        for &conv in &[Convention::Dtm0, Convention::Depth1] {
            check_opt_eq(3, 2, conv, l4);
            check_opt_eq(3, 3, conv, l4);
            check_opt_eq(4, 3, conv, l4);
            check_opt_eq(4, 3, conv, all); // L2+3+4 stacked
        }
    }

    #[test]
    #[ignore]
    fn lever4_matches_base_4x4() {
        let l4 = Opts { fast_rank: true, ..Default::default() };
        let all = Opts { fast_preds: true, fast_unrank: true, fast_rank: true, ..Default::default() };
        for &conv in &[Convention::Dtm0, Convention::Depth1] {
            check_opt_eq(4, 4, conv, l4);
            check_opt_eq(4, 4, conv, all);
        }
    }

    /// 4×5 byte-identity + speedup: Lever 4 alone, and all levers (2+3+4) stacked.
    #[test]
    #[ignore]
    fn lever4_4x5() {
        bench_opt_4x5(Opts { fast_rank: true, ..Default::default() }, "LEVER4");
        bench_opt_4x5(Opts { fast_preds: true, fast_unrank: true, fast_rank: true, ..Default::default() }, "LEVER2+3+4");
    }

    #[test]
    fn lever5_matches_base_small() {
        let l5 = Opts { fast_unrank2: true, ..Default::default() };
        let l25 = Opts { fast_preds: true, fast_unrank2: true, ..Default::default() };
        for &conv in &[Convention::Dtm0, Convention::Depth1] {
            check_opt_eq(3, 2, conv, l5);
            check_opt_eq(3, 3, conv, l5);
            check_opt_eq(4, 3, conv, l5);
            check_opt_eq(4, 3, conv, l25);
        }
    }

    #[test]
    #[ignore]
    fn lever5_matches_base_4x4() {
        let l5 = Opts { fast_unrank2: true, ..Default::default() };
        let l25 = Opts { fast_preds: true, fast_unrank2: true, ..Default::default() };
        for &conv in &[Convention::Dtm0, Convention::Depth1] {
            check_opt_eq(4, 4, conv, l5);
            check_opt_eq(4, 4, conv, l25);
        }
    }

    /// 4×5 byte-identity + speedup: Lever 5 alone, the 本走 candidate L2+L5, and
    /// the current L2+L3 for comparison (all run on all 56 cores by default rayon).
    #[test]
    #[ignore]
    fn lever5_4x5() {
        bench_opt_4x5(Opts { fast_unrank2: true, ..Default::default() }, "LEVER5");
        bench_opt_4x5(Opts { fast_preds: true, fast_unrank2: true, ..Default::default() }, "LEVER2+5");
    }

    #[test]
    fn lever6_matches_base_small() {
        let l6 = Opts { fast_rank3: true, ..Default::default() };
        let all = Opts { fast_preds: true, fast_unrank2: true, fast_rank3: true, ..Default::default() };
        for &conv in &[Convention::Dtm0, Convention::Depth1] {
            check_opt_eq(3, 2, conv, l6);
            check_opt_eq(3, 3, conv, l6);
            check_opt_eq(4, 3, conv, l6);
            check_opt_eq(4, 3, conv, all);
        }
    }

    #[test]
    #[ignore]
    fn lever6_matches_base_4x4() {
        let l6 = Opts { fast_rank3: true, ..Default::default() };
        let all = Opts { fast_preds: true, fast_unrank2: true, fast_rank3: true, ..Default::default() };
        for &conv in &[Convention::Dtm0, Convention::Depth1] {
            check_opt_eq(4, 4, conv, l6);
            check_opt_eq(4, 4, conv, all);
        }
    }

    /// 4×5 full-retrograde byte-identity + speedup: ③a alone, and the full stack
    /// L2+L5+③a (the candidate if ③a delivers at the retrograde level).
    #[test]
    #[ignore]
    fn lever6_4x5() {
        bench_opt_4x5(Opts { fast_rank3: true, ..Default::default() }, "LEVER6(3a)");
        bench_opt_4x5(
            Opts { fast_preds: true, fast_unrank2: true, fast_rank3: true, ..Default::default() },
            "LEVER2+5+3a",
        );
    }

    /// CLEAN same-run marginal: L2+L5 vs L2+L5+③a, D1 (本走 conv), back-to-back so
    /// the system state is identical — the only valid way to see ③a's marginal %
    /// on top of L2+L5 (cross-run base variance is ~10%, larger than ③a's effect).
    #[test]
    #[ignore]
    fn lever6_marginal_4x5_d1() {
        let mut mr = MirrorRanker::build(4, 5);
        mr.build_place_mirror();
        let l25 = Opts { fast_preds: true, fast_unrank2: true, ..Default::default() };
        let l253a = Opts { fast_preds: true, fast_unrank2: true, fast_rank3: true, ..Default::default() };
        let mut t25 = 0.0f64;
        for (opts, label) in [(l25, "L2+L5"), (l253a, "L2+L5+3a")] {
            let dir = PathBuf::from(format!("/tmp/nocca_lever/m_{}", label.replace('+', "_")));
            let _ = fs::remove_dir_all(&dir);
            let t = std::time::Instant::now();
            let (_v, st) = run_bounded_opt(&mr, 4, 5, true, &dir, Convention::Depth1, None, opts).unwrap();
            let el = t.elapsed().as_secs_f64();
            assert_eq!((st.root_value, st.root_dist), (WIN, 23), "{label}: root");
            if label == "L2+L5" {
                t25 = el;
            } else {
                println!(
                    "MARGINAL 4x5 D1: L2+L5={t25:.0}s  L2+L5+3a={el:.0}s  ③a marginal={:.1}% ({})",
                    (t25 - el) / t25 * 100.0,
                    if el < t25 { "③a helps" } else { "③a HURTS/neutral" }
                );
            }
            let _ = fs::remove_dir_all(&dir);
        }
    }

    /// mirror_unrank throughput (base vs fast2) across thread counts, to see if the
    /// k_at coarse table's random access degrades 56-core scaling (per the concern
    /// that table lookups add memory-bandwidth pressure at high parallelism).
    #[test]
    #[ignore]
    fn lever5_unrank_scaling() {
        for &(r, c) in &[(4usize, 5usize), (5, 5)] {
            println!("===== {r}x{c} mirror_unrank scaling (table = {} entries) =====", {
                let mr = MirrorRanker::build(r, c);
                (mr.r_full() >> 13) + 2
            });
            let mr = MirrorRanker::build(r, c);
            let rf = mr.r_full();
            let n = 4_000_000u64;
            let step = (rf / n).max(1);
            let samples: Vec<u64> = (0..n).map(|j| (j * step).min(rf - 1)).collect();
            for &(label, fast2) in &[("base ", false), ("fast2", true)] {
                let mut base1 = 0.0f64;
                for &th in &[1usize, 16, 28, 56] {
                    let pool = rayon::ThreadPoolBuilder::new().num_threads(th).build().unwrap();
                    let t0 = std::time::Instant::now();
                    let sink: u64 = pool.install(|| {
                        samples
                            .par_iter()
                            .map(|&idx| {
                                let b = if fast2 { mr.mirror_unrank_fast2(idx) } else { mr.mirror_unrank(idx) };
                                b.cols() as u64
                            })
                            .sum()
                    });
                    let el = t0.elapsed().as_secs_f64();
                    if th == 1 {
                        base1 = el;
                    }
                    println!(
                        "  {label} th={th:2}  {el:5.2}s  {:6.2} Munrank/s  scale={:.1}x (sink={sink})",
                        n as f64 / el / 1e6,
                        base1 / el
                    );
                }
            }
        }
    }

    /// 5×5 MILESTONE for the 本走 candidate (D1, L2+L5) at the >u32 scale.
    /// Asserts the D1 oracle: root Win/33, exact W/L/D, parity, rounds, W+L+D=R_full.
    /// (The Lever-5 72 MB `k_at` table is indexed by `idx>>13` up to ~1.79M buckets
    /// over the 14.66e9 idx range — an oracle match confirms its u64 indexing.)
    #[test]
    #[ignore]
    fn lever5_milestone_5x5_d1() {
        let opts = Opts { fast_preds: true, fast_unrank2: true, ..Default::default() };
        let mr = MirrorRanker::build(5, 5);
        let rf = mr.r_full();
        assert_eq!(rf, 14_664_567_016, "5x5 R_full");
        let dir = PathBuf::from("/tmp/nocca_inmem5x5/ms_l25_d1");
        let _ = fs::remove_dir_all(&dir);
        let t = std::time::Instant::now();
        let (_v, st) =
            run_bounded_opt(&mr, 5, 5, true, &dir, Convention::Depth1, None, opts).unwrap();
        let el = t.elapsed().as_secs_f64();
        let sizes = stream_round_sizes(&dir);
        println!(
            "MILESTONE L2+L5 5x5 d1: {el:.0}s ({:.2}h) root={}/{} rounds={} W={} L={} D={}",
            el / 3600.0, st.root_value, st.root_dist, st.rounds, st.n_win, st.n_lose, st.n_draw
        );
        for &(rnd, nw, nl) in &sizes {
            if rnd % 2 == 1 {
                assert_eq!(nl, 0, "5x5 D1 parity: lose on odd {rnd}");
            } else {
                assert_eq!(nw, 0, "5x5 D1 parity: win on even {rnd}");
            }
        }
        assert_eq!((st.root_value, st.root_dist), (WIN, 33), "5x5 D1 root oracle");
        assert_eq!(
            (st.n_win, st.n_lose, st.n_draw),
            (10_910_358_067, 3_732_358_893, 21_850_056),
            "5x5 D1 W/L/D oracle"
        );
        assert_eq!(st.n_win + st.n_lose + st.n_draw, rf, "5x5 D1 W+L+D != R_full");
        assert_eq!(st.rounds, 54, "5x5 D1 rounds");
        println!(
            "MILESTONE L2+L5 5x5 d1: ORACLE-IDENTICAL ✓ (root Win/33, W/L/D, parity {} rounds, u64@>u32)",
            sizes.len()
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// ③a probe: mirror_rank throughput base vs `fast3` (place_mirror table) at
    /// 1/16/28/56 threads — tests the prediction "single-thread faster, 56-core
    /// worse (2.35 GB random DRAM saturates the memory-bound bottleneck)".
    #[test]
    #[ignore]
    fn lever6_place_mirror_probe() {
        for &(r, c) in &[(4usize, 5usize), (5, 5)] {
            lever6_probe_one(r, c);
        }
    }

    fn lever6_probe_one(r: usize, c: usize) {
        let total = 2 * c;
        let comp = crate::rank::comp_table(r * c, total);
        let nplace = comp[r * c][total];
        let mut mr = MirrorRanker::build(r, c);
        let t0 = std::time::Instant::now();
        mr.build_place_mirror();
        println!(
            "place_mirror {r}x{c}: {} entries = {:.0} MB, built in {:.1}s",
            nplace,
            nplace as f64 * 4.0 / 1e6,
            t0.elapsed().as_secs_f64()
        );
        let rf = mr.r_full();
        let n = 3_000_000u64;
        let step = (rf / n).max(1);
        let boards: Vec<VarBoard> = (0..n).map(|j| mr.mirror_unrank((j * step).min(rf - 1))).collect();
        let bad = boards.iter().filter(|b| mr.mirror_rank_fast3(b) != mr.mirror_rank(b)).count();
        println!("pointwise mismatches on {n} sample = {bad}");
        assert_eq!(bad, 0, "fast3 != mirror_rank on sample");
        for &(label, f3) in &[("base ", false), ("fast3", true)] {
            let mut base1 = 0.0f64;
            for &th in &[1usize, 16, 28, 56] {
                let pool = rayon::ThreadPoolBuilder::new().num_threads(th).build().unwrap();
                let t0 = std::time::Instant::now();
                let sink: u64 = pool.install(|| {
                    boards
                        .par_iter()
                        .map(|b| if f3 { mr.mirror_rank_fast3(b) } else { mr.mirror_rank(b) })
                        .sum()
                });
                let el = t0.elapsed().as_secs_f64();
                if th == 1 {
                    base1 = el;
                }
                println!(
                    "  {label} th={th:2}  {el:5.2}s  {:6.2} Mrank/s  scale={:.1}x (sink={sink})",
                    n as f64 / el / 1e6,
                    base1 / el
                );
            }
        }
    }

    /// 6×5 terminal census alone (memory-safe; ~minutes — scans 74e9 reps).
    #[test]
    #[ignore]
    fn term_census_6x5() {
        let mr = MirrorRanker::build(6, 5);
        let (tw, nm, rf) = count_terminals(&mr, true);
        println!(
            "TERM 6x5: R_full={rf} try_win={tw} ({:.4}) no_move={nm} ({:.6}) term_frac={:.4}",
            tw as f64 / rf as f64,
            nm as f64 / rf as f64,
            (tw + nm) as f64 / rf as f64
        );
    }
}
