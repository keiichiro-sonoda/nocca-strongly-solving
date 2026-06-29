# Comparison with paper B (Yamamoto–Hoki, 「NOCCA×NOCCAの強解決」 / "Strongly Solving NOCCA×NOCCA", GPW 2022)

Numerical comparison only (paper B's reported results are cited for validation; this
is not a reproduction of the paper's text or figures).

The **strong solution** is the value + DTM of *every* mirror representative
(`R_full = 73,995,673,500`), computed by retrograde analysis. The forward reachable BFS
and the projection below exist **only to compare on paper B's footing** — paper B reports
its Table 8 / A.1 over the *reachable* set, so we project the full solution onto it. The
projection does **not** restrict the solution (which already covers all positions).

## Normalization (mirror un-fold)

This work solves over **mirror-canonical representatives** (rank-min canonical,
side-to-move fixed): `R_full = 73,995,673,500`. Paper B reports **pseudo-reachable**
counts (before mirror folding): a mirror-asymmetric rep ↔ 2 pseudo positions, a
self-symmetric rep ↔ 1. So, per value/per ply (Burnside):

    pseudo = 2 · (mirror count) − (self-symmetric count)

Cross-checks: self-symmetric reps over R_full = `S_full = 3,623,508` (matches
`mirror_rank::counts().s_full`); over the reachable set `S_reach = 3,608,880`
(= 2·73,986,754,080 − 147,969,899,280, paper B's own figures).

## A — reachable representative count

- Paper B "擬到達可能" mirror-folded = **73,986,754,080**.
- This work's **exact** forward-reachable set (expanding try-win positions, since a
  player may decline an available try and move on) has **73,991,213,760** reps,
  *including* all 4,459,725 no-move (stalemate) terminal positions.
- For the like-for-like non-terminal comparison, exclude those no-move terminals:
  **73,986,754,035**. Paper B's folded pseudo-reachable count is **45** larger.
- The paper's **30** no-legal-move terminal configurations are not part of that
  non-terminal residual; they are checked separately in (D) and match exactly.
- A clean identity holds exactly: `R_full − 73,986,754,080 = 8,919,420 = 2·no_move − 30`,
  where `no_move = 4,459,725` and the **30** is the paper B no-legal-move terminal count.
  This identity is a consistency check for the terminal-counting convention, separate
  from the 45 pseudo-reachable-but-not-truly-reachable reps above.

## B — Table 8 (reachable W/L/D, pseudo-reachable)

| | this work | paper B |
|---|---|---|
| win | 106,144,078,857 | 106,144,078,911 |
| lose (no-move/DTM0 excluded) | 41,129,930,503 | 41,129,930,509 |
| draw | 695,889,830 | 695,889,860 |

Residuals −54 / −6 / −30 (pseudo) = 90 pseudo = **45 mirror** = the same 45 of (A).
(no-move DTM-0 stalemate losses, 8,912,136 pseudo, are excluded as paper B does.)

## C — Table A.1 (per-ply distribution, all 69 plies)

**67 of 69 rows match exactly** (`table_a1_reachable_vs_paperB.csv`). The two that
differ are ply 1 (−54) and ply 4 (−6) — the same 45-rep residual, un-folded.

Special note on ply 48: paper B's PDF prints "879 28" (the last digit is dropped in
the published typesetting). This work computes **879,284**, which is independently
forced by paper B's own consistency (Table A.1 lose rows must sum to Table 8 lose =
41,129,930,509). So this work *recovers the dropped digit*, and ply 48 matches.

## D — pinpoint checks

- deepest: **ply-69 wins = 30** — EXACT (full-space r69 = 15 mirror, all reachable,
  none self-symmetric → 2·15 = 30 pseudo).
- terminal (no-move) configs = **30** — EXACT (the "30" in the identity of (A)).
- average legal moves over non-terminal reachable nodes = **23.391** ≈ paper B 23.4.

## What the 45-rep residual is

Paper B defines "擬到達可能" (pseudo-reachable) as a configuration satisfying certain
combinatorial conditions — by construction a **superset** of the truly-reachable set
(their §方法: "ゲーム開始点から到達可能で…配置は **すべて** 擬到達可能"). This work's
forward BFS computes the **exact** truly-reachable set (validated bit-for-bit against an
independent `HashSet<canonical_key>` BFS at 3×3 / 4×3 / 4×4). The 45 reps are
pseudo-reachable-but-not-truly-reachable; this work correctly excludes them, so the
distributions agree to within those 45 reps (6e-7 of R_full).
