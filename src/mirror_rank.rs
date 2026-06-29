//! Dense bijective ranking over **mirror representatives** (rank-min canonical),
//! for the planned all-in-memory 2-bit-array retrograde.
//!
//! INDEPENDENT codepath: this does NOT touch `rank.rs` / `disk_retro.rs` nor the
//! key-min `canonical_key` used by the disk solver. Here the representative of a
//! mirror orbit `{b, mirror(b)}` is the **rank-min** one (smaller `VarBoard::rank`),
//! NOT key-min — chosen because `rank = place_rank(P)·NC + color_rank(C)` makes the
//! placement the dominant term, which yields a clean dense decomposition:
//!
//! * non-self-symmetric placement pair `{P0, πP0}` (rank_place(P0) < rank_place(πP0)):
//!   every one of the `NC = C(2cols,cols)` colorings of `P0` is a representative
//!   (`P0·NC + C < πP0·NC + anything`). ⇒ `NC` reps per pair.
//! * self-symmetric placement (`πP == P`): fold the colorings by the color
//!   involution `C ↦ πC`. ⇒ `(NC + Cself(P))/2` reps.
//!
//! Total reps `R_full = (M + S_full)/2` with `M = NC·NPLACE`, `S_full = Σ Cself`
//! over self-symmetric placements (Burnside). NOTE: this is the **full
//! combinatorial** fold; the paper-B figure 73,986,754,080 is the fold of the
//! *reachable* set and is checked separately (post-enumerate), not here.
//!
//! Mirror = left-right column flip (`c ↦ cols-1-c`), matching `VarBoard::mirror`.

use crate::rank::{
    binom_table, comp_table, rank_comb, rank_placement, unrank_comb_into, unrank_placement_into,
};
use crate::varboard::VarBoard;

/// Per-size combinatorial context (owns the comp/binom tables).
struct Ctx {
    rows: usize,
    cols: usize,
    ncells: usize,
    total: usize,
    nc: u64,     // C(2cols, cols) — number of colorings
    nplace: u64, // comp[ncells][total] — number of placements
    pi: Vec<usize>, // cell mirror permutation: mirrored[j] = heights[pi[j]]
    comp: Vec<Vec<u64>>,
    binom: Vec<Vec<u64>>,
}

impl Ctx {
    fn new(rows: usize, cols: usize) -> Ctx {
        let ncells = rows * cols;
        let total = 2 * cols;
        let comp = comp_table(ncells, total);
        let binom = binom_table(total);
        let nc = binom[total][cols];
        let nplace = comp[ncells][total];
        let mut pi = vec![0usize; ncells];
        for r in 0..rows {
            for c in 0..cols {
                pi[r * cols + c] = r * cols + (cols - 1 - c);
            }
        }
        Ctx { rows, cols, ncells, total, nc, nplace, pi, comp, binom }
    }

    /// Mirrored heights: `out[j] = heights[pi[j]]` (column flip).
    fn mirror_heights(&self, h: &[u8], out: &mut [u8]) {
        for j in 0..self.ncells {
            out[j] = h[self.pi[j]];
        }
    }

    /// Number of colorings `C` with `πC == C` for a **self-symmetric** placement.
    /// The slot involution has `f` fixed points (slots in self-paired cells, i.e.
    /// `pi[i]==i` — the center column) and `t=(total-f)/2` transpositions. A fixed
    /// coloring picks whole cycles summing to `cols` blacks: `Σ_b C(t,b)·C(f,cols-2b)`.
    fn cself(&self, h: &[u8]) -> u64 {
        let mut f = 0usize;
        for i in 0..self.ncells {
            if self.pi[i] == i {
                f += h[i] as usize;
            }
        }
        let t = (self.total - f) / 2;
        let mut s = 0u64;
        let mut b = 0usize;
        while 2 * b <= self.cols {
            let a = self.cols - 2 * b;
            if a <= f && b <= t {
                s += self.binom[t][b] * self.binom[f][a];
            }
            b += 1;
        }
        s
    }

    /// Slot permutation induced by the column flip for a placement `h`.
    /// `perm[slot(i,l)] = slot(pi[i], l)` where `slot(i,l)=slotstart[i]+l`.
    fn slotperm(&self, h: &[u8]) -> Vec<usize> {
        let mut slotstart = vec![0usize; self.ncells];
        let mut acc = 0usize;
        for i in 0..self.ncells {
            slotstart[i] = acc;
            acc += h[i] as usize;
        }
        let mut perm = vec![0usize; self.total];
        for i in 0..self.ncells {
            for l in 0..h[i] as usize {
                perm[slotstart[i] + l] = slotstart[self.pi[i]] + l;
            }
        }
        perm
    }

