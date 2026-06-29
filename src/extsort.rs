//! Minimal external-memory primitives over `u128` keys for the disk-based
//! retrograde: sort+dedup of a key stream, and streaming 2-way set merges
//! (union / subtract / intersect).
//!
//! Keys are always `u128` IN MEMORY (sort/compare/merge logic is width-agnostic
//! and unchanged), but only the low `kbytes` bytes are written/read on disk. So
//! the compressed retrograde stores 8-byte ranks (high bytes zero) through the
//! exact same, already-validated logic — only the serialization width differs.
//!
//! `run_size`/`fanin` are configurable so tests can force multi-pass merges on
//! small inputs (the out-of-core path a single in-RAM sort would hide).

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

const IO_BUF_BYTES: usize = 4 * 1024 * 1024;

// Cumulative wall-nanos across a solve. `GEN_NANOS` ≈ key-generation time (the
// feed loop; ≈ pure gen at large run_size since no mid-loop flush); `SORT_NANOS`
// = in-RAM `sort_unstable` time. The remainder of the wall time is merge I/O.
pub static SORT_NANOS: AtomicU64 = AtomicU64::new(0);
pub static GEN_NANOS: AtomicU64 = AtomicU64::new(0);
pub fn reset_profile() {
    SORT_NANOS.store(0, Ordering::Relaxed);
    GEN_NANOS.store(0, Ordering::Relaxed);
}
pub fn sort_secs() -> f64 {
    SORT_NANOS.load(Ordering::Relaxed) as f64 / 1e9
}
pub fn gen_secs() -> f64 {
    GEN_NANOS.load(Ordering::Relaxed) as f64 / 1e9
}

#[derive(Clone, Copy)]
pub struct ExtConfig {
    /// Records sorted in RAM per run (small in tests to force many runs).
    pub run_size: usize,
    /// Max run files merged in one pass (≥2; small in tests to force passes).
    pub fanin: usize,
    /// Bytes per key written to disk (low bytes of the u128; 16 = plain key,
    /// 8 = compressed rank). Must cover the key range losslessly.
    pub kbytes: usize,
}

impl ExtConfig {
    /// Plain 16-byte keys.
    pub fn new(run_size: usize, fanin: usize) -> Self {
        Self::with_kbytes(run_size, fanin, 16)
    }
    pub fn with_kbytes(run_size: usize, fanin: usize, kbytes: usize) -> Self {
        assert!(run_size >= 1 && fanin >= 2 && (1..=16).contains(&kbytes));
        ExtConfig {
            run_size,
            fanin,
            kbytes,
        }
    }
}

/// Streaming reader over a binary file of little-endian keys, `bytes` per record.
pub struct KeyReader {
    rdr: BufReader<File>,
    bytes: usize,
}

impl KeyReader {
    pub fn open(path: &Path, bytes: usize) -> io::Result<Self> {
        Ok(KeyReader {
            rdr: BufReader::with_capacity(IO_BUF_BYTES, File::open(path)?),
            bytes,
        })
    }
}

impl Iterator for KeyReader {
    type Item = u128;
    fn next(&mut self) -> Option<u128> {
        let mut buf = [0u8; 16];
        match self.rdr.read_exact(&mut buf[..self.bytes]) {
            Ok(()) => Some(u128::from_le_bytes(buf)),
            Err(_) => None,
        }
    }
}

struct KeyWriter {
    w: BufWriter<File>,
    bytes: usize,
}
impl KeyWriter {
    fn create(path: &Path, bytes: usize) -> io::Result<Self> {
        Ok(KeyWriter {
            w: BufWriter::with_capacity(IO_BUF_BYTES, File::create(path)?),
            bytes,
        })
    }
    #[inline]
    fn put(&mut self, k: u128) -> io::Result<()> {
        self.w.write_all(&k.to_le_bytes()[..self.bytes])
    }
    fn finish(mut self) -> io::Result<()> {
        self.w.flush()
    }
}

/// Number of records in a key file of `bytes`-wide records.
pub fn count(path: &Path, bytes: usize) -> io::Result<u64> {
    Ok(fs::metadata(path)?.len() / bytes as u64)
}

