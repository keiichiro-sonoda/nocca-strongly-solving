//! In-RAM retrograde analysis over the size-parametric [`VarBoard`] — gate G0 of
//! the disk-retrograde validation plan, and the oracle the disk version is
//! checked against.
//!
//! Semantics ("infinite play = draw"): a node is **Loss** if it is a terminal
//! loss or *all* its children are Win; **Win** if *some* child is Loss; else
//! **Draw**. This is the least fixpoint of the Win/Loss attractor, computed by a
//! BFS outward from terminal-loss nodes. A remaining-children counter marks a
//! node Loss only when its *last* child becomes Win, so nodes sitting on a cycle
//! (whose children never all resolve to Win) are never decided and fall into the
//! Draw complement — correct under cycles (NOCCA never captures, so cycles are
//! the norm). Distances follow the BFS order: Win = 1 + (min Loss-child dist),
//! Loss = 1 + (max Win-child dist).

use crate::solver::Value;
use crate::varboard::VarBoard;
use std::collections::{HashMap, VecDeque};

const UNKNOWN: u8 = 0;
const WIN: u8 = 1;
const LOSS: u8 = 2;

#[derive(Clone, Copy, Debug)]
pub struct RetroResult {
    /// Value of the initial position.
    pub value: Value,
    /// Distance to mate in plies for the initial position (0 if Draw).
    pub dist: u32,
    pub n_total: usize,
    pub n_win: usize,
    pub n_loss: usize,
    pub n_draw: usize,
}

/// Solve the reduced version on a `rows × cols` board (pieces per player =
/// `cols`) entirely in RAM. Panics via the caller's allocator if the reachable
/// set does not fit; intended for the small/medium reduced versions.
pub fn solve_ram(rows: usize, cols: usize, diag: bool) -> RetroResult {
    // -- Phase 1: enumerate reachable canonical nodes (BFS from the initial). ---
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
            continue; // terminal win (try-type): no successors
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

    // -- Phase 2: children + terminal seed values. ----------------------------
    // Two terminal kinds: try-win ⇒ Win (side to move already wins this turn),
    // no-legal-move ⇒ Loss. Both at distance 0.
    let mut children: Vec<Vec<u32>> = vec![Vec::new(); n];
    let mut seed: Vec<u8> = vec![UNKNOWN; n];
    for i in 0..n {
        let b = VarBoard::from_key(rows, cols, nodes[i]);
        if b.try_win() {
            seed[i] = WIN;
            continue;
        }
        b.moves(&mut mv, diag);
        if mv.is_empty() {
            seed[i] = LOSS; // no legal move = loss for the side to move
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
    let n_real = n;

    // -- Phase 3: reverse adjacency (predecessors). ---------------------------
    let mut pred_count = vec![0u32; n];
    for ch in &children {
        for &c in ch {
            pred_count[c as usize] += 1;
        }
    }
    let mut preds: Vec<Vec<u32>> = pred_count
        .iter()
        .map(|&c| Vec::with_capacity(c as usize))
        .collect();
    for (i, ch) in children.iter().enumerate() {
        for &c in ch {
            preds[c as usize].push(i as u32);
        }
    }

    // -- Phase 4: retrograde BFS (least fixpoint). ----------------------------
    let mut value = vec![UNKNOWN; n];
    let mut dist = vec![0u32; n];
    let mut win_count = vec![0u32; n];
    let out_deg: Vec<u32> = children.iter().map(|c| c.len() as u32).collect();
    let mut queue: VecDeque<u32> = VecDeque::new();

    for i in 0..n {
        if seed[i] != UNKNOWN {
            value[i] = seed[i];
            dist[i] = 0;
            queue.push_back(i as u32);
        }
    }

    while let Some(ni) = queue.pop_front() {
        let n_idx = ni as usize;
        let d = dist[n_idx];
        match value[n_idx] {
            LOSS => {
                // A Loss child makes every predecessor a Win (some child is Loss).
                for &p in &preds[n_idx] {
                    let p = p as usize;
                    if value[p] == UNKNOWN {
                        value[p] = WIN;
                        dist[p] = d + 1; // first (min-dist) Loss child wins fastest
                        queue.push_back(p as u32);
                    }
                }
            }
            WIN => {
                // A Win child advances each predecessor's counter; the last one
                // (all children Win) makes it a Loss.
                for &p in &preds[n_idx] {
                    let p = p as usize;
                    if value[p] == UNKNOWN {
                        win_count[p] += 1;
                        if win_count[p] == out_deg[p] {
                            value[p] = LOSS;
                            dist[p] = d + 1; // last (max-dist) Win child resists longest
                            queue.push_back(p as u32);
                        }
                    }
                }
            }
            _ => unreachable!("queued node must be decided"),
        }
    }

    // Count over real positions only (exclude the virtual WON leaf at won_idx).
    let n_win = value[..n_real].iter().filter(|&&v| v == WIN).count();
    let n_loss = value[..n_real].iter().filter(|&&v| v == LOSS).count();
    let n_draw = n_real - n_win - n_loss;

    let root = 0usize; // initial position is node 0
    let (value, root_dist) = match value[root] {
        WIN => (Value::Win, dist[root]),
        LOSS => (Value::Loss, dist[root]),
        _ => (Value::Draw, 0),
    };

    RetroResult {
        value,
        dist: root_dist,
        n_total: n_real,
        n_win,
        n_loss,
        n_draw,
    }
}
