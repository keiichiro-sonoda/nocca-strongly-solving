//! Forward **weak-solving** engine: 3-valued negamax (αβ) with a transposition table.
//!
//! NOTE: this is the project's *earlier* approach — proving the value of the initial
//! position by forward search. It diverged on NOCCA's draw-prone positions, which
//! motivated the **retrograde strong solve** that produced the full 6×5 result (see
//! the README and [`crate::inmem_retro`]). It is kept for the methodological contrast
//! and because it defines the shared [`Value`] type used by the retrogrades.
//!
//! Semantics ("infinite play = draw", see `docs/weak-solving-design.md`):
//! a position's value from the side-to-move's view is `Win` if the mover can
//! force reaching the goal in finitely many moves, `Loss` if the opponent can
//! force the mover's loss, `Draw` otherwise (perpetual avoidance). These values
//! are path-independent (a reachability game), which is what makes transposition
//! caching sound.
//!
//! **Soundness invariant (critical):** the [`Tt`] stores only proven `Win` /
//! `Loss` (keyed by [`Position::canonical_key`]); `Draw` is never cached.
//! Repetition-derived draws are history-dependent (the GHI problem), whereas a
//! proven `Win`/`Loss` never depends on the path or the depth bound — see the
//! design doc §2.3.

use std::collections::HashSet;

use crate::moves::MoveList;
use crate::position::Position;

/// Game value from the side-to-move's perspective. Ordered `Loss < Draw < Win`.
///
/// In depth-limited search, `Win` and `Loss` are *proofs*; `Draw` means either a
/// genuine repetition draw or "not proven within the depth bound".
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Value {
    Loss,
    Draw,
    Win,
}

impl Value {
    /// Value as seen by the parent (other player) one ply up.
    #[inline]
    pub fn negate(self) -> Value {
        match self {
            Value::Win => Value::Loss,
            Value::Loss => Value::Win,
            Value::Draw => Value::Draw,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Value::Win => "WIN",
            Value::Loss => "LOSS",
            Value::Draw => "draw",
        }
    }
}

/// A path-independent proven result, the only thing cached in the [`Tt`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Proven {
    Win,
    Loss,
}

impl From<Proven> for Value {
    #[inline]
    fn from(p: Proven) -> Value {
        match p {
            Proven::Win => Value::Win,
            Proven::Loss => Value::Loss,
        }
    }
}

#[derive(Clone, Copy)]
struct Slot {
    key: u128,
    work: u32,
    proven: Proven,
    used: bool,
}

/// Direct-mapped transposition table of proven `Win`/`Loss` results.
///
/// Because cached entries are facts, eviction is always sound (a re-search
/// re-derives the same value); the replacement policy only affects speed.
pub struct Tt {
    slots: Box<[Slot]>,
    filled: usize,
    probes: u64,
    hits: u64,
}

impl Tt {
    /// Allocate a table sized to fit within `bytes` (rounded down to a power of
    /// two number of slots).
    pub fn with_capacity_bytes(bytes: usize) -> Self {
        let per = std::mem::size_of::<Slot>().max(1);
        let raw = (bytes / per).max(1);
        let mut n = raw.next_power_of_two();
        if n > raw {
            n >>= 1;
        }
        n = n.max(1);
        let empty = Slot {
            key: 0,
            work: 0,
            proven: Proven::Win,
            used: false,
        };
        Tt {
            slots: vec![empty; n].into_boxed_slice(),
            filled: 0,
            probes: 0,
            hits: 0,
        }
    }

    #[inline]
    fn slot_index(&self, key: u128) -> usize {
        // Mix the full 128-bit key into 64 bits so every input bit matters: a
        // plain `lo ^ hi` fold collapsed distinct keys (half shared a fold).
        let lo = key as u64;
        let hi = (key >> 64) as u64;
        let mut h = lo ^ hi.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        h ^= h >> 32;
        h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
        h ^= h >> 33;
        // Fibonacci hashing: take the HIGH bits of the product, which mix in all
        // input bits. Masking the LOW bits instead depends only on the low bits
        // of `h` and was degenerate, collapsing many keys onto a few slots.
        let bits = self.slots.len().trailing_zeros(); // slot count is 2^bits
        if bits == 0 {
            return 0;
        }
        (h.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> (64 - bits)) as usize
    }

    #[inline]
    pub fn probe(&mut self, key: u128) -> Option<Proven> {
        self.probes += 1;
        let s = self.slots[self.slot_index(key)];
        if s.used && s.key == key {
            self.hits += 1;
            Some(s.proven)
        } else {
            None
        }
    }

