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
| **A** reachable reps (≥1 legal move) | 73,986,754,035 | 73,986,754,080 | 45 |
| **B** Table 8, reachable W (pseudo) | 106,144,078,857 | 106,144,078,911 | −54 |
| **B** Table 8, reachable L (pseudo, no-move excl.) | 41,129,930,503 | 41,129,930,509 | −6 |
| **B** Table 8, reachable D (pseudo) | 695,889,830 | 695,889,860 | −30 |
| **C** Table A.1 (69 plies) | — | — | **67 / 69 rows EXACT** |
| **D** deepest 69-ply wins | 30 | 30 | **EXACT** |
| **D** no-legal-move (終端) configs | 30 | 30 | **EXACT** |
| **D** avg legal moves (non-terminal) | 23.391 | 23.4 | match |

The only residual is **45 representatives out of 73.99 billion (6e-7)**, concentrated
at plies 1 and 4 (Table A.1) — see `comparison_with_paperB.md`. It is fully explained:
paper B's "擬到達可能" (pseudo-reachable) is, by their own definition, a combinatorial
**superset** of the truly-reachable set; this work's forward BFS computes the exact
non-terminal subset, which is 45 reps smaller. The no-legal-move terminal count is
the separate **30 vs 30** check in (D), not part of that residual. Our computation is
therefore *more precise* than the published numbers, and it also **recovered a digit
the published PDF dropped**: Table A.1
ply-48 prints "879 28" (missing last digit); our value 879,284 is independently confirmed
by paper B's own constraint (Table A.1 lose rows must sum to Table 8 lose = 41,129,930,509).

## Files
- `full_space_dtm_distribution.csv` — 70-round full-space DTM distribution (win/lose per DTM).
- `table_a1_reachable_vs_paperB.csv` — per-ply reachable distribution, this work vs paper B.
- `comparison_with_paperB.md` — the four comparisons (A/B/C/D) in detail.
- `dtm_stream.sha256` — byte-exact fingerprint of the 368 GB DTM stream (regenerable; see README).
- `logs/` — solve + reachability + validation logs.

The 368 GB DTM stream and the 9.25 GB reachable/self-symmetric bitsets are **not**
committed (too large); they are deterministically regenerable from the code (the stream
is byte-identical across thread counts). The per-round census above and the SHA256 let a
third party verify a regenerated stream.