    /// Apply the slot permutation to a coloring rank `c` → mirrored coloring rank.
    fn mirror_color(&self, c: u64, perm: &[usize]) -> u64 {
        let mut blacks = vec![0usize; self.cols];
        unrank_comb_into(c, self.total, self.cols, &self.binom, &mut blacks);
        let mut nb: Vec<usize> = blacks.iter().map(|&s| perm[s]).collect();
        nb.sort_unstable();
        rank_comb(&nb, &self.binom)
    }
}

// ===================== step0: S_full / R_full counts =========================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    pub nplace: u64,
    pub nc: u64,
    pub m: u64,          // NC · NPLACE = full rank space
    pub nplace_self: u64,
    pub s_full: u64,     // Σ Cself over self-symmetric placements (= #self-symmetric states)
    pub r_full_a: u64,   // placement-enum rep count (weighted by actual rank comparison)
    pub r_full_b: u64,   // analytic (M + S_full)/2
}

/// step0: enumerate placements once, computing `S_full` and `R_full` two ways.
/// `r_full_a` counts representatives by the **actual** `rank(P)` vs `rank(πP)`
/// comparison (not the /2 shortcut), so `r_full_a == r_full_b` is a real check
/// that the mirror/rank pairing is exact and self-symmetry is handled correctly.
pub fn counts(rows: usize, cols: usize) -> Counts {
    let ctx = Ctx::new(rows, cols);
    let mut h = vec![0u8; ctx.ncells];
    let mut mh = vec![0u8; ctx.ncells];
    let mut nplace_self = 0u64;
    let mut s_full = 0u64;
    let mut r_full_a = 0u64;
    for p in 0..ctx.nplace {
        unrank_placement_into(p, ctx.ncells, ctx.total, &ctx.comp, &mut h);
        ctx.mirror_heights(&h, &mut mh);
        let pm = rank_placement(&mh, ctx.total, &ctx.comp);
        if pm == p {
            nplace_self += 1;
            let cs = ctx.cself(&h);
            s_full += cs;
            r_full_a += (ctx.nc + cs) / 2;
        } else if p < pm {
            r_full_a += ctx.nc;
        }
    }
    let m = ctx.nc * ctx.nplace;
    let r_full_b = (m + s_full) / 2;
    Counts { nplace: ctx.nplace, nc: ctx.nc, m, nplace_self, s_full, r_full_a, r_full_b }
}

/// Independent cross-check by **full state enumeration** (small sizes only):
/// returns `(#representatives, #self-symmetric states)` by ranking every state.
pub fn counts_by_states(rows: usize, cols: usize) -> (u64, u64) {
    let ctx = Ctx::new(rows, cols);
    let m = ctx.nc * ctx.nplace;
    let mut reps = 0u64;
    let mut selfs = 0u64;
    for r in 0..m {
        let b = VarBoard::unrank(rows, cols, r);
        let rm = b.mirror().rank();
        if rm == r {
            selfs += 1;
            reps += 1;
        } else if r < rm {
            reps += 1;
        }
    }
    (reps, selfs)
}

// ===================== MirrorRanker (dense rank/unrank) ======================

/// Dense bijection between mirror representatives and `[0, R_full)`.
/// Built once per `(rows, cols)`; `mirror_rank`/`mirror_unrank` are then O(cells)
/// (plus O(NC) only for the rare self-symmetric-placement color fold).
pub struct MirrorRanker {
    ctx: Ctx,
    isrep: Vec<u64>,    // bit p set ⇔ placement p is a placement-rep (rank(p) ≤ rank(πp))
    cumword: Vec<u64>,  // cumword[w] = popcount of isrep[0..w]
    nrep_place: u64,
    self_repidx: Vec<u64>,       // ascending place-rep indices that are self-symmetric
    self_cself: Vec<u64>,        // Cself for each (parallel to self_repidx)
    self_corr_prefix: Vec<u64>,  // prefix sum of (NC - Cself)/2 before each self entry
    r_full: u64,
    // Lever 5 (math-preserving): coarse index for the mirror_unrank fold search.
    // `k_at[idx >> UNRANK_SHIFT]` = largest place-rep k with base_offset(k) ≤
    // (bucket start idx); the true k for any idx in that bucket lies in
    // `[k_at[b], k_at[b+1]]` (≈2^SHIFT/NC wide), so the outer binary search shrinks
    // from ~log2(nrep_place) to ~log2(window). 4 B/entry, R_full/2^SHIFT entries
    // (6×5 ≈ 36 MB at SHIFT 13).
    k_at: Vec<u32>,
    // Lever 6 / ③a probe (opt-in via `build_place_mirror`): `place_mirror[p]` =
    // placement rank of the column-mirror of placement `p`. Lets mirror_rank skip
    // the losing side's rank_placement. 4 B × NPLACE (6×5 ≈ 2.35 GB, random
    // access — probing whether that DRAM traffic hurts at 56 cores).
    place_mirror: Vec<u32>,
}