    /// Store a proven result, preferring to keep higher-`work` entries on
    /// collision (work = size of the subtree that proved it).
    #[inline]
    pub fn store(&mut self, key: u128, proven: Proven, work: u32) {
        let i = self.slot_index(key);
        let s = &mut self.slots[i];
        if !s.used {
            self.filled += 1;
        } else if s.key != key && work < s.work {
            return; // keep the more valuable entry
        }
        s.key = key;
        s.work = work;
        s.proven = proven;
        s.used = true;
    }

    /// Number of occupied slots.
    #[inline]
    pub fn len(&self) -> usize {
        self.filled
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.filled == 0
    }

    #[inline]
    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    /// Size in bytes of one table slot (for reporting how `--tt-bytes` maps to
    /// slot count).
    #[inline]
    pub fn slot_bytes() -> usize {
        std::mem::size_of::<Slot>()
    }

    pub fn hit_rate(&self) -> f64 {
        if self.probes == 0 {
            0.0
        } else {
            self.hits as f64 / self.probes as f64
        }
    }
}

/// Depth-limited 3-valued negamax searcher sharing a [`Tt`].
pub struct Solver<'a> {
    tt: &'a mut Tt,
    path: HashSet<u128>,
    max_ply: u32,
    nodes: u64,
}

impl<'a> Solver<'a> {
    pub fn new(tt: &'a mut Tt, max_ply: u32) -> Self {
        Solver {
            tt,
            path: HashSet::new(),
            max_ply,
            nodes: 0,
        }
    }

    #[inline]
    pub fn nodes(&self) -> u64 {
        self.nodes
    }

    /// Value of `pos` from the side-to-move's view, within the depth bound.
    pub fn search(&mut self, pos: &Position) -> Value {
        self.negamax(pos, 0, Value::Loss, Value::Win)
    }

    fn negamax(&mut self, pos: &Position, ply: u32, mut alpha: Value, beta: Value) -> Value {
        self.nodes += 1;

        // 1. Terminal (path-independent).
        if pos.opponent_reached_goal() {
            return Value::Loss; // opponent already won last move
        }
        let mut ml = MoveList::new();
        pos.generate_legal_moves(&mut ml);
        if ml.is_empty() {
            return Value::Loss; // rule (B): no move = loss
        }

        let key = pos.canonical_key();

        // 2. A cached proof (Win/Loss) is path-independent: take it first.
        if let Some(p) = self.tt.probe(key) {
            return p.into();
        }
        // 3. Repetition on the current path = Draw (2-fold).
        if self.path.contains(&key) {
            return Value::Draw;
        }
        // 4. Depth bound: unproven, treat as Draw (NOT a draw proof; see design §7).
        if ply >= self.max_ply {
            return Value::Draw;
        }

        // 5. Expand.
        self.path.insert(key);
        let work_before = self.nodes;
        let mut best = Value::Loss;
        for i in 0..ml.len() {
            let child = pos.apply_move(ml.get(i));
            let v = self.negamax(&child, ply + 1, beta.negate(), alpha.negate()).negate();
            if v > best {
                best = v;
            }
            if best > alpha {
                alpha = best;
            }
            if best == Value::Win || alpha >= beta {
                break; // Win is maximal; αβ cutoff
            }
        }
        self.path.remove(&key);

        // 6. Cache only proven Win/Loss (never Draw) — the GHI-safe invariant.
        match best {
            Value::Win => {
                let work = (self.nodes - work_before).min(u32::MAX as u64) as u32;
                self.tt.store(key, Proven::Win, work);
            }
            Value::Loss => {
                let work = (self.nodes - work_before).min(u32::MAX as u64) as u32;
                self.tt.store(key, Proven::Loss, work);
            }
            Value::Draw => {}
        }
        best
    }
}

/// One iterative-deepening iteration's progress, for live reporting.
#[derive(Clone, Copy, Debug)]
pub struct Progress {
    pub max_ply: u32,
    pub root_value: Value,
    pub nodes: u64,
    pub tt_entries: usize,
    pub elapsed_secs: f64,
}

