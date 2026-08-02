# NOCCA×NOCCA — strongly solved (independent reproduction)

A size-parametric **strong solver** for the two-player board game
**[NOCCA×NOCCA](https://www.undanoga.com/)** (published by Undanoga, 2017), and
an **independent reproduction** of the full 6×5 result first published by
**Yamamoto & Hoki, 「NOCCA×NOCCAの強解決」 / "Strongly Solving NOCCA×NOCCA",
第27回ゲーム・プログラミングワークショップ (GPW 2022)**.

> **Strongly solved** = the game value (Win/Loss/Draw) **and** distance-to-mate of
> *every one* of the **73,995,673,500** mirror-canonical positions is computed by
> retrograde analysis. The famous "**first player wins the initial position in 41
> moves**" is just one entry of that full solution.

> **Headline check:** **67 of 69** per-move rows match paper B's Table A.1 exactly.
> The two residual rows are exactly 30 unreachable mirror representatives (60 unfolded
> positions), confirmed by intersecting all 4,459,740 unreachable candidates with the
> authors' ZDD. Direct reads from the authors' `db02.bin`, `db09.bin`, and `db14.bin`
> also match all 60 value/DTM bytes. The GPW PDF's truncated ply-48 value is confirmed
> as 879,284 by the later journal version.

The solver itself is a clean-room implementation: a separate engine, ranking, and
retrograde sharing no code with the original. After the independent computation was
complete, the optional post-hoc verifier under `tools/paper_b_verification/` deliberately
used the authors' public-domain ZDD code to cross-check set membership and database IDs;
it is not linked into the solver. See [`results/RESULT.md`](results/RESULT.md) and
[`results/comparison_with_paperB.md`](results/comparison_with_paperB.md).

## What was solved

| | |
|---|---|
| **All positions** (the strong solution) | value + DTM for **73,995,673,500** mirror reps |
| full-space W / L / D | 53,077,668,702 / 20,570,045,468 / 347,959,330 |
| initial position (6×5) | **first-player WIN, DTM 41** |
| reachable comparison (Table 8 / Table A.1) | 30 unreachable reps = 60 positions; **67/69 rows exact** |
| deepest win / paper's exceptional terminals / avg branching | 69 plies (30) / 30 / 23.4 — all match |

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
- **Author-data cross-check**: all 4,459,740 unreachable candidates were intersected
  with the authors' ZDD, yielding exactly 30 mirror reps; all 60 bytes across
  `db02.bin`, `db09.bin`, and `db14.bin` agree with the independent values and DTMs
  with zero mismatches.
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
tools/paper_b_verification
                     author-ZDD intersection + direct database-byte verifier
```

The 368 GB DTM stream, 9.25 GB bitsets, 547 MB candidate pool, and author database
shards are **not** committed. Small verification outputs, hashes, and reproduction
instructions are in `results/` and `tools/paper_b_verification/`.

## Dependencies & license

The Rust solver's single dependency is [`rayon`](https://crates.io/crates/rayon)
(parallelism). The optional C++ verification tools require only a C++11 compiler;
their vendored author ZDD source is public domain as documented in its notice. No
copyleft dependencies. The project is dual-licensed **MIT OR Apache-2.0**
(`LICENSE-MIT`, `LICENSE-APACHE`).

## References

This is an independent reproduction; the works below are cited for validation and
context, not used as source material.

**Primary result reproduced here**

- 山本 敦也, 保木 邦仁. **NOCCA×NOCCAの強解決** (Strongly Solving NOCCA×NOCCA).
  第27回ゲーム・プログラミングワークショップ (GPW 2022), 情報処理学会, 2022.
  <https://cir.nii.ac.jp/crid/1050856970555547904>
- 山本 敦也, 保木 邦仁. **NOCCA × NOCCAの強解決**. 情報処理学会論文誌,
  Vol.64, No.12, pp.1678–1688, 2023. <https://doi.org/10.20729/00231448>

**Related work**

- 松本 優希, 黒田 久泰. **ノッカノッカの縮小版に対する後退解析**.
  第84回全国大会講演論文集, 情報処理学会, 2022, pp. 437–438.
  <https://cir.nii.ac.jp/crid/1050575495579222912> — the reduced-version retrograde
  reproduced here as a cross-check oracle (≤5×5).
- 池内 明伸, 山口 勇太郎. **ボードゲーム「ノッカノッカ」の一般化と解析**.
  情報処理学会研究報告 Vol.2022-AL-189, No.6, 2022.
  <https://jglobal.jst.go.jp/detail?JGLOBAL_ID=202202214390869382> — generalizes the board to (n×m)-NOCCA
  and proves the initial position is a draw for n=2, m≥5 and n=3, m≥7.
- 諏訪 壮紀. **ボードゲーム「ノッカノッカ」の解析** (Analysis of a Board Game "NOCCA NOCCA").
  法政大学情報科学部 卒業論文要旨, 2022.
  <https://hosobe.cis.k.hosei.ac.jp/lab/wp-content/uploads/2022/03/t_suwa-bthesis-abstract.pdf>
  — retrograde analysis of reduced versions (bachelor's thesis abstract).
