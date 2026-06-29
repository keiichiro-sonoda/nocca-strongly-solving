//! Disk-based (external-memory) retrograde for NOCCA. All set operations are
//! external sort + merge, so dedup/membership is O(n) per layer (not O(chunks²))
//! and it runs out-of-core in small RAM. Validated against the in-RAM
//! [`crate::retro::solve_ram`].
//!
//! A [`Codec`] selects the on-disk node identity: **plain** = `canonical_key`
//! (16-byte u128), or **compressed** = `rank` (8-byte u64, see [`crate::rank`]).
//! Only the serialization width and the board↔key map change; the sort/merge
//! logic is byte-for-byte identical (keys are `u128` in memory throughout).

use crate::extsort::{self, count, ExtConfig, KeyReader, MergeOp};
use crate::solver::Value;
use crate::varboard::VarBoard;
use rayon::prelude::*;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const IO_BUF_BYTES: usize = 4 * 1024 * 1024;
const BITSET_IO_WORDS: usize = IO_BUF_BYTES / std::mem::size_of::<u64>();

/// Node-identity codec: maps boards ↔ on-disk keys at a fixed byte width.
#[derive(Clone, Copy)]
pub struct Codec {
    pub compress: bool,
    pub rows: usize,
    pub cols: usize,
    /// Bytes per key on disk (8 = compressed rank, 16 = plain canonical key).
    pub kb: usize,
}
impl Codec {
    pub fn new(compress: bool, rows: usize, cols: usize) -> Self {
        Codec {
            compress,
            rows,
            cols,
            kb: if compress { 8 } else { 16 },
        }
    }
    #[inline]
    fn key(&self, b: &VarBoard) -> u128 {
        // Always identify by the canonical (mirror-min) representative. In
        // compressed mode we rank THAT board (not `b` as-is), so mirror images
        // fold to one rank — matching the plain `canonical_key` folding.
        let ck = b.canonical_key();
        if self.compress {
            VarBoard::from_key(self.rows, self.cols, ck).rank() as u128
        } else {
            ck
        }
    }
    #[inline]
    fn board(&self, k: u128) -> VarBoard {
        if self.compress {
            VarBoard::unrank(self.rows, self.cols, k as u64)
        } else {
            VarBoard::from_key(self.rows, self.cols, k)
        }
    }
    /// Map a canonical key (as produced by `predecessors`) to this codec's key.
    #[inline]
    fn key_from_canonical(&self, ck: u128) -> u128 {
        if self.compress {
            VarBoard::from_key(self.rows, self.cols, ck).rank() as u128
        } else {
            ck
        }
    }
    /// ExtConfig with this codec's key width.
    fn cfg(&self, base: &ExtConfig) -> ExtConfig {
        ExtConfig::with_kbytes(base.run_size, base.fanin, self.kb)
    }
}

// ---- single-key writer (width-aware) -------------------------------------
struct KW {
    w: BufWriter<File>,
    kb: usize,
}
fn keywriter(p: &Path, kb: usize) -> io::Result<KW> {
    Ok(KW {
        w: BufWriter::with_capacity(IO_BUF_BYTES, File::create(p)?),
        kb,
    })
}
impl KW {
    #[inline]
    fn put(&mut self, k: u128) -> io::Result<()> {
        self.w.write_all(&k.to_le_bytes()[..self.kb])
    }
    fn finish(mut self) -> io::Result<()> {
        self.w.flush()
    }
}
fn write_one(path: &Path, key: u128, kb: usize) -> io::Result<()> {
    let mut w = keywriter(path, kb)?;
    w.put(key)?;
    w.finish()
}

/// Source keys read per parallel batch (each yields ~tens of emitted keys).
const PAR_GEN_BATCH: usize = 1_000_000;

/// Children of one board (try-win / no-move terminals emit nothing), as codec keys.
#[inline]
fn emit_children(codec: &Codec, b: &VarBoard, mv: &mut Vec<(usize, usize)>, out: &mut Vec<u128>) {
    if b.try_win() {
        return;
    }
    b.moves(mv, true);
    if mv.is_empty() {
        return;
    }
    for &(f, t) in mv.iter() {
        out.push(codec.key(&b.apply_move(f, t)));
    }
}

#[inline]
fn emit_children_new_ranks(
    codec: &Codec,
    visited: &AtomicBitSet,
    b: &VarBoard,
    mv: &mut Vec<(usize, usize)>,
    out: &mut Vec<u64>,
) {
    if b.try_win() {
        return;
    }
    b.moves(mv, true);
    if mv.is_empty() {
        return;
    }
    for &(f, t) in mv.iter() {
        let rank = codec.key(&b.apply_move(f, t)) as u64;
        if visited.set_new(rank) {
            out.push(rank);
        }
    }
}

/// Predecessors of one board (terminal parents filtered), as codec keys.
#[inline]
fn emit_preds(codec: &Codec, b: &VarBoard, buf: &mut Vec<u128>, out: &mut Vec<u128>) {
    b.predecessors(true, true, buf);
    for &ck in buf.iter() {
        out.push(codec.key_from_canonical(ck));
    }
}

/// Parallel batched key generator. Reads up to [`PAR_GEN_BATCH`] source keys,
/// then computes each source's emitted keys (children or predecessors) across
/// the rayon pool, yielding the flattened multiset. Order within a batch is
/// unspecified, but the multiset equals the sequential version — and every
/// consumer sorts the stream, so order is irrelevant. Threads = the global rayon
/// pool (`RAYON_NUM_THREADS`); 1 thread ≈ sequential. The per-thread table cache
/// in [`crate::rank`] is built once per worker.
struct ParGen {
    rdr: KeyReader,
    codec: Codec,
    preds: bool,
    pending: Vec<u128>,
    pos: usize,
}
impl ParGen {
    fn children(frontier: &Path, codec: Codec) -> io::Result<Self> {
        Ok(ParGen {
            rdr: KeyReader::open(frontier, codec.kb)?,
            codec,
            preds: false,
            pending: Vec::new(),
            pos: 0,
        })
    }
    fn preds(frontier: &Path, codec: Codec) -> io::Result<Self> {
        Ok(ParGen {
            rdr: KeyReader::open(frontier, codec.kb)?,
            codec,
            preds: true,
            pending: Vec::new(),
            pos: 0,
        })
    }
    fn refill(&mut self) {
        let mut src: Vec<u128> = Vec::with_capacity(PAR_GEN_BATCH);
        for _ in 0..PAR_GEN_BATCH {
            match self.rdr.next() {
                Some(k) => src.push(k),
                None => break,
            }
        }
        self.pos = 0;
        if src.is_empty() {
            self.pending.clear();
            return;
        }
        let codec = self.codec;
        let preds = self.preds;
        self.pending = src
            .par_iter()
            .fold(
                || {
                    (
                        Vec::<u128>::new(),
                        Vec::<(usize, usize)>::new(),
                        Vec::<u128>::new(),
                    )
                },
                |(mut out, mut mv, mut pbuf), &k| {
                    let b = codec.board(k);
                    if preds {
                        emit_preds(&codec, &b, &mut pbuf, &mut out);
                    } else {
                        emit_children(&codec, &b, &mut mv, &mut out);
                    }
                    (out, mv, pbuf)
                },
            )
            .map(|(out, _, _)| out)
            .reduce(Vec::new, |mut a, mut b| {
                a.append(&mut b);
                a
            });
    }
}
impl Iterator for ParGen {
    type Item = u128;
    fn next(&mut self) -> Option<u128> {
        loop {
            if self.pos < self.pending.len() {
                let k = self.pending[self.pos];
                self.pos += 1;
                return Some(k);
            }
            self.refill();
            if self.pending.is_empty() {
                return None;
            }
        }
    }
}