/// Solve `root` by ply-limited iterative deepening, calling `on_progress` after
/// each depth. Returns as soon as the root is proven `Win`/`Loss`; if still
/// undecided at `max_ply_limit`, returns `Draw` (unproven within the bound — a
/// real draw proof needs the fixpoint argument in the design doc §7).
pub fn solve_root<F: FnMut(&Progress)>(
    root: &Position,
    tt: &mut Tt,
    max_ply_limit: u32,
    mut on_progress: F,
) -> Value {
    let start = std::time::Instant::now();
    let mut last = Value::Draw;
    for max_ply in 1..=max_ply_limit {
        let (value, nodes) = {
            let mut solver = Solver::new(tt, max_ply);
            let v = solver.search(root);
            (v, solver.nodes())
        };
        let prog = Progress {
            max_ply,
            root_value: value,
            nodes,
            tt_entries: tt.len(),
            elapsed_secs: start.elapsed().as_secs_f64(),
        };
        on_progress(&prog);
        last = value;
        if value == Value::Win || value == Value::Loss {
            return value;
        }
    }
    last
}

// ===================================================================== df-pn(r)

const INF: u32 = u32::MAX;

/// Which binary question a df-pn search proves (design doc §11.1).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Goal {
    /// Prove the root's side-to-move can force a Win.
    ProveWin,
    /// Prove the opponent can force the root's side-to-move to Lose.
    ProveLoss,
}

/// Proof / disproof numbers. `pn == 0` ⇒ proven; `dn == 0` ⇒ disproven.
#[derive(Clone, Copy)]
struct Pd {
    pn: u32,
    dn: u32,
}

impl Pd {
    const PROOF: Pd = Pd { pn: 0, dn: INF };
    const DISPROOF: Pd = Pd { pn: INF, dn: 0 };
    const UNKNOWN: Pd = Pd { pn: 1, dn: 1 };
}

#[inline]
fn sat(x: u64) -> u32 {
    if x >= INF as u64 {
        INF
    } else {
        x as u32
    }
}

fn fmt_inf(x: u32) -> String {
    if x == INF {
        "INF".to_string()
    } else {
        x.to_string()
    }
}

/// Depth-first proof-number search with repetition handling — df-pn(r).
///
/// Cyclic safety (design §11.2): a position already on the current DFS path is
/// an immediate **disproof** leaf, so back-edges close and the search
/// terminates. GHI safety: only proofs (`pn == 0`, which can never contain a
/// repetition leaf) are promoted to the shared facts [`Tt`]; repetition-tainted
/// disproofs are never globally cached.
pub struct Dfpn<'a> {
    facts: &'a mut Tt,
    work: std::collections::HashMap<u128, Pd>,
    path: HashSet<u128>,
    goal: Goal,
    nodes: u64,
    promotions: u64,
    start: std::time::Instant,
    root_key: u128,
    /// Live (pn, dn) of the root node, updated in place each time the root's
    /// expansion loop recomputes them. Observational only (the search never
    /// reads it): the root's `mid` call does not return until the search ends,
    /// so its value can't be read back from `work` mid-run — this exposes it.
    root_live: Pd,
    progress_every: u64,
    /// Per-search node budget (0 = unlimited). On exhaustion the search aborts
    /// and reports "not proven" — guaranteeing termination even when a cyclic
    /// disproof (e.g. a deep perpetual defense) would otherwise not converge.
    node_limit: u64,
    aborted: bool,
}

impl<'a> Dfpn<'a> {
    pub fn new(facts: &'a mut Tt, goal: Goal) -> Self {
        Dfpn {
            facts,
            work: std::collections::HashMap::new(),
            path: HashSet::new(),
            goal,
            nodes: 0,
            promotions: 0,
            start: std::time::Instant::now(),
            root_key: 0,
            root_live: Pd::UNKNOWN,
            progress_every: 0,
            node_limit: 0,
            aborted: false,
        }
    }

    /// Emit a progress line to stderr every `every` nodes (0 = disabled).
    pub fn with_progress(mut self, every: u64) -> Self {
        self.progress_every = every;
        self
    }

    /// Stop after `limit` node expansions (0 = unlimited).
    pub fn with_node_limit(mut self, limit: u64) -> Self {
        self.node_limit = limit;
        self
    }

    pub fn aborted(&self) -> bool {
        self.aborted
    }

    pub fn nodes(&self) -> u64 {
        self.nodes
    }

    /// Number of times a proven node was promoted to the facts table. Includes
    /// re-promotions of already-known facts, so `promotions / facts.len()` is a
    /// re-derivation factor, not a count of distinct proofs.
    pub fn promotions(&self) -> u64 {
        self.promotions
    }

