# Results — 6×5 NOCCA×NOCCA, strongly solved

Independent reproduction of **Yamamoto–Hoki, 「NOCCA×NOCCAの強解決」 /
"Strongly Solving NOCCA×NOCCA", GPW 2022**, by a fully independent implementation: a
bounded all-in-memory 2-bit retrograde over a dense mirror-canonical ranking computes the
value + DTM of **every** position (the strong solution); a forward reachable BFS +
projection is then used **only to compare with paper B's reachable-set tables** (it does
not restrict the solution).

## The headline result

| quantity | value |
|---|---|
| **initial position** | **first-player WIN, DTM = 41** (depth-1 convention) |
| solve wall time / peak RAM | 13.6 h (56 cores) / 34.6 GB |
| full mirror-rep space `R_full` | 73,995,673,500 |
| full-space W / L / D | 53,077,668,702 / 20,570,045,468 / 347,959,330 (= R_full) |
| deepest DTM | 69 |

`root = Win/41` exactly matches paper B's "先手は最短41手で勝ち".

## Reachable projection vs paper B

Paper B reports results over the **pseudo-reachable** set (a combinatorial
over-approximation). This work computes the **exact** forward-reachable set and
projects the full-space solution onto it.

| comparison | this work | paper B | difference |
|---|---|---|---|
| **A** exact non-terminal reps vs ZDD set | 73,986,754,035 | 73,986,754,080 | 45 (= 30 unreachable + 15 retained terminals) |
| **B** Table 8, reachable W (pseudo) | 106,144,078,857 | 106,144,078,911 | −54 |
| **B** Table 8, reachable L (pseudo, no-move excl.) | 41,129,930,503 | 41,129,930,509 | −6 |
| **B** Table 8, reachable D (pseudo) | 695,889,830 | 695,889,860 | −30 |
| **C** Table A.1 (69 plies) | — | — | **67 / 69 rows EXACT** |
| **D** deepest 69-ply wins | 30 | 30 | **EXACT** |
| **D** exceptional no-move configs retained as unknown | 30 | 30 | **EXACT** |
| **D** avg legal moves (non-terminal) | 23.391 | 23.4 | match |

The 45-representative cardinality difference in (A) is not 45 unreachable reps.
An exhaustive intersection with the authors' ZDD finds exactly **30 unreachable
mirror reps**: 27 Win/DTM1 and 3 Lose/DTM4, all non-self-symmetric, hence 60 unfolded
positions. The other 15 mirror reps are the 30 unfolded no-move terminal positions
that the authors explicitly retained as `unknown` in their database. Therefore the
Table 8 residual separates into Win 54 + Lose 6 from the unreachable set and Draw 30
from those reachable exceptional terminals.

This exactly confirms paper B's prediction of 60 unreachable positions. The complete
30-row ZDD intersection is in `candidates_zdd.csv`; direct random access to both author
IDs for every representative across `db02.bin`, `db09.bin`, and `db14.bin`
independently confirmed all 60 bytes: 54 Win/DTM1 (`1`) and 6 Lose/DTM4 (`4`), with
zero mismatches.

The GPW Table A.1 PDF truncates ply 48 as "879 28". The value is **879,284**: it is
forced by the Table 8 lose total and printed in full in the later peer-reviewed journal
version's Table A·3.

## Files
- `full_space_dtm_distribution.csv` — 70-round full-space DTM distribution (win/lose per DTM).
- `table_a1_reachable_vs_paperB.csv` — per-ply reachable distribution, this work vs paper B.
- `comparison_with_paperB.md` — the four comparisons (A/B/C/D) in detail.
- `paper_b_zdd_verification.md` — exhaustive ZDD intersection and direct DB-byte check.
- `candidates_zdd.csv` — all 30 unreachable ZDD members with both author IDs/offsets.
- `db02_verification.csv`, `db09_verification.csv`, `db14_verification.csv` — all 60
  directly checked author database bytes.
- `dtm_stream.sha256` — byte-exact fingerprint of the 368 GB DTM stream (regenerable; see README).
- `logs/` — solve + reachability + validation logs.

The 368 GB DTM stream, 9.25 GB reachable/self-symmetric bitsets, 547 MB candidate
pool, and author database shards are **not** committed. The solver artifacts are
deterministically regenerable; hashes, small result sets, and verification tools let a
third party check regenerated data without redistributing the large inputs.