/// Result of Phase 1.
pub struct Enumeration {
    pub visited: PathBuf,
    pub total: u64,
}

struct AtomicBitSet {
    words: Vec<AtomicU64>,
    nbits: u64,
}

impl AtomicBitSet {
    fn new(nbits: u64) -> io::Result<Self> {
        let nwords = nbits
            .checked_add(63)
            .and_then(|n| n.checked_div(64))
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "bitset too large"))?;
        let mut words = Vec::new();
        words.try_reserve_exact(nwords).map_err(|e| {
            io::Error::new(
                io::ErrorKind::OutOfMemory,
                format!("bitset allocation failed: {e}"),
            )
        })?;
        words.resize_with(nwords, || AtomicU64::new(0));
        Ok(AtomicBitSet { words, nbits })
    }

    #[inline]
    fn set_new(&self, bit: u64) -> bool {
        debug_assert!(bit < self.nbits);
        let idx = (bit / 64) as usize;
        let mask = 1u64 << (bit & 63);
        self.words[idx].fetch_or(mask, Ordering::Relaxed) & mask == 0
    }

    fn count_ones(&self) -> u64 {
        self.words
            .iter()
            .map(|w| w.load(Ordering::Relaxed).count_ones() as u64)
            .sum()
    }
}

fn rank_space(codec: Codec) -> io::Result<u64> {
    let ncells = codec.rows * codec.cols;
    let total = 2 * codec.cols;
    let (comp, binom) = crate::rank::tables(ncells, total);
    comp[ncells][total]
        .checked_mul(binom[total][codec.cols])
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "rank space overflow"))
}

fn read_key_batch_u64(r: &mut KeyReader, out: &mut Vec<u64>) -> usize {
    out.clear();
    for _ in 0..PAR_GEN_BATCH {
        match r.next() {
            Some(k) => out.push(k as u64),
            None => break,
        }
    }
    out.len()
}

fn write_bitset(path: &Path, bits: &AtomicBitSet) -> io::Result<()> {
    let mut w = BufWriter::with_capacity(IO_BUF_BYTES, File::create(path)?);
    let mut buf = Vec::with_capacity(IO_BUF_BYTES);
    for chunk in bits.words.chunks(BITSET_IO_WORDS) {
        buf.clear();
        for word in chunk {
            buf.extend_from_slice(&word.load(Ordering::Relaxed).to_le_bytes());
        }
        w.write_all(&buf)?;
    }
    w.flush()
}

fn read_bitset(path: &Path, nbits: u64) -> io::Result<AtomicBitSet> {
    let bits = AtomicBitSet::new(nbits)?;
    let expected_len = (bits.words.len() as u64) * 8;
    let got_len = fs::metadata(path)?.len();
    if got_len != expected_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid bitset length: got {got_len}, expected {expected_len}"),
        ));
    }
    let mut r = BufReader::with_capacity(IO_BUF_BYTES, File::open(path)?);
    let mut buf = vec![0u8; IO_BUF_BYTES];
    let mut offset = 0usize;
    while offset < bits.words.len() {
        let n = BITSET_IO_WORDS.min(bits.words.len() - offset);
        let bytes = &mut buf[..n * 8];
        r.read_exact(bytes)?;
        for (i, raw) in bytes.chunks_exact(8).enumerate() {
            let mut a = [0u8; 8];
            a.copy_from_slice(raw);
            bits.words[offset + i].store(u64::from_le_bytes(a), Ordering::Relaxed);
        }
        offset += n;
    }
    Ok(bits)
}

fn bitset_from_visited(visited: &Path, codec: Codec, nbits: u64) -> io::Result<AtomicBitSet> {
    let bits = AtomicBitSet::new(nbits)?;
    let mut r = KeyReader::open(visited, codec.kb)?;
    while let Some(k) = r.next() {
        bits.set_new(k as u64);
    }
    Ok(bits)
}

fn write_visited_from_bitset(bits: &AtomicBitSet, out: &Path, kb: usize) -> io::Result<u64> {
    let mut w = keywriter(out, kb)?;
    let mut total = 0u64;
    for (i, word) in bits.words.iter().enumerate() {
        let mut v = word.load(Ordering::Relaxed);
        while v != 0 {
            let bit = v.trailing_zeros() as u64;
            let rank = (i as u64) * 64 + bit;
            if rank < bits.nbits {
                w.put(rank as u128)?;
                total += 1;
            }
            v &= v - 1;
        }
    }
    w.finish()?;
    Ok(total)
}

fn write_frontier_next_bitset(
    frontier: &Path,
    frontier_next: &Path,
    codec: Codec,
    visited: &AtomicBitSet,
) -> io::Result<u64> {
    let mut r = KeyReader::open(frontier, codec.kb)?;
    let mut w = keywriter(frontier_next, codec.kb)?;
    let mut src = Vec::<u64>::with_capacity(PAR_GEN_BATCH);
    let mut added = 0u64;
    while read_key_batch_u64(&mut r, &mut src) != 0 {
        let new_keys = src
            .par_iter()
            .fold(
                || (Vec::<u64>::new(), Vec::<(usize, usize)>::new()),
                |(mut out, mut mv), &k| {
                    let b = codec.board(k as u128);
                    emit_children_new_ranks(&codec, visited, &b, &mut mv, &mut out);
                    (out, mv)
                },
            )
            .map(|(out, _)| out)
            .reduce(Vec::new, |mut a, mut b| {
                a.append(&mut b);
                a
            });
        added += new_keys.len() as u64;
        for k in new_keys {
            w.put(k as u128)?;
        }
    }
    w.finish()?;
    Ok(added)
}