    /// A node is an OR node (prover to move) iff the prover is the side to move.
    /// For `ProveWin` the prover moves at even plies; for `ProveLoss`, odd plies.
    #[inline]
    fn is_or(&self, ply: u32) -> bool {
        (ply % 2 == 0) == matches!(self.goal, Goal::ProveWin)
    }

    /// Run the search; returns `true` if the root question is proven.
    pub fn prove(&mut self, root: &Position) -> bool {
        self.root_key = root.canonical_key();
        self.start = std::time::Instant::now();
        self.aborted = false;
        self.root_live = Pd::UNKNOWN;
        let r = self.mid(root, 0, INF, INF);
        // On a clean finish `r` is the true result; on abort `r` is UNKNOWN, so
        // keep the last live (pn, dn) the root loop computed instead of clobbering
        // it back to 1/1 — that's the value worth seeing when interrupted.
        if !self.aborted {
            self.root_live = r;
        }
        if self.progress_every > 0 {
            self.print_progress(if self.aborted { "aborted" } else { "final" });
        }
        !self.aborted && r.pn == 0
    }

    fn print_progress(&self, tag: &str) {
        let root = self.root_live;
        eprintln!(
            "  [{:?} {}] root pn={} dn={}  nodes={}  facts={}  promo={}  {:.1}s",
            self.goal,
            tag,
            fmt_inf(root.pn),
            fmt_inf(root.dn),
            self.nodes,
            self.facts.len(),
            self.promotions,
            self.start.elapsed().as_secs_f64()
        );
    }

    /// Leaf value if `pos` is a game-terminal (side to move has no move or the
    /// opponent reached its goal), else `None`. A terminal proof is *fresh* —
    /// not yet in the facts table — so the caller promotes it exactly once.
    fn terminal_value(&self, pos: &Position, ply: u32) -> Option<Pd> {
        let lost = pos.opponent_reached_goal() || {
            let mut ml = MoveList::new();
            pos.generate_legal_moves(&mut ml);
            ml.is_empty()
        };
        if lost {
            // Side to move has lost. At an OR node the prover lost (disproof);
            // at an AND node the prover's opponent lost (proof).
            return Some(if self.is_or(ply) { Pd::DISPROOF } else { Pd::PROOF });
        }
        None
    }

    /// Leaf value if `pos` is already a proven fact, else `None`. A fact hit is
    /// *not* re-promoted (it is already in the table) — that double-count is what
    /// made `promotions` blow up relative to `facts.len()`.
    fn fact_value(&mut self, pos: &Position, ply: u32) -> Option<Pd> {
        let p = self.facts.probe(pos.canonical_key())?;
        // V = value for the side to move. OR node ⇒ Win is a proof; AND node ⇒
        // Win is a disproof (the prover does not win there). Loss flips.
        let win = matches!(p, Proven::Win);
        Some(if win == self.is_or(ply) {
            Pd::PROOF
        } else {
            Pd::DISPROOF
        })
    }

    /// Current (pn, dn) estimate for a child, honoring terminals/facts, path
    /// repetition (= disproof), the work table, then an unknown default.
    fn child_value(&mut self, child: &Position, cply: u32) -> Pd {
        if let Some(pd) = self.terminal_value(child, cply) {
            return pd;
        }
        if let Some(pd) = self.fact_value(child, cply) {
            return pd;
        }
        let ck = child.canonical_key();
        if self.path.contains(&ck) {
            return Pd::DISPROOF; // repetition
        }
        self.work.get(&ck).copied().unwrap_or(Pd::UNKNOWN)
    }

    /// Promote a proven node to the facts table. OR node ⇒ side to move Wins;
    /// AND node ⇒ side to move Loses. Proofs are always repetition-free.
    fn promote(&mut self, key: u128, ply: u32) {
        let fact = if self.is_or(ply) {
            Proven::Win
        } else {
            Proven::Loss
        };
        self.facts.store(key, fact, 1);
        self.promotions += 1;
    }

