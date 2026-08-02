# Author-ZDD intersection and database-byte verification

The exact unreachable pool (`R_full − R`) contains 4,459,740 mirror
representatives. Intersecting every row with the authors' ZDD gives exactly 30
members:

The archived input `candidates.csv.gz` has SHA-256
`2abbadf791ddd421cee615f9db73e434b60ed67e37a10ea86f8e369382125405`.

| independent value | mirror reps | unfolded positions | self-symmetric |
|---|---:|---:|---:|
| Win / DTM 1 | 27 | 54 | 0 |
| Lose / DTM 4 | 3 | 6 | 0 |
| Draw | 0 | 0 | 0 |
| **total** | **30** | **60** | **0** |

Of the 27 Win/DTM1 representatives, 15 have an immediate try win and 12 win by
passing a no-move terminal to the opponent. Filtering only on `try_win` would
therefore miss 12 positions.

The committed [`candidates_zdd.csv`](candidates_zdd.csv) contains each mirror
rank, independent value/DTM, board, both author ZDD IDs, database shard, and byte
offset. Its SHA-256 is
`67cbf24dc42303b44e880745be5ea4dbfcf6f5b77ab18a0adeb5eb05cca415ee`.

## Direct `db14.bin` check

The authors' `db14.bin` shard has the documented size of 9,864,659,952 bytes.
It contains both orientations of 15 of the 30 representatives. Random access at
the 30 ZDD-derived offsets produced:

| expected byte | meaning | entries | mismatches |
|---:|---|---:|---:|
| 1 | Win / DTM 1 | 24 | 0 |
| 4 | Lose / DTM 4 | 6 | 0 |
| **total** | | **30** | **0** |

The byte-level evidence is committed as
[`db14_verification.csv`](db14_verification.csv). The large database shard is
not redistributed.

## Correct interpretation of the 45-representative count difference

Let `R` be the exact reachable set, `N` its no-move terminals, `P` the paper's
ZDD set, `T` the exceptional no-move terminals retained as unknown in the
authors' database, and `U = P − R`. Then

```text
P = (R − (N − T)) ∪ U
|T| = 15 mirror reps
|U| = |P| − |R| + |N| − |T| = 30 mirror reps
```

Thus the 45-representative cardinality difference between `P` and the exact
non-terminal set `R − N` decomposes into 30 unreachable representatives plus
15 reachable exceptional terminals. In Table 8, Win 54 + Lose 6 are the 60
unreachable unfolded positions; Draw 30 are the reachable exceptional terminals
stored as unknown. The latter are not additional unreachable positions.