fn enumerate_bitset(codec: Codec, dir: &Path) -> io::Result<Enumeration> {
    debug_assert!(codec.compress);
    fs::create_dir_all(dir)?;
    let kb = codec.kb;
    let init = codec.key(&VarBoard::initial(codec.rows, codec.cols)) as u64;
    let rank_space = rank_space(codec)?;

    let visited = dir.join("visited.bin");
    let visited_next = dir.join("visited.next");
    let visited_bits = dir.join("visited.bits");
    let visited_bits_next = dir.join("visited.bits.next");
    let frontier = dir.join("frontier.bin");
    let frontier_next = dir.join("frontier.next");
    let echk = dir.join("enum.checkpoint");
    let t_enum = std::time::Instant::now();

    recover_enum(dir)?;
    let mut layer;
    let bits;
    match read_enum_meta(&echk) {
        Some(c) if c.done => {
            return Ok(Enumeration {
                total: count(&visited, kb)?,
                visited,
            });
        }
        Some(c) => {
            layer = c.layer;
            bits = if visited_bits.exists() {
                read_bitset(&visited_bits, rank_space)?
            } else if visited.exists() {
                bitset_from_visited(&visited, codec, rank_space)?
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "missing visited state for enum resume",
                ));
            };
        }
        None => {
            layer = 0;
            bits = AtomicBitSet::new(rank_space)?;
            bits.set_new(init);
            write_one(&frontier, init as u128, kb)?;
            write_bitset(&visited_bits, &bits)?;
            write_enum_meta(
                &echk,
                &EnumCkpt {
                    layer: 0,
                    done: false,
                },
            )?;
        }
    }
    let mut nvis = bits.count_ones();

    loop {
        let added = write_frontier_next_bitset(&frontier, &frontier_next, codec, &bits)?;
        if added == 0 {
            let total = write_visited_from_bitset(&bits, &visited_next, kb)?;
            fs::rename(&visited_next, &visited)?;
            write_enum_meta(&echk, &EnumCkpt { layer, done: true })?;
            nvis = total;
            break;
        }
        write_bitset(&visited_bits_next, &bits)?;
        enum_commit(
            dir,
            &EnumCkpt {
                layer: layer + 1,
                done: false,
            },
        )?;
        layer += 1;
        nvis += added;
        eprintln!(
            "[enum-bitset L{layer}] +{added} visited={nvis} elapsed={:.0}s",
            t_enum.elapsed().as_secs_f64()
        );
        if layer >= enum_limit() {
            PHASE_HIT.with(|c| c.set(true));
            return Ok(Enumeration {
                visited,
                total: nvis,
            });
        }
    }
    Ok(Enumeration {
        visited,
        total: nvis,
    })
}

/// Phase 1: enumerate all reachable keys (per `codec`) into a sorted/deduped file.
///
/// **Resumable** at BFS-layer boundaries (same write-ahead + atomic rename as the
/// retrograde rounds). If `enum.checkpoint` exists, resumes the layered BFS from
/// the on-disk {visited,frontier}; `edone=1` means it already reached fixpoint.
pub fn enumerate(codec: Codec, cfg: &ExtConfig, dir: &Path) -> io::Result<Enumeration> {
    if codec.compress && std::env::var_os("NOCCA_ENUM_EXTERNAL").is_none() {
        return enumerate_bitset(codec, dir);
    }

    fs::create_dir_all(dir)?;
    let kb = codec.kb;
    let cfg = codec.cfg(cfg);
    let sortdir = dir.join("sort");
    let init = codec.key(&VarBoard::initial(codec.rows, codec.cols));

    let visited = dir.join("visited.bin");
    let frontier = dir.join("frontier.bin");
    let children = dir.join("children.bin");
    let newf = dir.join("new.bin");
    let visited_n = dir.join("visited.next");
    let frontier_n = dir.join("frontier.next");
    let echk = dir.join("enum.checkpoint");
    let t_enum = std::time::Instant::now();

    // ----- resume or fresh start (layer-boundary checkpoint) -----
    recover_enum(dir)?;
    let mut layer = match read_enum_meta(&echk) {
        Some(c) if c.done => {
            return Ok(Enumeration {
                total: count(&visited, kb)?,
                visited,
            });
        }
        Some(c) => c.layer, // resume: on-disk visited/frontier are at this layer
        None => {
            write_one(&visited, init, kb)?;
            write_one(&frontier, init, kb)?;
            write_enum_meta(
                &echk,
                &EnumCkpt {
                    layer: 0,
                    done: false,
                },
            )?;
            0
        }
    };
    let mut nvis = count(&visited, kb)?; // running total (avoids O(N) recount per layer)

    loop {
        extsort::sort_dedup(
            ParGen::children(&frontier, codec)?,
            &cfg,
            &sortdir,
            &children,
        )?;
        extsort::merge2(&children, &visited, MergeOp::Subtract, &newf, kb)?;
        let added = count(&newf, kb)?;
        if added == 0 {
            write_enum_meta(&echk, &EnumCkpt { layer, done: true })?;
            break;
        }
        // next visited/frontier into .next, then commit the layer atomically.
        extsort::merge2(&visited, &newf, MergeOp::Union, &visited_n, kb)?;
        fs::rename(&newf, &frontier_n)?;
        enum_commit(
            dir,
            &EnumCkpt {
                layer: layer + 1,
                done: false,
            },
        )?;
        layer += 1;
        nvis += added;
        eprintln!(
            "[enum L{layer}] +{added} visited={nvis} elapsed={:.0}s",
            t_enum.elapsed().as_secs_f64()
        );
        if layer >= enum_limit() {
            // Test hook: simulate interruption after a clean layer commit. Caller
            // (`solve_disk_frontier`) sees PHASE_HIT and bails; resuming completes.
            PHASE_HIT.with(|c| c.set(true));
            return Ok(Enumeration {
                visited,
                total: nvis,
            });
        }
    }
    Ok(Enumeration {
        visited,
        total: nvis,
    })
}

// ===================== pair IO (width-aware: 2 × kb bytes) ====================

struct PairWriter {
    w: BufWriter<File>,
    kb: usize,
}
impl PairWriter {
    fn create(p: &Path, kb: usize) -> io::Result<Self> {
        Ok(PairWriter {
            w: BufWriter::with_capacity(IO_BUF_BYTES, File::create(p)?),
            kb,
        })
    }
    #[inline]
    fn put(&mut self, a: u128, b: u128) -> io::Result<()> {
        self.w.write_all(&a.to_le_bytes()[..self.kb])?;
        self.w.write_all(&b.to_le_bytes()[..self.kb])
    }
    fn finish(mut self) -> io::Result<()> {
        self.w.flush()
    }
}
struct PairReader {
    r: BufReader<File>,
    kb: usize,
}
impl PairReader {
    fn open(p: &Path, kb: usize) -> io::Result<Self> {
        Ok(PairReader {
            r: BufReader::with_capacity(IO_BUF_BYTES, File::open(p)?),
            kb,
        })
    }
}
impl Iterator for PairReader {
    type Item = (u128, u128);
    fn next(&mut self) -> Option<(u128, u128)> {
        let mut a = [0u8; 16];
        let mut b = [0u8; 16];
        if self.r.read_exact(&mut a[..self.kb]).is_err() {
            return None;
        }
        if self.r.read_exact(&mut b[..self.kb]).is_err() {
            return None;
        }
        Some((u128::from_le_bytes(a), u128::from_le_bytes(b)))
    }
}