const UNRANK_SHIFT: u32 = 13;

impl MirrorRanker {
    pub fn build(rows: usize, cols: usize) -> MirrorRanker {
        let ctx = Ctx::new(rows, cols);
        let nwords = ((ctx.nplace + 63) / 64) as usize;
        let mut isrep = vec![0u64; nwords];
        let mut h = vec![0u8; ctx.ncells];
        let mut mh = vec![0u8; ctx.ncells];
        let mut self_p: Vec<(u64, u64)> = Vec::new(); // (placement, cself) in placement order
        for p in 0..ctx.nplace {
            unrank_placement_into(p, ctx.ncells, ctx.total, &ctx.comp, &mut h);
            ctx.mirror_heights(&h, &mut mh);
            let pm = rank_placement(&mh, ctx.total, &ctx.comp);
            if p <= pm {
                isrep[(p / 64) as usize] |= 1u64 << (p % 64);
                if p == pm {
                    self_p.push((p, ctx.cself(&h)));
                }
            }
        }
        let mut cumword = vec![0u64; nwords + 1];
        for w in 0..nwords {
            cumword[w + 1] = cumword[w] + isrep[w].count_ones() as u64;
        }
        let nrep_place = cumword[nwords];

        let mut me = MirrorRanker {
            ctx,
            isrep,
            cumword,
            nrep_place,
            self_repidx: Vec::with_capacity(self_p.len()),
            self_cself: Vec::with_capacity(self_p.len()),
            self_corr_prefix: Vec::with_capacity(self_p.len() + 1),
            r_full: 0,
            k_at: Vec::new(),
            place_mirror: Vec::new(),
        };
        let mut corr = 0u64;
        for &(p, cs) in &self_p {
            let k = me.rank1(p);
            me.self_repidx.push(k);
            me.self_cself.push(cs);
            me.self_corr_prefix.push(corr);
            corr += (me.ctx.nc - cs) / 2;
        }
        me.self_corr_prefix.push(corr);
        me.r_full = me.ctx.nc * me.nrep_place - corr; // = base_offset(nrep_place)
        me.build_k_at();
        me
    }

    /// Build the [`k_at`](Self::k_at) coarse index: one forward pass over `k`
    /// (base_offset is monotone), tracking the self-rep count incrementally so
    /// each `base_offset` is O(1) within the pass.
    fn build_k_at(&mut self) {
        let nc = self.ctx.nc;
        let nbuckets = ((self.r_full >> UNRANK_SHIFT) + 2) as usize;
        let mut k_at = vec![0u32; nbuckets];
        let mut k = 0u64;
        let mut si = 0usize; // # self-reps with index < (k+1)
        for (b, slot) in k_at.iter_mut().enumerate() {
            let target = (b as u64) << UNRANK_SHIFT;
            loop {
                if k + 1 >= self.nrep_place {
                    break;
                }
                while si < self.self_repidx.len() && self.self_repidx[si] < k + 1 {
                    si += 1;
                }
                let bo = nc * (k + 1) - self.self_corr_prefix[si];
                if bo <= target {
                    k += 1;
                } else {
                    break;
                }
            }
            *slot = k as u32;
        }
        self.k_at = k_at;
    }

