# Validation summary — why the 6×5 result is trustworthy

The 6×5 run cannot be checked against a brute-force oracle (too large), so confidence
comes from layered, *independent* validations that each pass exactly.

## 1. Small-size oracles (value + DTM)
- In-RAM retrograde (`retro::solve_ram`) and the disk retrograde reproduce the
  **Matsumoto–Kuroda reduced-version oracle** (≤5×5): value exact, DTM = oracle − 1
  (a uniform definitional offset of the try-type rule). Baked into the test gates.
- 4×4 initial position = Win/DTM21 (D1); 5×5 = Win/DTM33 (D1); etc.

## 2. Two independent solver systems agree
- **In-memory** (rank-min canonical, full-space, 2-bit array) and **disk** (key-min
  canonical, reachable set, external sort) are separate codepaths. At 5×5 both give
  the initial position = Win (DTM-0 distance 32); the disk run completed independently.

## 3. Reachable BFS = an independent forward BFS
- `reachable_bfs` (mirror_rank bitset) matches a plain `HashSet<canonical_key>` forward
  BFS **bit-for-bit on the reachable count**, for BOTH try-win modes, at 3×3/4×3/4×4
  (test `reach_bfs_matches_hashset`).
- Reachable counts match independently-known values:
  - 4×4 = 9,700,372 (G0.5 oracle), 4×5 = 1,104,675,849 (disk method),
    5×5 = 9,362,329,436 (disk method) — all EXACT.

## 4. Internal invariants (hold at every size incl. 6×5)
- W + L + D = R_full (no integer overflow at 74e9; >u32 stress passed at 5×5/6×5).
- self_sym count = S_full = mirror_rank::counts().s_full (3,623,508 at 6×5).
- Burnside un-fold identity: pseudo total = 2·reachable − S_reach.
- D1 parity: wins only on odd DTM, loses only on even (70 rounds at 6×5).

## 5. Determinism + crash-safety
- DTM stream + value array are **byte-identical across thread counts** (1/4/16/56),
  by per-round sorted output + commutative atomic OR.
- Resume verified by real `kill -9` → restart → byte-identical answer, at 4×5 and 5×5
  (in-memory retrograde) and at 4×5 (reachable BFS, layer-boundary and intra-layer).

## 6. Mid-run correctness signals at 6×5
- Round-0 no-move terminals = 4,459,725 = the independent terminal census.
- Round-1 try-win wins = 38,827,875,334 (= the 194 GB written at round 1).
- The deepest layers of the full-space DTM distribution match paper B's Table A.1
  exactly (plies 64–69, including ply-69 = 30) — an independent landing on the
  published deepest numbers.

## 7. Authors' ZDD and database agree

- Every one of the 4,459,740 exact-unreachable mirror reps was tested against the
  authors' 147,969,899,280-path ZDD. Exactly 30 reps are members: 27 Win/DTM1,
  3 Lose/DTM4, no Draw, and no self-symmetric reps.
- Each author ID was decoded back through the ZDD and matched the original 30-cell
  board exactly; the 30 reps yield 60 distinct IDs after horizontal reflection.
- Direct random reads of both orientations of all 30 reps across `db02.bin`, `db09.bin`,
  and `db14.bin` match the independent DTM bytes exactly: 54 bytes equal 1 and 6 bytes
  equal 4, with zero mismatches among 60 distinct author IDs.

See `RESULT.md` / `comparison_with_paperB.md` for the final figures.
