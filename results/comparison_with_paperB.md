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
`mirror_rank::counts().s_full`); over both the exact non-terminal comparison set
and paper B's ZDD set the count is `3,608,880`. The 30 unreachable reps and 15
exceptional-terminal reps identified below are all non-self-symmetric.

## A — reachable representative count

- Paper B "擬到達可能" mirror-folded = **73,986,754,080**.
- This work's **exact** forward-reachable set (expanding try-win positions, since a
  player may decline an available try and move on) has **73,991,213,760** reps,
  *including* all 4,459,725 no-move (stalemate) terminal positions.
- For the like-for-like non-terminal comparison, exclude those no-move terminals:
  **73,986,754,035**. Paper B's folded pseudo-reachable count is **45** larger.
- That **45** decomposes into two different sets: **30** ZDD members that are not
  actually reachable, plus **15** mirror reps (30 unfolded positions) of reachable
  no-move terminals retained as `unknown` in the authors' database.
- Equivalently, with exact reachability `R`, all no-move terminals `N`, the authors'
  ZDD set `P`, retained terminals `T`, and unreachable ZDD members `U`:
  `P = (R − (N − T)) ∪ U`, with `|T|=15` and `|U|=30`.

## B — Table 8 (reachable W/L/D, pseudo-reachable)

| | this work | paper B |
|---|---|---|
| win | 106,144,078,911 | 106,144,078,911 |
| lose | 41,129,930,509 | 41,129,930,509 |
| draw | 695,889,860 | 695,889,860 |

Stage 3 first projects the independent full-space solution onto exact reachability `R`:

```text
mirror-rep W/L/D = 53,073,229,223 / 20,570,027,276 / 347,957,261
self-sym  W/L/D =      2,379,589 /      1,211,913 /      24,692
```

It then maps `R` to `P = (R − (N − T)) ∪ U`: remove all 4,459,725 no-move
terminals `N`, retain 15 representatives `T` as Draw, and add the independently
identified `U` as 27 Win/DTM1 + 3 Lose/DTM4. Only `N` contains self-symmetric reps
(7,314), giving normalized self-symmetric W/L/D =
2,379,589 / 1,204,599 / 24,692 and folded `|P| = 73,986,754,080`. Un-folding these
counts produces the exact Table 8 values above and pseudo total 147,969,899,280.

## C — Table A.1 (per-ply distribution, all 69 plies)

After the same `R → P` normalization—excluding `N − T`, adding `T` to Draw, and
adding `U` back as +54 at ply 1 and +6 at ply 4—**all 69 rows match exactly**
([`table_a1_reachable_vs_paperB.csv`](table_a1_reachable_vs_paperB.csv)): zero
differences and no unchecked rows.

The 6×5 stage-3 stream pass took 2,857.5 seconds (47.6 minutes), with peak RSS
17.4 GiB. It also writes the comparison CSV directly so a rerun can be checked
byte-for-byte against the committed artifact.

Special note on ply 48: the GPW PDF prints "879 28" (the last digit is truncated).
This work computes **879,284**, which is forced by Table A.1's lose rows summing to
Table 8 lose = 41,129,930,509 and is printed in full in the later journal version's
Table A·3.

## D — pinpoint checks

- deepest: **ply-69 wins = 30** — EXACT (full-space r69 = 15 mirror, all reachable,
  none self-symmetric → 2·15 = 30 pseudo).
- exceptional no-move configs retained as unknown = **30** unfolded positions — EXACT.
- average legal moves over non-terminal reachable nodes = **23.391** ≈ paper B 23.4.

## Exhaustive identification of the unreachable set

Paper B defines "擬到達可能" (pseudo-reachable) as a configuration satisfying certain
combinatorial conditions — by construction a **superset** of the truly-reachable set
(their §方法: "ゲーム開始点から到達可能で…配置は **すべて** 擬到達可能"). This work's
forward BFS computes the **exact** truly-reachable set (validated bit-for-bit against an
independent `HashSet<canonical_key>` BFS at 3×3 / 4×3 / 4×4).

`reachproj --stage cand` enumerated all **4,459,740** clear bits of the 6×5 exact
reachable bitset and attached each full-space value/DTM in one stream pass. The
authors' ZDD was then applied to every row, with each computed author ID decoded back
to the same 30-cell array as a round-trip check. Exactly **30** rows were members:

- 27 Win/DTM1 (15 immediate try wins, 12 other one-ply wins)
- 3 Lose/DTM4
- 0 Draw
- 0 self-symmetric

Both author IDs for every row are recorded in
[`candidates_zdd.csv`](candidates_zdd.csv). Direct reads of all 60 entries across the
downloaded `db02.bin`, `db09.bin`, and `db14.bin` shards matched their independent DTM
bytes exactly; see
[`paper_b_zdd_verification.md`](paper_b_zdd_verification.md). Thus paper B's prediction
of 60 unfolded unreachable positions is confirmed, not exceeded.