    /// Opt-in build of the [`place_mirror`](Self::place_mirror) table (③a probe).
    /// `place_mirror[p] = rank_placement(column-mirror of placement p)`.
    pub fn build_place_mirror(&mut self) {
        let n = self.ctx.nplace as usize;
        let mut pm = vec![0u32; n];
        let mut h = vec![0u8; self.ctx.ncells];
        let mut mh = vec![0u8; self.ctx.ncells];
        for (p, slot) in pm.iter_mut().enumerate() {
            unrank_placement_into(p as u64, self.ctx.ncells, self.ctx.total, &self.ctx.comp, &mut h);
            self.ctx.mirror_heights(&h, &mut mh);
            *slot = rank_placement(&mh, self.ctx.total, &self.ctx.comp) as u32;
        }
        self.place_mirror = pm;
    }

    /// **③a probe (math-preserving)**: like [`mirror_rank`] but gets the mirror's
    /// placement rank from [`place_mirror`](Self::place_mirror) (one table lookup)
    /// instead of a second `rank_placement`; only the losing side's `rank_comb`
    /// (color) is computed. Identical index. Requires `build_place_mirror` first.
    pub fn mirror_rank_fast3(&self, b: &VarBoard) -> u64 {
        let mut bb = [0usize; crate::varboard::MAX_CELLS];
        let (pb, nb) = b.place_rank_into(&mut bb);
        let pmb = self.place_mirror[pb as usize] as u64;
        let (p0, c0) = if pb < pmb {
            (pb, rank_comb(&bb[..nb], &self.ctx.binom))
        } else {
            let mut bm = [0usize; crate::varboard::MAX_CELLS];
            let nm = b.mirror().blacks_into(&mut bm);
            let cm = rank_comb(&bm[..nm], &self.ctx.binom);
            if pb > pmb {
                (pmb, cm)
            } else {
                (pb, rank_comb(&bb[..nb], &self.ctx.binom).min(cm))
            }
        };
        let k = self.rank1(p0);
        let base = self.base_offset(k);
        let csub = if self.selfsym_slot(k).is_some() {
            let mut hh = vec![0u8; self.ctx.ncells];
            unrank_placement_into(p0, self.ctx.ncells, self.ctx.total, &self.ctx.comp, &mut hh);
            let perm = self.ctx.slotperm(&hh);
            let mut sub = 0u64;
            for c in 0..c0 {
                if c <= self.ctx.mirror_color(c, &perm) {
                    sub += 1;
                }
            }
            sub
        } else {
            c0
        };
        base + csub
    }

    pub fn r_full(&self) -> u64 {
        self.r_full
    }

    /// Number of placement-reps strictly before placement `p`.
    fn rank1(&self, p: u64) -> u64 {
        let w = (p / 64) as usize;
        let mask = (1u64 << (p % 64)) - 1;
        self.cumword[w] + (self.isrep[w] & mask).count_ones() as u64
    }

    /// Placement of the `k`-th placement-rep (0-based): the k-th set bit of isrep.
    fn select1(&self, k: u64) -> u64 {
        let w = self.cumword.partition_point(|&x| x <= k) - 1;
        let mut rem = k - self.cumword[w];
        let mut wd = self.isrep[w];
        while rem > 0 {
            wd &= wd - 1;
            rem -= 1;
        }
        (w as u64) * 64 + wd.trailing_zeros() as u64
    }

    /// If place-rep `k` is self-symmetric, its index into `self_*` arrays.
    fn selfsym_slot(&self, k: u64) -> Option<usize> {
        let i = self.self_repidx.partition_point(|&x| x < k);
        if i < self.self_repidx.len() && self.self_repidx[i] == k {
            Some(i)
        } else {
            None
        }
    }

    /// Start index of place-rep `k`'s color block: `NC·k − Σ_{self reps<k}(NC−Cself)/2`.
    fn base_offset(&self, k: u64) -> u64 {
        let i = self.self_repidx.partition_point(|&x| x < k);
        self.ctx.nc * k - self.self_corr_prefix[i]
    }

    /// Dense mirror-rank of a board (same value for `b` and `mirror(b)`).
    pub fn mirror_rank(&self, b: &VarBoard) -> u64 {
        let nc = self.ctx.nc;
        let r1 = b.rank();
        let r2 = b.mirror().rank();
        let rep = r1.min(r2);
        let p0 = rep / nc;
        let c0 = rep % nc;
        let k = self.rank1(p0);
        let base = self.base_offset(k);
        let csub = if self.selfsym_slot(k).is_some() {
            // self-symmetric placement: dense rank of c0 among color-reps
            let mut hh = vec![0u8; self.ctx.ncells];
            unrank_placement_into(p0, self.ctx.ncells, self.ctx.total, &self.ctx.comp, &mut hh);
            let perm = self.ctx.slotperm(&hh);
            let mut sub = 0u64;
            for c in 0..c0 {
                if c <= self.ctx.mirror_color(c, &perm) {
                    sub += 1;
                }
            }
            sub
        } else {
            c0
        };
        base + csub
    }