fn flush_run(
    buf: &mut Vec<u128>,
    dir: &Path,
    idx: usize,
    dedup: bool,
    bytes: usize,
) -> io::Result<PathBuf> {
    let t = Instant::now();
    buf.sort_unstable(); // sequential (sort parallelization skipped: bandwidth-bound)
    SORT_NANOS.fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
    if dedup {
        buf.dedup();
    }
    let path = dir.join(format!("run_{idx:06}.bin"));
    let mut w = KeyWriter::create(&path, bytes)?;
    for &k in buf.iter() {
        w.put(k)?;
    }
    w.finish()?;
    buf.clear();
    Ok(path)
}

/// k-way merge of sorted `inputs` into `out`. Dedups iff `dedup`.
fn kway_merge(inputs: &[PathBuf], out: &Path, dedup: bool, bytes: usize) -> io::Result<()> {
    let mut readers: Vec<KeyReader> = inputs
        .iter()
        .map(|p| KeyReader::open(p, bytes))
        .collect::<io::Result<_>>()?;
    let mut heap: BinaryHeap<Reverse<(u128, usize)>> = BinaryHeap::new();
    for (i, r) in readers.iter_mut().enumerate() {
        if let Some(k) = r.next() {
            heap.push(Reverse((k, i)));
        }
    }
    let mut w = KeyWriter::create(out, bytes)?;
    let mut last: Option<u128> = None;
    while let Some(Reverse((k, i))) = heap.pop() {
        if !dedup || last != Some(k) {
            w.put(k)?;
            last = Some(k);
        }
        if let Some(nk) = readers[i].next() {
            heap.push(Reverse((nk, i)));
        }
    }
    w.finish()
}

/// Sort + dedup a key stream into the single sorted file `out`.
pub fn sort_dedup<I: Iterator<Item = u128>>(
    items: I,
    cfg: &ExtConfig,
    dir: &Path,
    out: &Path,
) -> io::Result<()> {
    sort(items, cfg, dir, out, true)
}

/// Sort a key stream, **keeping duplicates**, into `out` (for counting).
pub fn sort_keep<I: Iterator<Item = u128>>(
    items: I,
    cfg: &ExtConfig,
    dir: &Path,
    out: &Path,
) -> io::Result<()> {
    sort(items, cfg, dir, out, false)
}

/// Sort a key stream into `out`. Creates runs of `run_size`, then merges `fanin`
/// at a time (multiple passes if needed). Dedups iff `dedup`.
pub fn sort<I: Iterator<Item = u128>>(
    items: I,
    cfg: &ExtConfig,
    dir: &Path,
    out: &Path,
    dedup: bool,
) -> io::Result<()> {
    let b = cfg.kbytes;
    fs::create_dir_all(dir)?;
    let mut runs: Vec<PathBuf> = Vec::new();
    let mut buf: Vec<u128> = Vec::with_capacity(cfg.run_size);
    let t_feed = Instant::now();
    for k in items {
        buf.push(k);
        if buf.len() >= cfg.run_size {
            runs.push(flush_run(&mut buf, dir, runs.len(), dedup, b)?);
        }
    }
    // ≈ pure generation time at large run_size (no mid-loop flush). Subtracts
    // any flush that did happen via SORT_NANOS being tracked separately.
    GEN_NANOS.fetch_add(t_feed.elapsed().as_nanos() as u64, Ordering::Relaxed);
    if !buf.is_empty() {
        runs.push(flush_run(&mut buf, dir, runs.len(), dedup, b)?);
    }
    if runs.is_empty() {
        KeyWriter::create(out, b)?.finish()?;
        return Ok(());
    }
    let mut pass = 0;
    while runs.len() > 1 {
        let mut next: Vec<PathBuf> = Vec::new();
        for (g, group) in runs.chunks(cfg.fanin).enumerate() {
            let merged = dir.join(format!("merge_{pass}_{g:06}.bin"));
            kway_merge(group, &merged, dedup, b)?;
            next.push(merged);
        }
        for p in &runs {
            let _ = fs::remove_file(p);
        }
        runs = next;
        pass += 1;
    }
    fs::rename(&runs[0], out).or_else(|_| {
        fs::copy(&runs[0], out).map(|_| ())?;
        let _ = fs::remove_file(&runs[0]);
        Ok(())
    })
}