// ===================== Phase 2 (frontier / predecessor) =====================
// Predecessor generation is `ParGen::preds` (parallel; see above).

/// Classify reachable keys into win/lose terminals + unknown, emitting out-degrees.
///
/// **Parallel** (the per-state classification is independent and was the dominant
/// single-threaded cost: it regenerates every edge to count distinct children =
/// `outdeg`). Sorted `visited` is read in batches; each batch is classified across
/// the rayon pool with `map_init` (order-preserving + per-thread scratch buffers),
/// then emitted IN ORDER — so win/lose/unknown/outdeg stay rank-sorted with no extra
/// sort. The per-state result is identical to the sequential version regardless of
/// thread count (deterministic; validated byte-for-byte by
/// `classify_parallel_matches_sequential`). Mirrors `ParGen`'s deterministic-multiset
/// pattern; does not touch the BFS/`ParGen`/`emit_*` path.
fn classify_build(
    codec: Codec,
    visited: &Path,
    win: &Path,
    lose: &Path,
    unknown: &Path,
    outdeg: &Path,
) -> io::Result<()> {
    let kb = codec.kb;
    let mut win_w = keywriter(win, kb)?;
    let mut lose_w = keywriter(lose, kb)?;
    let mut unk_w = keywriter(unknown, kb)?;
    let mut od_w = PairWriter::create(outdeg, kb)?;
    let mut vis = KeyReader::open(visited, kb)?;
    let mut batch: Vec<u128> = Vec::with_capacity(PAR_GEN_BATCH);
    loop {
        batch.clear();
        for _ in 0..PAR_GEN_BATCH {
            match vis.next() {
                Some(k) => batch.push(k),
                None => break,
            }
        }
        if batch.is_empty() {
            break;
        }
        // Classify the batch in parallel, preserving input (sorted) order.
        // kind: 0 = win (try-win terminal), 1 = lose (no-move terminal), 2 = unknown.
        let results: Vec<(u128, u8, u32)> = batch
            .par_iter()
            .map_init(
                || (Vec::<(usize, usize)>::new(), Vec::<u128>::new()),
                |(mv, ch), &k| {
                    let b = codec.board(k);
                    if b.try_win() {
                        return (k, 0u8, 0u32);
                    }
                    b.moves(mv, true);
                    if mv.is_empty() {
                        return (k, 1u8, 0u32);
                    }
                    ch.clear();
                    ch.extend(mv.iter().map(|&(f, t)| codec.key(&b.apply_move(f, t))));
                    ch.sort_unstable();
                    ch.dedup();
                    (k, 2u8, ch.len() as u32)
                },
            )
            .collect();
        for (k, kind, od) in results {
            match kind {
                0 => win_w.put(k)?,
                1 => lose_w.put(k)?,
                _ => {
                    unk_w.put(k)?;
                    od_w.put(k, od as u128)?;
                }
            }
        }
    }
    win_w.finish()?;
    lose_w.finish()?;
    unk_w.finish()?;
    od_w.finish()
}

/// `(parent, count)` runs from a sorted key file (duplicates kept).
fn count_consecutive(infile: &Path, out: &Path, kb: usize) -> io::Result<()> {
    let mut r = KeyReader::open(infile, kb)?;
    let mut w = PairWriter::create(out, kb)?;
    let mut cur = r.next();
    while let Some(k) = cur {
        let mut cnt: u128 = 1;
        loop {
            match r.next() {
                Some(x) if x == k => cnt += 1,
                other => {
                    cur = other;
                    break;
                }
            }
        }
        w.put(k, cnt)?;
    }
    w.finish()
}

/// `out[parent] = a[parent] + b[parent]` (both sorted by parent).
fn merge_add_counts(a: &Path, b: &Path, out: &Path, kb: usize) -> io::Result<()> {
    let mut ra = PairReader::open(a, kb)?;
    let mut rb = PairReader::open(b, kb)?;
    let mut w = PairWriter::create(out, kb)?;
    let mut ca = ra.next();
    let mut cb = rb.next();
    loop {
        match (ca, cb) {
            (Some((pa, va)), Some((pb, vb))) => {
                if pa < pb {
                    w.put(pa, va)?;
                    ca = ra.next();
                } else if pa > pb {
                    w.put(pb, vb)?;
                    cb = rb.next();
                } else {
                    w.put(pa, va + vb)?;
                    ca = ra.next();
                    cb = rb.next();
                }
            }
            (Some((pa, va)), None) => {
                w.put(pa, va)?;
                ca = ra.next();
            }
            (None, Some((pb, vb))) => {
                w.put(pb, vb)?;
                cb = rb.next();
            }
            (None, None) => break,
        }
    }
    w.finish()
}

/// Emit parent keys where `wincount[parent] == outdeg[parent]`.
fn parents_eq_outdeg(wincount: &Path, outdeg: &Path, out: &Path, kb: usize) -> io::Result<()> {
    let mut rw = PairReader::open(wincount, kb)?;
    let mut ro = PairReader::open(outdeg, kb)?;
    let mut w = keywriter(out, kb)?;
    let mut cw = rw.next();
    let mut co = ro.next();
    loop {
        match (cw, co) {
            (Some((pw, vc)), Some((po, deg))) => {
                if pw < po {
                    cw = rw.next();
                } else if pw > po {
                    co = ro.next();
                } else {
                    if vc == deg {
                        w.put(pw)?;
                    }
                    cw = rw.next();
                    co = ro.next();
                }
            }
            _ => break,
        }
    }
    w.finish()
}

/// `out` = pairs of `wincount` whose parent is NOT in `keyset`.
fn pair_subtract_keys(wincount: &Path, keyset: &Path, out: &Path, kb: usize) -> io::Result<()> {
    let mut a = PairReader::open(wincount, kb)?;
    let mut k = KeyReader::open(keyset, kb)?;
    let mut w = PairWriter::create(out, kb)?;
    let mut ca = a.next();
    let mut ck = k.next();
    loop {
        match (ca, ck) {
            (Some((pa, va)), Some(key)) => {
                if pa < key {
                    w.put(pa, va)?;
                    ca = a.next();
                } else if pa > key {
                    ck = k.next();
                } else {
                    ca = a.next();
                    ck = k.next();
                }
            }
            (Some((pa, va)), None) => {
                w.put(pa, va)?;
                ca = a.next();
            }
            _ => break,
        }
    }
    w.finish()
}

fn key_in_file(path: &Path, key: u128, kb: usize) -> io::Result<bool> {
    Ok(KeyReader::open(path, kb)?.any(|k| k == key))
}

/// Read up to `bs` keys (`bs == 0` ⇒ all) from `r` into `out`. Returns count.
fn read_block(r: &mut KeyReader, bs: usize, out: &Path, kb: usize) -> io::Result<u64> {
    let mut w = keywriter(out, kb)?;
    let mut n: u64 = 0;
    while bs == 0 || (n as usize) < bs {
        match r.next() {
            Some(k) => {
                w.put(k)?;
                n += 1;
            }
            None => break,
        }
    }
    w.finish()?;
    Ok(n)
}

