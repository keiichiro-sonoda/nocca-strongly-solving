//! Bijective ranking of NOCCA positions to a dense integer index, for the
//! compressed disk retrograde (16-byte key → ~5-byte rank). A position factors
//! into a **placement** (per-cell stack heights, color-agnostic, summing to the
//! fixed piece count `2·cols`) and a **coloring** (which of the `2·cols` piece
//! slots, read bottom-to-top across cells, are Black — `C(2·cols, cols)` ways):
//!
//!   rank = rank_placement(heights) · C(2·cols, cols) + rank_coloring(blacks)
//!
//! Both factors use a standard combinatorial number system, so the whole map is
//! a bijection onto `[0, N_placements · C(2·cols, cols))` — injective by
//! construction (no two distinct boards share a rank). `VarBoard::rank` /
//! `VarBoard::unrank` wrap this; ranking is always applied to the canonical
//! (mirror-min) representative, so mirror images collapse to one rank correctly.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

thread_local! {
    static CACHE: RefCell<HashMap<(usize, usize), (Rc<Vec<Vec<u64>>>, Rc<Vec<Vec<u64>>>)>> =
        RefCell::new(HashMap::new());
}

/// Cached `(comp_table(ncells,total), binom_table(total))` — built once per
/// `(ncells,total)` per thread. rank/unrank are called billions of times, so
/// rebuilding these tables per call (they depend only on board dimensions) was
/// the dominant cost; this memoizes them.
pub fn tables(ncells: usize, total: usize) -> (Rc<Vec<Vec<u64>>>, Rc<Vec<Vec<u64>>>) {
    CACHE.with(|c| {
        c.borrow_mut()
            .entry((ncells, total))
            .or_insert_with(|| {
                (
                    Rc::new(comp_table(ncells, total)),
                    Rc::new(binom_table(total)),
                )
            })
            .clone()
    })
}

/// Binomial coefficients `C(n, k)` for `n <= nmax`.
pub fn binom_table(nmax: usize) -> Vec<Vec<u64>> {
    let mut c = vec![vec![0u64; nmax + 1]; nmax + 1];
    for n in 0..=nmax {
        c[n][0] = 1;
        for k in 1..=n {
            c[n][k] = c[n - 1][k - 1] + c[n - 1][k];
        }
    }
    c
}

/// `comp[n][s]` = number of length-`n` vectors with entries in `0..=3` summing
/// to `s` (each entry a stack height ≤ 3). `n <= ncells`, `s <= total`.
pub fn comp_table(ncells: usize, total: usize) -> Vec<Vec<u64>> {
    let mut t = vec![vec![0u64; total + 1]; ncells + 1];
    t[0][0] = 1;
    for n in 1..=ncells {
        for s in 0..=total {
            let mut v = 0u64;
            for d in 0..=3usize.min(s) {
                v += t[n - 1][s - d];
            }
            t[n][s] = v;
        }
    }
    t
}

/// Lex rank of a height vector (each `0..=3`, summing to `total`) in `[0, comp[ncells][total])`.
pub fn rank_placement(heights: &[u8], total: usize, comp: &[Vec<u64>]) -> u64 {
    let ncells = heights.len();
    let mut rem = total;
    let mut r = 0u64;
    for i in 0..ncells {
        let n_rest = ncells - 1 - i;
        let h = heights[i] as usize;
        for d in 0..h {
            if d <= rem {
                r += comp[n_rest][rem - d];
            }
        }
        rem -= h;
    }
    r
}

/// Inverse of [`rank_placement`].
pub fn unrank_placement(r: u64, ncells: usize, total: usize, comp: &[Vec<u64>]) -> Vec<u8> {
    let mut heights = vec![0u8; ncells];
    unrank_placement_into(r, ncells, total, comp, &mut heights);
    heights
}

/// Allocation-free inverse of [`rank_placement`], writing into `out[..ncells]`.
pub fn unrank_placement_into(
    mut r: u64,
    ncells: usize,
    total: usize,
    comp: &[Vec<u64>],
    out: &mut [u8],
) {
    debug_assert!(out.len() >= ncells);
    out[..ncells].fill(0);
    let mut rem = total;
    for i in 0..ncells {
        let n_rest = ncells - 1 - i;
        for d in 0..=3usize {
            if d > rem {
                break;
            }
            let cnt = comp[n_rest][rem - d];
            if r < cnt {
                out[i] = d as u8;
                rem -= d;
                break;
            }
            r -= cnt;
        }
    }
}

/// Combinatorial-number-system rank of the `k`-subset `set` (sorted ascending)
/// of `[0, n)`, in `[0, C(n, k))`: `Σ_i C(set[i], i+1)`.
pub fn rank_comb(set: &[usize], binom: &[Vec<u64>]) -> u64 {
    let mut r = 0u64;
    for (i, &c) in set.iter().enumerate() {
        r += binom[c][i + 1];
    }
    r
}

/// Inverse of [`rank_comb`]: the `k`-subset of `[0, n)` with the given rank.
pub fn unrank_comb(r: u64, n: usize, k: usize, binom: &[Vec<u64>]) -> Vec<usize> {
    let mut out = vec![0usize; k];
    unrank_comb_into(r, n, k, binom, &mut out);
    out
}

/// Allocation-free inverse of [`rank_comb`], writing into `out[..k]`.
pub fn unrank_comb_into(mut r: u64, n: usize, k: usize, binom: &[Vec<u64>], out: &mut [usize]) {
    debug_assert!(out.len() >= k);
    // Greedy from the largest element down.
    let mut hi = n; // exclusive upper bound for the next (largest) element search
    for i in (0..k).rev() {
        // largest c < hi with C(c, i+1) <= r
        let mut c = i; // minimum possible value for position i
        while c + 1 < hi && binom[c + 1][i + 1] <= r {
            c += 1;
        }
        out[i] = c;
        r -= binom[c][i + 1];
        hi = c;
    }
}