    /// **Lever 4 (case A)**: like [`mirror_rank`] but computes the *color* rank
    /// only for the rank-min side. Since `rank = place·NC + color`, the side with
    /// the smaller placement rank is rank-min (color < NC can't overturn a
    /// placement difference); only on a placement *tie* (self-symmetric
    /// placement) are both colors needed. Saves one `rank_comb` per call.
    /// Representative selection is unchanged ⇒ identical index (枝刈りのみ).
    pub fn mirror_rank_fast(&self, b: &VarBoard) -> u64 {
        let mb = b.mirror();
        let mut bb = [0usize; crate::varboard::MAX_CELLS];
        let mut bm = [0usize; crate::varboard::MAX_CELLS];
        let (pb, nb) = b.place_rank_into(&mut bb);
        let (pmb, nm) = mb.place_rank_into(&mut bm);
        let (p0, c0) = match pb.cmp(&pmb) {
            std::cmp::Ordering::Less => (pb, rank_comb(&bb[..nb], &self.ctx.binom)),
            std::cmp::Ordering::Greater => (pmb, rank_comb(&bm[..nm], &self.ctx.binom)),
            std::cmp::Ordering::Equal => {
                let cb = rank_comb(&bb[..nb], &self.ctx.binom);
                let cm = rank_comb(&bm[..nm], &self.ctx.binom);
                (pb, cb.min(cm))
            }
        };
        let k = self.rank1(p0);
        let base = self.base_offset(k);
        let csub = if self.selfsym_slot(k).is_some() {
            let mut hh = vec![0u8; self.ctx.ncells];
            unrank_placement_into(p0, self.ctx.ncells, self.ctx.total, &self.ctx.comp, &mut hh);
            let perm = self.ctx.slotperm(&hh);
            let mut sub = 0u64;
            for c in 0..c0 {
                if c <= self.ctx.mirror_color(c, &perm) {
                    sub += 1;
                }
            }
            sub
        } else {
            c0
        };
        base + csub
    }