/// Outcome of the disk retrograde.
pub struct DiskResult {
    pub value: Value,
    pub dist: u32,
    pub n_total: u64,
    pub n_win: u64,
    pub n_loss: u64,
    pub n_draw: u64,
}

// ----- resume: round-boundary checkpoint (write-ahead + atomic rename) -------
//
// The canonical state files {win,lose,unknown,wincount,fwin,flose}(.bin) + the
// static outdeg are the retrograde state between rounds. A round NEVER mutates
// them in place: it writes the next state to `*.next`, then commits atomically.
// Commit order: write `committing` marker → rename all .next→.bin → write
// `checkpoint` → delete `committing`. On startup `recover_committing` finishes
// any interrupted rename batch (the .next files are complete once `committing`
// exists), so a crash anywhere is idempotently recoverable.

const STATE_BASES: [&str; 6] = ["win", "lose", "unknown", "wincount", "fwin", "flose"];

// Test-only: stop the round loop after this many committed rounds (simulating an
// interruption at a clean round boundary). Default u32::MAX = no limit. Read on
// the loop's own thread; rayon workers never touch it.
// ENUM_LIMIT does the same for enumerate layers; PHASE_HIT is set when an enum/
// classify limit fires so `solve_disk_frontier` bails before the next phase.
thread_local! {
    static ROUND_LIMIT: std::cell::Cell<u32> = const { std::cell::Cell::new(u32::MAX) };
    static ENUM_LIMIT: std::cell::Cell<u32> = const { std::cell::Cell::new(u32::MAX) };
    static PHASE_HIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
fn round_limit() -> u32 {
    ROUND_LIMIT.with(|c| c.get())
}
fn enum_limit() -> u32 {
    ENUM_LIMIT.with(|c| c.get())
}
fn phase_hit() -> bool {
    PHASE_HIT.with(|c| c.get())
}
#[cfg(test)]
fn set_round_limit(n: u32) {
    ROUND_LIMIT.with(|c| c.set(n));
}
#[cfg(test)]
fn set_enum_limit(n: u32) {
    ENUM_LIMIT.with(|c| c.set(n));
}

struct Ckpt {
    round: u32,
    init_dist: i64, // -1 = not yet decided
    n_total: u64,
}

fn write_meta(path: &Path, c: &Ckpt) -> io::Result<()> {
    let tmp = path.with_extension("meta.tmp");
    fs::write(
        &tmp,
        format!(
            "round={}\ninit_dist={}\nn_total={}\n",
            c.round, c.init_dist, c.n_total
        ),
    )?;
    fs::rename(&tmp, path)
}

fn read_meta(path: &Path) -> Option<Ckpt> {
    let s = fs::read_to_string(path).ok()?;
    let (mut round, mut init_dist, mut n_total) = (None, -1i64, None);
    for line in s.lines() {
        if let Some(v) = line.strip_prefix("round=") {
            round = v.trim().parse().ok();
        } else if let Some(v) = line.strip_prefix("init_dist=") {
            init_dist = v.trim().parse().ok()?;
        } else if let Some(v) = line.strip_prefix("n_total=") {
            n_total = v.trim().parse().ok();
        }
    }
    Some(Ckpt {
        round: round?,
        init_dist,
        n_total: n_total?,
    })
}

/// Finish any interrupted commit (idempotent): complete .next→.bin renames, then
/// write the checkpoint the `committing` marker describes, and clear the marker.
fn recover_committing(dir: &Path) -> io::Result<()> {
    let committing = dir.join("committing");
    if let Some(c) = read_meta(&committing) {
        for b in STATE_BASES {
            let nx = dir.join(format!("{b}.next"));
            if nx.exists() {
                fs::rename(&nx, dir.join(format!("{b}.bin")))?;
            }
        }
        write_meta(&dir.join("checkpoint"), &c)?;
        let _ = fs::remove_file(&committing);
    }
    Ok(())
}

fn commit_round(dir: &Path, c: &Ckpt) -> io::Result<()> {
    let committing = dir.join("committing");
    write_meta(&committing, c)?; // write-ahead: intent recorded before any rename
    for b in STATE_BASES {
        let nx = dir.join(format!("{b}.next"));
        if nx.exists() {
            fs::rename(&nx, dir.join(format!("{b}.bin")))?;
        }
    }
    write_meta(&dir.join("checkpoint"), c)?;
    let _ = fs::remove_file(&committing);
    Ok(())
}

// ----- resume: enumerate layer-boundary checkpoint --------------------------
// Same write-ahead + atomic rename as the retrograde rounds. The external-sort
// path commits {visited.bin, frontier.bin}; the bitset path commits
// {visited.bits, frontier.bin} at layer boundaries and writes final visited.bin
// only at fixpoint. Staging files are renamed under an `enum.committing` marker
// so an interrupted commit is finished idempotently on the next start.
struct EnumCkpt {
    layer: u32,
    done: bool,
}
const ENUM_BASES: [(&str, &str); 3] = [
    ("visited.next", "visited.bin"),
    ("visited.bits.next", "visited.bits"),
    ("frontier.next", "frontier.bin"),
];

fn write_enum_meta(path: &Path, c: &EnumCkpt) -> io::Result<()> {
    let tmp = path.with_extension("etmp");
    fs::write(
        &tmp,
        format!("elayer={}\nedone={}\n", c.layer, c.done as u8),
    )?;
    fs::rename(&tmp, path)
}
fn read_enum_meta(path: &Path) -> Option<EnumCkpt> {
    let s = fs::read_to_string(path).ok()?;
    let (mut layer, mut done) = (None, false);
    for line in s.lines() {
        if let Some(v) = line.strip_prefix("elayer=") {
            layer = v.trim().parse().ok();
        } else if let Some(v) = line.strip_prefix("edone=") {
            done = v.trim() == "1";
        }
    }
    Some(EnumCkpt {
        layer: layer?,
        done,
    })
}
/// Finish any interrupted enumerate-layer commit (idempotent).
fn recover_enum(dir: &Path) -> io::Result<()> {
    let committing = dir.join("enum.committing");
    if let Some(c) = read_enum_meta(&committing) {
        for (nx, fin) in ENUM_BASES {
            let n = dir.join(nx);
            if n.exists() {
                fs::rename(&n, dir.join(fin))?;
            }
        }
        write_enum_meta(&dir.join("enum.checkpoint"), &c)?;
        let _ = fs::remove_file(&committing);
    }
    Ok(())
}
fn enum_commit(dir: &Path, c: &EnumCkpt) -> io::Result<()> {
    let committing = dir.join("enum.committing");
    write_enum_meta(&committing, c)?; // write-ahead
    for (nx, fin) in ENUM_BASES {
        let n = dir.join(nx);
        if n.exists() {
            fs::rename(&n, dir.join(fin))?;
        }
    }
    write_enum_meta(&dir.join("enum.checkpoint"), c)?;
    let _ = fs::remove_file(&committing);
    Ok(())
}

/// Frontier (predecessor) disk retrograde, sequential, **resumable**. `compress`
/// selects the 8-byte rank codec; `block_size` bounds the predecessor delta
/// (0 = whole). If a `checkpoint` exists under `dir`, resumes from it (skipping
/// enumeration/classification); otherwise starts fresh. Crash-safe at round
/// boundaries (see the checkpoint machinery above).
pub fn solve_disk_frontier(
    rows: usize,
    cols: usize,
    base_cfg: &ExtConfig,
    block_size: usize,
    compress: bool,
    dir: &Path,
) -> io::Result<DiskResult> {
    fs::create_dir_all(dir)?;
    PHASE_HIT.with(|c| c.set(false)); // clear any stale test-hook flag from a prior call
    let codec = Codec::new(compress, rows, cols);
    let kb = codec.kb;
    let cfg = codec.cfg(base_cfg);
    let sortdir = dir.join("sort");

    let win = dir.join("win.bin");
    let lose = dir.join("lose.bin");
    let unknown = dir.join("unknown.bin");
    let outdeg = dir.join("outdeg.bin");
    let fwin = dir.join("fwin.bin");
    let flose = dir.join("flose.bin");
    let wincount = dir.join("wincount.bin");
    // next-state (staging) files
    let win_n = dir.join("win.next");
    let lose_n = dir.join("lose.next");
    let unknown_n = dir.join("unknown.next");
    let wincount_n = dir.join("wincount.next");
    let fwin_n = dir.join("fwin.next");
    let flose_n = dir.join("flose.next");
    // scratch
    let cand_win = dir.join("cand_win.bin");
    let cand_win_acc = dir.join("cand_win_acc.bin");
    let blk = dir.join("blk.bin");
    let new_win = dir.join("new_win.bin");
    let delta_keys = dir.join("delta_keys.bin");
    let delta = dir.join("delta.bin");
    let wc_work = dir.join("wincount.work");
    let wc_work2 = dir.join("wincount.work2");
    let reached = dir.join("reached.bin");
    let new_lose0 = dir.join("new_lose0.bin");
    let new_lose = dir.join("new_lose.bin");
    let tmp = dir.join("tmp.bin");
    let tmpp = dir.join("tmpp.bin");
    let init = codec.key(&VarBoard::initial(rows, cols));
    let t_loop = std::time::Instant::now();

    // ----- resume or fresh start -----
    recover_committing(dir)?;
    let checkpoint = dir.join("checkpoint");
    let (mut round, mut init_dist, n_total) = match read_meta(&checkpoint) {
        Some(c) => (c.round, c.init_dist, c.n_total), // resume: state files already on disk
        None => {
            let en = enumerate(codec, base_cfg, dir)?;
            if phase_hit() {
                // Test hook: enumerate was interrupted (ENUM_LIMIT). Bail before
                // classify; the canonical state files are not built yet. Resuming
                // (a fresh call on the same dir) continues enumeration.
                return Ok(DiskResult {
                    value: Value::Draw,
                    dist: 0,
                    n_total: en.total,
                    n_win: 0,
                    n_loss: 0,
                    n_draw: 0,
                });
            }
            let n_total = en.total;
            classify_build(codec, &en.visited, &win, &lose, &unknown, &outdeg)?;
            fs::copy(&win, &fwin)?;
            fs::copy(&lose, &flose)?;
            PairWriter::create(&wincount, kb)?.finish()?;
            let id = if key_in_file(&win, init, kb)? || key_in_file(&lose, init, kb)? {
                0
            } else {
                -1
            };
            write_meta(
                &checkpoint,
                &Ckpt {
                    round: 1,
                    init_dist: id,
                    n_total,
                },
            )?;
            (1, id, n_total)
        }
    };

    loop {
        // --- Win step (blocked): predecessors of lose-frontier → new_win. ---
        keywriter(&cand_win_acc, kb)?.finish()?;
        {
            let mut fr = KeyReader::open(&flose, kb)?;
            loop {
                if read_block(&mut fr, block_size, &blk, kb)? == 0 {
                    break;
                }
                extsort::sort_dedup(ParGen::preds(&blk, codec)?, &cfg, &sortdir, &cand_win)?;
                extsort::merge2(&cand_win_acc, &cand_win, MergeOp::Union, &tmp, kb)?;
                fs::rename(&tmp, &cand_win_acc)?;
            }
        }
        extsort::merge2(&cand_win_acc, &unknown, MergeOp::Intersect, &new_win, kb)?;

        // --- Lose step (blocked): win-frontier preds → win-counts in `wc_work`
        // (canonical `wincount` is read-only until commit). ---
        {
            let mut fr = KeyReader::open(&fwin, kb)?;
            let mut first = true;
            loop {
                if read_block(&mut fr, block_size, &blk, kb)? == 0 {
                    break;
                }
                extsort::sort_keep(ParGen::preds(&blk, codec)?, &cfg, &sortdir, &delta_keys)?;
                count_consecutive(&delta_keys, &delta, kb)?;
                let src = if first { &wincount } else { &wc_work };
                merge_add_counts(src, &delta, &wc_work2, kb)?;
                fs::rename(&wc_work2, &wc_work)?;
                first = false;
            }
            if first {
                fs::copy(&wincount, &wc_work)?; // no win-frontier this round
            }
        }
        parents_eq_outdeg(&wc_work, &outdeg, &reached, kb)?;
        extsort::merge2(&reached, &unknown, MergeOp::Intersect, &new_lose0, kb)?;
        extsort::merge2(&new_lose0, &new_win, MergeOp::Subtract, &new_lose, kb)?;

        let nw = count(&new_win, kb)?;
        let nl = count(&new_lose, kb)?;
        let added = nw + nl;
        if added == 0 {
            break; // fixpoint reached; canonical files hold the final partition
        }
        if init_dist == -1
            && (key_in_file(&new_win, init, kb)? || key_in_file(&new_lose, init, kb)?)
        {
            init_dist = round as i64;
        }
        // Per-round (per-ply) progress: frontier deltas = the DTM profile. Cheap
        // (new_win/new_lose are small frontier deltas, not the full partition).
        eprintln!(
            "[round {round}] +win={nw} +lose={nl} added={added} init_dist={} elapsed={:.0}s",
            if init_dist < 0 {
                "-".into()
            } else {
                init_dist.to_string()
            },
            t_loop.elapsed().as_secs_f64()
        );

        // --- Build next state into .next (no canonical mutation), then commit. ---
        extsort::merge2(&win, &new_win, MergeOp::Union, &win_n, kb)?;
        extsort::merge2(&lose, &new_lose, MergeOp::Union, &lose_n, kb)?;
        extsort::merge2(&unknown, &new_win, MergeOp::Subtract, &tmp, kb)?;
        extsort::merge2(&tmp, &new_lose, MergeOp::Subtract, &unknown_n, kb)?;
        pair_subtract_keys(&wc_work, &new_win, &tmpp, kb)?;
        pair_subtract_keys(&tmpp, &new_lose, &wincount_n, kb)?;
        fs::rename(&new_win, &fwin_n)?;
        fs::rename(&new_lose, &flose_n)?;

        commit_round(
            dir,
            &Ckpt {
                round: round + 1,
                init_dist,
                n_total,
            },
        )?;
        round += 1;

        if round > round_limit() {
            // Test hook: simulate interruption after a clean commit. The result
            // is a placeholder; resuming (a fresh call on the same dir) completes.
            return Ok(DiskResult {
                value: Value::Draw,
                dist: 0,
                n_total,
                n_win: count(&win, kb)?,
                n_loss: count(&lose, kb)?,
                n_draw: count(&unknown, kb)?,
            });
        }
    }

    let n_win = count(&win, kb)?;
    let n_loss = count(&lose, kb)?;
    let n_draw = count(&unknown, kb)?;
    let dist = if init_dist < 0 { 0 } else { init_dist as u32 };
    let value = if key_in_file(&win, init, kb)? {
        Value::Win
    } else if key_in_file(&lose, init, kb)? {
        Value::Loss
    } else {
        Value::Draw
    };

    Ok(DiskResult {
        value,
        dist: if value == Value::Draw { 0 } else { dist },
        n_total,
        n_win,
        n_loss,
        n_draw,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::retro::solve_ram;

    fn tmpdir(tag: &str) -> PathBuf {
        let mut d = std::env::temp_dir();
        d.push(format!("nocca_disk_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    /// Reference: the original sequential `classify_build` (pre-parallelization).
    /// Kept test-only so `classify_parallel_matches_sequential` can prove the
    /// parallel version is byte-for-byte identical.
    fn classify_build_seq(
        codec: Codec,
        visited: &Path,
        win: &Path,
        lose: &Path,
        unknown: &Path,
        outdeg: &Path,
    ) -> io::Result<()> {
        let kb = codec.kb;
        let mut win_w = keywriter(win, kb)?;
        let mut lose_w = keywriter(lose, kb)?;
        let mut unk_w = keywriter(unknown, kb)?;
        let mut od_w = PairWriter::create(outdeg, kb)?;
        let mut mv = Vec::new();
        let mut vis = KeyReader::open(visited, kb)?;
        while let Some(k) = vis.next() {
            let b = codec.board(k);
            if b.try_win() {
                win_w.put(k)?;
                continue;
            }
            b.moves(&mut mv, true);
            if mv.is_empty() {
                lose_w.put(k)?;
                continue;
            }
            let mut ch: Vec<u128> = mv
                .iter()
                .map(|&(f, t)| codec.key(&b.apply_move(f, t)))
                .collect();
            ch.sort_unstable();
            ch.dedup();
            unk_w.put(k)?;
            od_w.put(k, ch.len() as u128)?;
        }
        win_w.finish()?;
        lose_w.finish()?;
        unk_w.finish()?;
        od_w.finish()
    }

    /// The parallel `classify_build` must produce win/lose/unknown/outdeg files
    /// byte-for-byte identical to the sequential reference (same classification,
    /// same per-state outdeg, same sorted order), for both key widths. This is the
    /// "parallel == sequential, deterministic" guarantee the production run relies on.
    #[test]
    fn classify_parallel_matches_sequential() {
        fn read(p: &Path) -> Vec<u8> {
            fs::read(p).unwrap()
        }
        for &(r, c) in &[(3usize, 2usize), (3, 3), (4, 3)] {
            for &compress in &[false, true] {
                let dir = tmpdir(&format!("cls_{r}x{c}_{compress}"));
                let cfg = ExtConfig::new(100_000, 8);
                let codec = Codec::new(compress, r, c);
                let en = enumerate(codec, &cfg, &dir).unwrap();
                let p = |n: &str| dir.join(n);
                classify_build(
                    codec,
                    &en.visited,
                    &p("w_p"),
                    &p("l_p"),
                    &p("u_p"),
                    &p("o_p"),
                )
                .unwrap();
                classify_build_seq(
                    codec,
                    &en.visited,
                    &p("w_s"),
                    &p("l_s"),
                    &p("u_s"),
                    &p("o_s"),
                )
                .unwrap();
                assert_eq!(
                    read(&p("w_p")),
                    read(&p("w_s")),
                    "{r}x{c} c={compress}: win"
                );
                assert_eq!(
                    read(&p("l_p")),
                    read(&p("l_s")),
                    "{r}x{c} c={compress}: lose"
                );
                assert_eq!(
                    read(&p("u_p")),
                    read(&p("u_s")),
                    "{r}x{c} c={compress}: unknown"
                );
                assert_eq!(
                    read(&p("o_p")),
                    read(&p("o_s")),
                    "{r}x{c} c={compress}: outdeg"
                );
                let _ = fs::remove_dir_all(&dir);
            }
        }
    }

    /// Resume: interrupting after any round (early/mid/late) and re-invoking on
    /// the same dir must reach the IDENTICAL value/DTM/W/L/D as a one-shot run.
    #[test]
    fn resume_matches_oneshot() {
        for &(r, c) in &[(3usize, 3usize), (4, 3)] {
            let cfg = ExtConfig::new(100_000, 8);
            // one-shot reference (no interruption)
            let d1 = tmpdir(&format!("r1_{r}x{c}"));
            set_round_limit(u32::MAX);
            let want = solve_disk_frontier(r, c, &cfg, 50, true, &d1).unwrap();
            let _ = fs::remove_dir_all(&d1);

            for &stop in &[1u32, 4, 9, 14] {
                let d2 = tmpdir(&format!("r2_{r}x{c}_{stop}"));
                // run, interrupted after `stop` committed rounds (if that many exist)
                set_round_limit(stop);
                let _partial = solve_disk_frontier(r, c, &cfg, 50, true, &d2).unwrap();
                // resume to completion
                set_round_limit(u32::MAX);
                let got = solve_disk_frontier(r, c, &cfg, 50, true, &d2).unwrap();
                assert_eq!(got.value, want.value, "{r}x{c} stop={stop}: value");
                assert_eq!(
                    got.dist, want.dist,
                    "{r}x{c} stop={stop}: dist {} != {}",
                    got.dist, want.dist
                );
                assert_eq!(got.n_win, want.n_win, "{r}x{c} stop={stop}: win");
                assert_eq!(got.n_loss, want.n_loss, "{r}x{c} stop={stop}: loss");
                assert_eq!(got.n_draw, want.n_draw, "{r}x{c} stop={stop}: draw");
                let _ = fs::remove_dir_all(&d2);
            }
            set_round_limit(u32::MAX);
        }
    }

    /// Enumerate-phase resume: interrupting enumeration after any BFS layer and
    /// re-invoking on the same dir must reach the IDENTICAL reachable set
    /// (`n_total`) and value/DTM/W/L/D as a one-shot run — for both key widths.
    /// This guards against a buggy enum-resume silently dropping/duplicating
    /// reachable states (which would make the retrograde answer quietly wrong).
    #[test]
    fn enum_resume_matches_oneshot() {
        for &(r, c) in &[(3usize, 3usize), (4, 3)] {
            for &compress in &[false, true] {
                let cfg = ExtConfig::new(100_000, 8);
                // one-shot reference (no interruption)
                let d1 = tmpdir(&format!("er1_{r}x{c}_{compress}"));
                set_enum_limit(u32::MAX);
                set_round_limit(u32::MAX);
                let want = solve_disk_frontier(r, c, &cfg, 50, compress, &d1).unwrap();
                let _ = fs::remove_dir_all(&d1);

                // interrupt during enumeration after `stop` committed BFS layers
                for &stop in &[1u32, 2, 3, 5] {
                    let d2 = tmpdir(&format!("er2_{r}x{c}_{compress}_{stop}"));
                    set_enum_limit(stop);
                    set_round_limit(u32::MAX);
                    let _partial = solve_disk_frontier(r, c, &cfg, 50, compress, &d2).unwrap();
                    // resume to completion (no enum limit)
                    set_enum_limit(u32::MAX);
                    let got = solve_disk_frontier(r, c, &cfg, 50, compress, &d2).unwrap();
                    assert_eq!(
                        got.n_total, want.n_total,
                        "{r}x{c} c={compress} es={stop}: reachable"
                    );
                    assert_eq!(
                        got.value, want.value,
                        "{r}x{c} c={compress} es={stop}: value"
                    );
                    assert_eq!(got.dist, want.dist, "{r}x{c} c={compress} es={stop}: dist");
                    assert_eq!(got.n_win, want.n_win, "{r}x{c} c={compress} es={stop}: win");
                    assert_eq!(
                        got.n_loss, want.n_loss,
                        "{r}x{c} c={compress} es={stop}: loss"
                    );
                    assert_eq!(
                        got.n_draw, want.n_draw,
                        "{r}x{c} c={compress} es={stop}: draw"
                    );
                    let _ = fs::remove_dir_all(&d2);
                }
                set_enum_limit(u32::MAX);
            }
        }
    }

    /// A `committing` marker (mid-commit crash) is idempotently completed:
    /// pending `.next`→`.bin` renames finish, the checkpoint is written, and the
    /// marker is cleared — including when some renames already happened.
    #[test]
    fn committing_recovery_is_idempotent() {
        for &drop_one in &[false, true] {
            let dir = tmpdir(&format!("commit_{drop_one}"));
            fs::create_dir_all(&dir).unwrap();
            for b in STATE_BASES {
                fs::write(dir.join(format!("{b}.bin")), b"OLD").unwrap();
                fs::write(dir.join(format!("{b}.next")), b"NEW").unwrap();
            }
            if drop_one {
                // simulate a crash after the first rename already happened
                fs::rename(dir.join("win.next"), dir.join("win.bin")).unwrap();
            }
            write_meta(
                &dir.join("committing"),
                &Ckpt {
                    round: 7,
                    init_dist: 5,
                    n_total: 123,
                },
            )
            .unwrap();
            recover_committing(&dir).unwrap();
            for b in STATE_BASES {
                assert_eq!(
                    fs::read(dir.join(format!("{b}.bin"))).unwrap(),
                    b"NEW",
                    "{b}: not committed"
                );
                assert!(!dir.join(format!("{b}.next")).exists(), "{b}.next remains");
            }
            let c = read_meta(&dir.join("checkpoint")).unwrap();
            assert_eq!((c.round, c.init_dist, c.n_total), (7, 5, 123));
            assert!(!dir.join("committing").exists());
            let _ = fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn enumerate_matches_solve_ram() {
        for &(r, c) in &[(3usize, 2usize), (3, 3), (4, 3)] {
            for &compress in &[false, true] {
                let dir = tmpdir(&format!("e{r}x{c}_{compress}"));
                let cfg = ExtConfig::new(64, 3);
                let e = enumerate(Codec::new(compress, r, c), &cfg, &dir).unwrap();
                let want = solve_ram(r, c, true).n_total as u64;
                assert_eq!(e.total, want, "{r}x{c} compress={compress}: reachable");
                let _ = fs::remove_dir_all(&dir);
            }
        }
    }

    /// Frontier retrograde, plain AND compressed, vs solve_ram: value + DTM +
    /// W/L/D. Multiple block sizes force block boundaries; run_size=64 forces
    /// multi-pass merges.
    #[test]
    fn frontier_matches_solve_ram() {
        for &(r, c, blocks) in &[
            (3usize, 2usize, &[0usize, 1, 7][..]),
            (3, 3, &[0, 1, 13]),
            (4, 3, &[0, 13, 1000]),
        ] {
            let want = solve_ram(r, c, true);
            for &compress in &[false, true] {
                for &bs in blocks {
                    let dir = tmpdir(&format!("f{r}x{c}_{compress}_{bs}"));
                    let cfg = ExtConfig::new(64, 3);
                    let got = solve_disk_frontier(r, c, &cfg, bs, compress, &dir).unwrap();
                    assert_eq!(got.value, want.value, "{r}x{c} c={compress} bs={bs}: value");
                    assert_eq!(
                        got.dist, want.dist,
                        "{r}x{c} c={compress} bs={bs}: dist {} != {}",
                        got.dist, want.dist
                    );
                    assert_eq!(
                        got.n_win, want.n_win as u64,
                        "{r}x{c} c={compress} bs={bs}: win"
                    );
                    assert_eq!(
                        got.n_loss, want.n_loss as u64,
                        "{r}x{c} c={compress} bs={bs}: loss"
                    );
                    assert_eq!(
                        got.n_draw, want.n_draw as u64,
                        "{r}x{c} c={compress} bs={bs}: draw"
                    );
                    let _ = fs::remove_dir_all(&dir);
                }
            }
        }
    }

    /// Larger validation vs the in-RAM oracle (slow). Run with --ignored.
    #[test]
    #[ignore]
    fn disk_large() {
        for &(r, c) in &[(4usize, 4usize)] {
            for &compress in &[false, true] {
                let dir = tmpdir(&format!("L{r}x{c}_{compress}"));
                let cfg = ExtConfig::new(2_000_000, 8);
                let got = solve_disk_frontier(r, c, &cfg, 5_000_000, compress, &dir).unwrap();
                let want = solve_ram(r, c, true);
                assert_eq!(got.value, want.value);
                assert_eq!(got.dist, want.dist);
                assert_eq!(got.n_win, want.n_win as u64);
                assert_eq!(got.n_loss, want.n_loss as u64);
                assert_eq!(got.n_draw, want.n_draw as u64);
                println!(
                    "{r}x{c} compress={compress}: OK ({:?} dist={})",
                    got.value, got.dist
                );
                let _ = fs::remove_dir_all(&dir);
            }
        }
    }
}