#[derive(Clone, Copy, PartialEq)]
pub enum MergeOp {
    Union,
    Subtract,
    Intersect,
}

/// Streaming 2-way set merge of sorted+deduped `a`, `b` into `out`. `bytes` is
/// the on-disk key width for all three files.
pub fn merge2(a: &Path, b: &Path, op: MergeOp, out: &Path, bytes: usize) -> io::Result<()> {
    let mut ra = KeyReader::open(a, bytes)?;
    let mut rb = KeyReader::open(b, bytes)?;
    let mut w = KeyWriter::create(out, bytes)?;
    let mut ca = ra.next();
    let mut cb = rb.next();
    loop {
        match (ca, cb) {
            (Some(x), Some(y)) => {
                if x < y {
                    if op == MergeOp::Union || op == MergeOp::Subtract {
                        w.put(x)?;
                    }
                    ca = ra.next();
                } else if x > y {
                    if op == MergeOp::Union {
                        w.put(y)?;
                    }
                    cb = rb.next();
                } else {
                    if op == MergeOp::Union || op == MergeOp::Intersect {
                        w.put(x)?;
                    }
                    ca = ra.next();
                    cb = rb.next();
                }
            }
            (Some(x), None) => {
                if op == MergeOp::Union || op == MergeOp::Subtract {
                    w.put(x)?;
                }
                ca = ra.next();
            }
            (None, Some(y)) => {
                if op == MergeOp::Union {
                    w.put(y)?;
                }
                cb = rb.next();
            }
            (None, None) => break,
        }
    }
    w.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn tmp(name: &str) -> PathBuf {
        let mut d = std::env::temp_dir();
        d.push(format!("nocca_extsort_test_{name}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn read_all(p: &Path, bytes: usize) -> Vec<u128> {
        KeyReader::open(p, bytes).unwrap().collect()
    }

    #[test]
    fn sort_dedup_forces_multipass_and_is_correct() {
        for &kb in &[16usize, 8] {
            let dir = tmp(&format!("sd{kb}"));
            let mut raw: Vec<u128> = Vec::new();
            let mut x: u128 = 1;
            for _ in 0..1000 {
                x = x
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                raw.push(x % 137);
            }
            let expected: BTreeSet<u128> = raw.iter().copied().collect();
            let cfg = ExtConfig::with_kbytes(3, 2, kb);
            let out = dir.join("out.bin");
            sort_dedup(raw.into_iter(), &cfg, &dir, &out).unwrap();
            let got = read_all(&out, kb);
            assert!(got.windows(2).all(|w| w[0] < w[1]));
            assert_eq!(got, expected.into_iter().collect::<Vec<_>>());
            let _ = fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn merge_ops() {
        let dir = tmp("merge");
        let cfg = ExtConfig::with_kbytes(4, 2, 8);
        let a = dir.join("a.bin");
        let b = dir.join("b.bin");
        sort_dedup([1u128, 3, 5, 7, 9].into_iter(), &cfg, &dir, &a).unwrap();
        sort_dedup([3u128, 4, 5, 6].into_iter(), &cfg, &dir, &b).unwrap();
        let u = dir.join("u.bin");
        merge2(&a, &b, MergeOp::Union, &u, 8).unwrap();
        assert_eq!(read_all(&u, 8), vec![1, 3, 4, 5, 6, 7, 9]);
        let s = dir.join("s.bin");
        merge2(&a, &b, MergeOp::Subtract, &s, 8).unwrap();
        assert_eq!(read_all(&s, 8), vec![1, 7, 9]);
        let i = dir.join("i.bin");
        merge2(&a, &b, MergeOp::Intersect, &i, 8).unwrap();
        assert_eq!(read_all(&i, 8), vec![3, 5]);
        let _ = fs::remove_dir_all(&dir);
    }
}