    /// Inverse of [`mirror_rank`]: the rank-min representative board for `idx`.
    pub fn mirror_unrank(&self, idx: u64) -> VarBoard {
        // largest k with base_offset(k) <= idx
        let (mut lo, mut hi) = (0u64, self.nrep_place);
        while lo + 1 < hi {
            let mid = (lo + hi) / 2;
            if self.base_offset(mid) <= idx {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        let k = lo;
        let csub = idx - self.base_offset(k);
        let p0 = self.select1(k);
        let nc = self.ctx.nc;
        let c0 = if self.selfsym_slot(k).is_some() {
            let mut hh = vec![0u8; self.ctx.ncells];
            unrank_placement_into(p0, self.ctx.ncells, self.ctx.total, &self.ctx.comp, &mut hh);
            let perm = self.ctx.slotperm(&hh);
            let mut seen = 0u64;
            let mut found = 0u64;
            for c in 0..nc {
                if c <= self.ctx.mirror_color(c, &perm) {
                    if seen == csub {
                        found = c;
                        break;
                    }
                    seen += 1;
                }
            }
            found
        } else {
            csub
        };
        VarBoard::unrank(self.ctx.rows, self.ctx.cols, p0 * nc + c0)
    }

    /// **Lever 3**: like [`mirror_unrank`] but seeds the outer binary search from
    /// an `idx/nc` estimate window instead of `[0, nrep_place)`. Since
    /// `base_offset(k) = nc·k − corr(k)` with `corr ∈ [0, CORR]`
    /// (`CORR = nc·nrep_place − R_full`), the true `k` is in
    /// `[idx/nc − 2, idx/nc + CORR/nc + 4]` (≈48k wide at 6×5 vs 294M), so the
    /// search is ~log₂(window) instead of ~log₂(nrep_place). Returns the
    /// **identical** board (math-unchanged); gated by `mirror_unrank_fast ==
    /// mirror_unrank` (exhaustive ≤4×4, sampled 4×5/5×5).
    pub fn mirror_unrank_fast(&self, idx: u64) -> VarBoard {
        let nc = self.ctx.nc;
        let corr_total = nc * self.nrep_place - self.r_full;
        let est = idx / nc;
        let mut lo = est.saturating_sub(2);
        let mut hi = (est + corr_total / nc + 4).min(self.nrep_place);
        // `base_offset(lo) ≤ idx < base_offset(hi)` holds by the bound above, so
        // the search converges to the same `k` as the full-range version.
        while lo + 1 < hi {
            let mid = lo + (hi - lo) / 2;
            if self.base_offset(mid) <= idx {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        let k = lo;
        let csub = idx - self.base_offset(k);
        let p0 = self.select1(k);
        let c0 = if self.selfsym_slot(k).is_some() {
            let mut hh = vec![0u8; self.ctx.ncells];
            unrank_placement_into(p0, self.ctx.ncells, self.ctx.total, &self.ctx.comp, &mut hh);
            let perm = self.ctx.slotperm(&hh);
            let mut seen = 0u64;
            let mut found = 0u64;
            for c in 0..nc {
                if c <= self.ctx.mirror_color(c, &perm) {
                    if seen == csub {
                        found = c;
                        break;
                    }
                    seen += 1;
                }
            }
            found
        } else {
            csub
        };
        VarBoard::unrank(self.ctx.rows, self.ctx.cols, p0 * nc + c0)
    }

    /// **Lever 5 (math-preserving)**: like [`mirror_unrank`] but seeds the outer
    /// binary search from the `k_at` coarse index (`[k_at[b], k_at[b+1]]`,
    /// ≈2^SHIFT/NC wide) instead of `[0, nrep_place)` — turning the fold search
    /// (≈85% of `mirror_unrank`) into ~log2(window) `base_offset` evals. Identical
    /// board; gated by `mirror_unrank_fast2 == mirror_unrank`.
    pub fn mirror_unrank_fast2(&self, idx: u64) -> VarBoard {
        let nc = self.ctx.nc;
        let b = (idx >> UNRANK_SHIFT) as usize;
        let mut lo = self.k_at[b] as u64;
        let mut hi = (self.k_at[b + 1] as u64 + 1).min(self.nrep_place);
        while lo + 1 < hi {
            let mid = lo + (hi - lo) / 2;
            if self.base_offset(mid) <= idx {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        let k = lo;
        let csub = idx - self.base_offset(k);
        let p0 = self.select1(k);
        let c0 = if self.selfsym_slot(k).is_some() {
            let mut hh = vec![0u8; self.ctx.ncells];
            unrank_placement_into(p0, self.ctx.ncells, self.ctx.total, &self.ctx.comp, &mut hh);
            let perm = self.ctx.slotperm(&hh);
            let mut seen = 0u64;
            let mut found = 0u64;
            for c in 0..nc {
                if c <= self.ctx.mirror_color(c, &perm) {
                    if seen == csub {
                        found = c;
                        break;
                    }
                    seen += 1;
                }
            }
            found
        } else {
            csub
        };
        VarBoard::unrank(self.ctx.rows, self.ctx.cols, p0 * nc + c0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// step0 small sizes: placement 2-path agreement + state-level cross-check.
    #[test]
    fn counts_two_paths_small() {
        for &(r, c) in &[(3usize, 2usize), (3, 3), (4, 3)] {
            let k = counts(r, c);
            assert_eq!(k.r_full_a, k.r_full_b, "{r}x{c}: r_full A!=B");
            assert_eq!(k.m, k.nc * k.nplace, "{r}x{c}: M");
            // independent full-state enumeration
            let (reps, selfs) = counts_by_states(r, c);
            assert_eq!(reps, k.r_full_b, "{r}x{c}: state reps != (M+S)/2");
            assert_eq!(selfs, k.s_full, "{r}x{c}: state self-sym != S_full");
        }
    }

    /// Full bijection: over all reps, mirror_rank fills [0,R_full) once each,
    /// round-trips, and is mirror-invariant.
    fn check_bijection(rows: usize, cols: usize) {
        let mr = MirrorRanker::build(rows, cols);
        let k = counts(rows, cols);
        assert_eq!(mr.r_full(), k.r_full_b, "{rows}x{cols}: R_full mismatch");
        let m = k.m;
        let mut seen = vec![false; mr.r_full() as usize];
        let mut reps = 0u64;
        for r in 0..m {
            let b = VarBoard::unrank(rows, cols, r);
            let rm = b.mirror();
            // mirror invariance
            assert_eq!(mr.mirror_rank(&b), mr.mirror_rank(&rm), "{rows}x{cols} r={r}: mirror inv");
            let mrank = mr.mirror_rank(&b);
            assert!(mrank < mr.r_full(), "{rows}x{cols} r={r}: out of range");
            if r <= rm.rank() {
                // representative
                assert!(!seen[mrank as usize], "{rows}x{cols} r={r}: dup rank {mrank}");
                seen[mrank as usize] = true;
                reps += 1;
                // round trip: unrank gives back this rep
                let b2 = mr.mirror_unrank(mrank);
                assert_eq!(b2.rank(), r, "{rows}x{cols} r={r}: round-trip");
            }
        }
        assert_eq!(reps, mr.r_full(), "{rows}x{cols}: rep count");
        assert!(seen.iter().all(|&x| x), "{rows}x{cols}: gaps in rank space");
    }

    #[test]
    fn bijection_3x2_3x3_4x3() {
        check_bijection(3, 2);
        check_bijection(3, 3);
        check_bijection(4, 3);
    }

    /// 4×4 is larger (#[ignore]: run explicitly).
    #[test]
    #[ignore]
    fn bijection_4x4() {
        check_bijection(4, 4);
    }

    /// Lever 3 gate: `mirror_unrank_fast(idx) == mirror_unrank(idx)` for every
    /// idx (exhaustive ≤4×4). Comparing `.rank()` is a unique board fingerprint.
    fn check_unrank_fast(rows: usize, cols: usize) {
        let mr = MirrorRanker::build(rows, cols);
        for idx in 0..mr.r_full() {
            assert_eq!(
                mr.mirror_unrank_fast(idx).rank(),
                mr.mirror_unrank(idx).rank(),
                "{rows}x{cols} idx={idx}"
            );
        }
    }

    #[test]
    fn unrank_fast_matches_small() {
        check_unrank_fast(3, 2);
        check_unrank_fast(3, 3);
        check_unrank_fast(4, 3);
    }

    /// Lever 5 gate: `mirror_unrank_fast2 == mirror_unrank` (k_at coarse index).
    fn check_unrank_fast2(rows: usize, cols: usize) {
        let mr = MirrorRanker::build(rows, cols);
        for idx in 0..mr.r_full() {
            assert_eq!(
                mr.mirror_unrank_fast2(idx).rank(),
                mr.mirror_unrank(idx).rank(),
                "{rows}x{cols} idx={idx}"
            );
        }
    }

    #[test]
    fn unrank_fast2_matches_small() {
        check_unrank_fast2(3, 2);
        check_unrank_fast2(3, 3);
        check_unrank_fast2(4, 3);
    }

    #[test]
    #[ignore]
    fn unrank_fast2_matches_4x4() {
        check_unrank_fast2(4, 4);
    }

    #[test]
    #[ignore]
    fn unrank_fast2_matches_big() {
        for &(r, c) in &[(4usize, 5usize), (5, 5)] {
            let mr = MirrorRanker::build(r, c);
            let rf = mr.r_full();
            let step = (rf / 3_000_000).max(1);
            let mut idx = 0u64;
            while idx < rf {
                assert_eq!(mr.mirror_unrank_fast2(idx).rank(), mr.mirror_unrank(idx).rank(), "{r}x{c} idx={idx}");
                idx += step;
            }
            for &e in &[0u64, 1, rf - 1, rf - 2, rf / 2] {
                assert_eq!(mr.mirror_unrank_fast2(e).rank(), mr.mirror_unrank(e).rank(), "{r}x{c} edge {e}");
            }
            println!("unrank_fast2 {r}x{c}: OK (sampled + edges)");
        }
    }

    #[test]
    #[ignore]
    fn unrank_fast_matches_4x4() {
        check_unrank_fast(4, 4);
    }

    /// Sampled + edge-case check at 4×5 / 5×5 (the >u32 scale).
    #[test]
    #[ignore]
    fn unrank_fast_matches_big() {
        for &(r, c) in &[(4usize, 5usize), (5, 5)] {
            let mr = MirrorRanker::build(r, c);
            let rf = mr.r_full();
            let n = 3_000_000u64;
            let step = (rf / n).max(1);
            let mut idx = 0u64;
            while idx < rf {
                assert_eq!(
                    mr.mirror_unrank_fast(idx).rank(),
                    mr.mirror_unrank(idx).rank(),
                    "{r}x{c} idx={idx}"
                );
                idx += step;
            }
            // explicit edges
            for &e in &[0u64, 1, rf - 1, rf - 2, rf / 2] {
                assert_eq!(mr.mirror_unrank_fast(e).rank(), mr.mirror_unrank(e).rank(), "{r}x{c} edge {e}");
            }
            println!("unrank_fast {r}x{c}: OK (sampled {} + edges)", n.min(rf));
        }
    }

    /// Lever 4 gate: `mirror_rank_fast(b) == mirror_rank(b)` over EVERY board
    /// (every rank `r ∈ [0, M)` is a distinct board). Exhaustive ≤4×4.
    fn check_mirror_rank_fast(rows: usize, cols: usize) {
        let mr = MirrorRanker::build(rows, cols);
        let total = 2 * cols;
        let ncells = rows * cols;
        let comp = comp_table(ncells, total);
        let binom = binom_table(total);
        let m = binom[total][cols] * comp[ncells][total];
        for r in 0..m {
            let b = VarBoard::unrank(rows, cols, r);
            assert_eq!(mr.mirror_rank_fast(&b), mr.mirror_rank(&b), "{rows}x{cols} r={r}");
        }
    }

    #[test]
    fn mirror_rank_fast_matches_small() {
        check_mirror_rank_fast(3, 2);
        check_mirror_rank_fast(3, 3);
        check_mirror_rank_fast(4, 3);
    }

    /// ③a gate: `mirror_rank_fast3 == mirror_rank` over all boards (exhaustive).
    fn check_mirror_rank_fast3(rows: usize, cols: usize) {
        let mut mr = MirrorRanker::build(rows, cols);
        mr.build_place_mirror();
        let total = 2 * cols;
        let ncells = rows * cols;
        let comp = comp_table(ncells, total);
        let binom = binom_table(total);
        let m = binom[total][cols] * comp[ncells][total];
        for r in 0..m {
            let b = VarBoard::unrank(rows, cols, r);
            assert_eq!(mr.mirror_rank_fast3(&b), mr.mirror_rank(&b), "{rows}x{cols} r={r}");
        }
    }

    #[test]
    fn mirror_rank_fast3_matches_small() {
        check_mirror_rank_fast3(3, 2);
        check_mirror_rank_fast3(3, 3);
        check_mirror_rank_fast3(4, 3);
    }

    #[test]
    #[ignore]
    fn mirror_rank_fast3_matches_4x4() {
        check_mirror_rank_fast3(4, 4);
    }

    #[test]
    #[ignore]
    fn mirror_rank_fast_matches_4x4() {
        check_mirror_rank_fast(4, 4);
    }

    /// Sampled over the full rank space `[0, M)` at 4×5 / 5×5 (the >u32 scale).
    #[test]
    #[ignore]
    fn mirror_rank_fast_matches_big() {
        for &(r, c) in &[(4usize, 5usize), (5, 5)] {
            let mr = MirrorRanker::build(r, c);
            let total = 2 * c;
            let ncells = r * c;
            let comp = comp_table(ncells, total);
            let binom = binom_table(total);
            let m = binom[total][c] * comp[ncells][total];
            let step = (m / 3_000_000).max(1);
            let mut rr = 0u64;
            while rr < m {
                let b = VarBoard::unrank(r, c, rr);
                assert_eq!(mr.mirror_rank_fast(&b), mr.mirror_rank(&b), "{r}x{c} r={rr}");
                rr += step;
            }
            for &e in &[0u64, 1, m - 1, m / 2] {
                let b = VarBoard::unrank(r, c, e);
                assert_eq!(mr.mirror_rank_fast(&b), mr.mirror_rank(&b), "{r}x{c} edge {e}");
            }
            println!("mirror_rank_fast {r}x{c}: OK (sampled + edges)");
        }
    }

    /// Big-size step0 (#[ignore]: placement enumeration, run explicitly).
    /// Prints S_full / R_full for the record.
    #[test]
    #[ignore]
    fn counts_big() {
        for &(r, c) in &[(4usize, 4usize), (5, 4), (4, 5), (5, 5), (6, 5)] {
            let k = counts(r, c);
            assert_eq!(k.r_full_a, k.r_full_b, "{r}x{c}: r_full A!=B");
            println!(
                "{r}x{c}: NPLACE={} NC={} M={} NPLACE_self={} S_full={} R_full={}",
                k.nplace, k.nc, k.m, k.nplace_self, k.s_full, k.r_full_b
            );
        }
    }
}