    fn mid(&mut self, pos: &Position, ply: u32, th_pn: u32, th_dn: u32) -> Pd {
        self.nodes += 1;
        if self.node_limit != 0 && self.nodes >= self.node_limit {
            self.aborted = true;
        }
        if self.progress_every > 0 && self.nodes % self.progress_every == 0 {
            self.print_progress("...");
        }
        let key = pos.canonical_key();

        // Facts first: once a node (terminal or proven internal) is in the table,
        // later visits return here and never re-promote — keeping `promotions`
        // close to the count of distinct proofs.
        if let Some(pd) = self.fact_value(pos, ply) {
            return pd;
        }
        if let Some(pd) = self.terminal_value(pos, ply) {
            if pd.pn == 0 {
                self.promote(key, ply); // fresh terminal proof (first visit only)
            }
            return pd;
        }
        if self.path.contains(&key) {
            return Pd::DISPROOF; // repetition (not cached)
        }
        if self.aborted {
            return Pd::UNKNOWN; // budget exhausted: unwind without proving
        }

        let is_or = self.is_or(ply);
        let mut ml = MoveList::new();
        pos.generate_legal_moves(&mut ml);
        let children: Vec<Position> = (0..ml.len()).map(|i| pos.apply_move(ml.get(i))).collect();

        self.path.insert(key);
        let result = loop {
            if self.aborted {
                break Pd::UNKNOWN;
            }
            let mut min_pn = INF;
            let mut min_dn = INF;
            let mut sum_pn = 0u64;
            let mut sum_dn = 0u64;
            let mut best_idx = 0usize;
            let mut best_metric = INF;
            let mut second_metric = INF;
            for (i, c) in children.iter().enumerate() {
                let pd = self.child_value(c, ply + 1);
                sum_pn += pd.pn as u64;
                sum_dn += pd.dn as u64;
                min_pn = min_pn.min(pd.pn);
                min_dn = min_dn.min(pd.dn);
                let metric = if is_or { pd.pn } else { pd.dn };
                if metric < best_metric {
                    second_metric = best_metric;
                    best_metric = metric;
                    best_idx = i;
                } else if metric < second_metric {
                    second_metric = metric;
                }
            }
            let node_pn = if is_or { min_pn } else { sat(sum_pn) };
            let node_dn = if is_or { sat(sum_dn) } else { min_dn };
            if ply == 0 {
                // Expose the root's live estimate for progress reporting.
                self.root_live = Pd {
                    pn: node_pn,
                    dn: node_dn,
                };
            }

            if node_pn >= th_pn || node_dn >= th_dn {
                break Pd {
                    pn: node_pn,
                    dn: node_dn,
                };
            }

            let best_child = children[best_idx];
            let best_pd = self.child_value(&best_child, ply + 1);
            let (cth_pn, cth_dn) = if is_or {
                (
                    th_pn.min(second_metric.saturating_add(1)),
                    th_dn.saturating_sub(sat(sum_dn).saturating_sub(best_pd.dn)),
                )
            } else {
                (
                    th_pn.saturating_sub(sat(sum_pn).saturating_sub(best_pd.pn)),
                    th_dn.min(second_metric.saturating_add(1)),
                )
            };
            self.mid(&best_child, ply + 1, cth_pn, cth_dn);
        };
        self.path.remove(&key);
        if !self.aborted {
            self.work.insert(key, result);
            if result.pn == 0 {
                self.promote(key, ply);
            }
        }
        result
    }
}

/// Full 3-valued value of `root` via two df-pn(r) searches (design §11.1):
/// prove Win, else prove Loss, else Draw. `facts` is shared across both searches
/// (and reusable across calls — it holds only sound, path-independent proofs).
///
/// `node_limit` (0 = unlimited) bounds each search; an exhausted search counts
/// as "not proven". So `Win`/`Loss` are always sound proofs, while `Draw` means
/// "neither side proven to force a result within the budget" — for a deep
/// perpetual-defense draw this is budget-inconclusive rather than a draw proof.
pub fn solve_dfpn(root: &Position, facts: &mut Tt, node_limit: u64) -> Value {
    if Dfpn::new(facts, Goal::ProveWin)
        .with_node_limit(node_limit)
        .prove(root)
    {
        return Value::Win;
    }
    if Dfpn::new(facts, Goal::ProveLoss)
        .with_node_limit(node_limit)
        .prove(root)
    {
        return Value::Loss;
    }
    Value::Draw
}

/// Like [`solve_dfpn`] but streams per-search progress to stderr every
/// `progress_every` nodes.
pub fn solve_dfpn_verbose(
    root: &Position,
    facts: &mut Tt,
    node_limit: u64,
    progress_every: u64,
) -> Value {
    if Dfpn::new(facts, Goal::ProveWin)
        .with_node_limit(node_limit)
        .with_progress(progress_every)
        .prove(root)
    {
        return Value::Win;
    }
    if Dfpn::new(facts, Goal::ProveLoss)
        .with_node_limit(node_limit)
        .with_progress(progress_every)
        .prove(root)
    {
        return Value::Loss;
    }
    Value::Draw
}
