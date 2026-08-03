# Reproducing the results

Everything below is the **same binaries** at every board size — only `<rows> <cols>`
change. Build once:

```sh
cargo build --release
```

Binaries (in `target/release/`):
- `inmem <rows> <cols> <stream_dir> [--conv d1|d0] [--snap-dir DIR] [--snap-interval SECS] [--base]`
  — full-space retrograde; writes the per-round distance-to-mate **stream** to `<stream_dir>`.
  `--conv d1` = paper-B counting (try-win = DTM 1). Re-run the same command to resume.
- `reachproj <rows> <cols> <stream_dir> --work-dir DIR [--stage 1|2|3|all|uf|cand] [--expand-try] [--ckpt-interval SECS]`
  — stage 1: exact reachable-set BFS (`--expand-try` = paper-B reachability); stage 2:
  self-symmetric bitset; stage 3: project reachable ∩ stream and un-fold → Table 8 / A.1;
  `cand`: export every unreachable representative with its value/DTM for the author-ZDD check.
- `disksolve <rows> <cols> <workdir>` — the independent disk external-sort retrograde.
- `enumerate [--max N]` — `HashSet` forward census of the 6×5 reachable graph (early growth).

## Tier 0 — seconds: tests (validates the engine + small-scale reproduction)

```sh
cargo test --release --lib
```
Runs the reduced-size oracle gates (value/DTM vs the Matsumoto–Kuroda oracle), the
`HashSet`-BFS cross-check (`reach_bfs_matches_hashset`), determinism, and `kill`-style
resume answer-match. All 45 pass.

## Tier 1 — 4×5 (minutes, 1.1e9 states)

```sh
S=/tmp/s45; W=/tmp/w45; rm -rf $S $W
./target/release/inmem 4 5 $S --conv d1
#  → 4x5 Depth1: root=1/23 ...  (first player wins, DTM 23)
./target/release/reachproj 4 5 $S --work-dir $W --stage 1 --expand-try
#  → STAGE1 4x5: reachable=2082095574 ...
./target/release/reachproj 4 5 $S --work-dir $W --stage 2     # self_sym
./target/release/reachproj 4 5 $S --work-dir $W --stage 3     # projection
```

## Tier 2 — 5×5 (~1–2 h, 1.5e10 states)

Same as Tier 1 with `5 5`. Retrograde ≈ 2 h; reachable BFS ≈ 1 h. (Keep the stream
for stage 3.) Expected: `root=1/33` (Win, DTM 33), reachable = 9,362,329,436.

## Tier 3 — 6×5 (the published result; multi-day)

Hardware used: 56 cores, 64 GB RAM, a ~640 GB disk for the stream (HDD is fine — the
stream is written/read sequentially), and an SSD for snapshots/bitsets (~75 GB).
On NUMA, prefix with `numactl --interleave=all`. Run each as a detached service
(e.g. `systemd-run --user`) since steps are long.

```sh
HDD=/mnt/hdd; SSD=/mnt/ssd
# 1. retrograde  (~13.6 h; 368 GB stream on HDD, snapshots on SSD; resume = same command)
numactl --interleave=all ./target/release/inmem 6 5 $HDD/6x5_stream --conv d1 \
    --snap-dir $SSD/snap --snap-interval 7200
#    → 6x5 Depth1: root=1/41 rounds=69 W=53077668702 L=20570045468 D=347959330 ...

# 2. exact reachable-set BFS  (~13 h; intra-layer checkpoint → ≤~1 h rollback)
numactl --interleave=all ./target/release/reachproj 6 5 $HDD/6x5_stream \
    --work-dir $SSD/rp --stage 1 --expand-try --ckpt-interval 2700
#    → STAGE1 6x5: reachable=73991213760 try_win_term=38793123070 no_move_term=4459725 ...

# 3. self-symmetric bitset  (~22 min)
numactl --interleave=all ./target/release/reachproj 6 5 $HDD/6x5_stream \
    --work-dir $SSD/rp --stage 2          # → self_sym(S_full)=3623508 OK

# 4. projection → Table 8 / Table A.1  (~48 min, sequential stream scan)
numactl --interleave=all ./target/release/reachproj 6 5 $HDD/6x5_stream \
    --work-dir $SSD/rp --stage 3
#    → Table 8 ALL OK; Table A.1 compared=69 DIFF=0 unchecked=0
cmp $SSD/rp/table_a1_reachable_vs_paperB.csv \
    results/table_a1_reachable_vs_paperB.csv

# 5. export unreachable candidates  (~48 min, one more sequential stream scan)
numactl --interleave=all ./target/release/reachproj 6 5 $HDD/6x5_stream \
    --work-dir $SSD/rp --stage cand
#    → candidates.csv: 4,459,740 rows

# 6. author-ZDD intersection (seconds after building the optional C++ tools)
make -C tools/paper_b_verification
tools/paper_b_verification/filter_candidates \
    $SSD/rp/candidates.csv /tmp/candidates_zdd.csv
cmp /tmp/candidates_zdd.csv results/candidates_zdd.csv
```

**Resume:** any step survives crash / power loss / SIGKILL — re-run the identical
command; it auto-detects the stream marker (`inmem`) or the `bfs.ckpt` checkpoint
(`reachproj`). Max rollback ≤ ~2 h (retrograde, round-2 snapshots) / ≤ ~1 h (BFS,
intra-layer cursor).

**Verifying without re-running 6×5:** `results/` ships the final solution summary, the
70-round full-space DTM distribution, the per-ply reachable distribution vs paper B,
the 30-row author-ZDD intersection, all 60 direct author-database byte checks, and the
**SHA256 of the 368 GB stream**. Since the stream is byte-deterministic (independent of
thread count), a regenerated stream can be checked against that hash and against the
per-round census in `results/full_space_dtm_distribution.csv`.

## Notes
- `--base` disables the (oracle-identical) speed levers — use it to re-verify
  byte-identity of the optimized path.
- `--conv d0` uses distance-to-mate 0 for try-wins; `--conv d1` (default here) matches
  paper B's move counting and is what the published DTM 41 refers to.
