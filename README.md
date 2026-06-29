# NOCCA×NOCCA — strongly solved (independent reproduction)

A size-parametric **strong solver** for the two-player board game **NOCCA×NOCCA**, and
an **independent reproduction** of the full 6×5 result first published by
**Yamamoto & Hoki, 「NOCCA×NOCCAの強解決」 / "Strongly Solving NOCCA×NOCCA",
第27回ゲーム・プログラミングワークショップ (GPW 2022)**.

> **Strongly solved** = the game value (Win/Loss/Draw) **and** distance-to-mate of
> *every one* of the **73,995,673,500** mirror-canonical positions is computed by
> retrograde analysis. The famous "**first player wins the initial position in 41
> moves**" is just one entry of that full solution.

> **Headline check:** the full per-move distribution of decided positions matches paper
> B's Table A.1 to **45 representatives out of 73.99 billion** (6×10⁻⁷). The only
> difference: paper B reports over a *pseudo-reachable* (over-approximate) set, while
> this work computes the *exact* reachable set — so our numbers are the precise subset,
> and we even recover a digit the published table dropped (ply-48).

Clean-room implementation: a separate engine, ranking, and solvers, sharing no code with
the original. See [`results/RESULT.md`](results/RESULT.md) and
[`results/comparison_with_paperB.md`](results/comparison_with_paperB.md).

## What was solved

| | |
|---|---|
| **All positions** (the strong solution) | value + DTM for **73,995,673,500** mirror reps |
| full-space W / L / D | 53,077,668,702 / 20,570,045,468 / 347,959,330 |
| initial position (6×5) | **first-player WIN, DTM 41** |
| reachable W/L/D (Table 8) / per-ply (Table A.1) | match paper B to 45 reps (6×10⁻⁷); **67/69 rows exact** |
| deepest win / no-move terminals / avg branching | 69 plies (30) / 30 / 23.4 — all match |

## How it works

1. **Strong solve — full-space retrograde** (`inmem_retro`): a bounded all-in-memory
   2-bit value array over a dense **mirror-canonical ranking** (`mirror_rank`) solves
   *every* representative (reachable or not) by backward induction, streaming the
   distance-to-mate to disk. **This is the strong solution.** 6×5: ~13.6 h, 34.6 GB RAM,
   a 368 GB DTM stream.

The remaining two steps exist only to **compare with paper B on the same footing** —
paper B reports its tables over the reachable set, so we project onto it. They do **not**
restrict the solution (which already covers every position):

2. **Forward reachability** (`reachable_bfs`): a parallel bitset BFS from the initial
   position computes the **exact reachable set** (73.99 billion reps). 6×5: ~13 h.
3. **Projection + un-fold** (`reachproj`): intersect the reachable set with the DTM
   stream and un-fold mirror reps to pseudo-reachable counts
   (`pseudo = 2·mirror − self_symmetric`, Burnside) → paper B's Table 8 / A.1.

## Why it's correct (independent validation)

No brute-force oracle exists at 6×5, so confidence is built from layers that each pass
*exactly* (details: [`results/logs/validation_summary.md`](results/logs/validation_summary.md)):

- **Two reduced-size oracles** — an in-RAM retrograde and a disk-based external-sort
  retrograde reproduce the Matsumoto–Kuroda reduced-version oracle (≤5×5).
- **Two independent retrograde systems** (in-memory rank-min vs disk key-min) agree at 5×5.
- The reachable BFS matches an **independent `HashSet<canonical_key>` BFS** bit-for-bit
  (3×3/4×3/4×4), and matches known reachable counts (4×4, 4×5, 5×5) exactly.
- **Internal invariants** hold at 6×5: `W+L+D = R_full`, `self_sym = S_full`, the
  Burnside un-fold identity, and the depth-1 win/lose parity over all 70 rounds.
- **Determinism + crash-safety**: byte-identical output across thread counts; real
  `kill -9` → resume → byte-identical answer, verified at 4×5 and 5×5.

(An early attempt to *weakly* solve the initial position by forward df-pn proof search
diverged on draw-prone positions — `src/solver.rs`, `docs/weak-solving-design.md` — which
motivated the retrograde strong solve. The forward solver is kept for the methodological
contrast and supplies the shared `Value` type.)

## Reproducing it

Everything is the **same code at every size** — only the board dimensions change.
See **[`REPRODUCING.md`](REPRODUCING.md)** for full recipes and the cost ladder:

| size | states | what runs | time |
|---|---|---|---|
| 4×4 | 1.5e7 | `cargo test` (oracle + cross-checks) | seconds |
| 4×5 | 1.1e9 | strong solve + reachable + project | minutes |
| 5×5 | 1.5e10 | strong solve + reachable | ~1–2 h |
| **6×5** | **7.4e10** | full pipeline | retrograde 13.6 h + BFS 13 h + project 48 min |

```sh
cargo build --release
cargo test --release --lib          # validate the engine + small-scale reproduction
```

## Layout

```
src/                 engine, ranking, retrograde strong-solvers (+ a forward weak-solver)
  varboard, position, moves, color, square   — size-parametric engine (try-type win)
  rank, mirror_rank                          — dense mirror-canonical ranking
  retro, disk_retro, extsort                 — in-RAM + disk external-sort retrogrades (oracles)
  inmem_retro                                — bounded in-memory retrograde (the strong solve)
                                               + exact reachable BFS + projection
  solver                                     — forward df-pn weak-solver (earlier attempt; shares Value)
  bin/{inmem, reachproj, disksolve, enumerate}
results/             solution summary, distributions, comparison, validation logs, stream SHA256
docs/                design notes (incl. the earlier forward weak-solving attempt)
```

The 368 GB DTM stream and the 9.25 GB bitsets are **not** committed (regenerable; the
stream is byte-deterministic — its SHA256 and per-round census are in `results/`).

## Dependencies & license

Single dependency: [`rayon`](https://crates.io/crates/rayon) (parallelism). No copyleft
dependencies. Dual-licensed **MIT OR Apache-2.0** (`LICENSE-MIT`, `LICENSE-APACHE`).

## Citation

Original result reproduced here:
山本 敦也, 保木 邦仁. **NOCCA×NOCCAの強解決** (Strongly Solving NOCCA×NOCCA).
第27回ゲーム・プログラミングワークショップ (GPW 2022), 情報処理学会, 2022.
