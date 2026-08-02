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

## Direct author-database check (60/60)

Both author IDs of every representative were read directly from the three applicable
database shards. Each shard has the documented size of 9,864,659,952 bytes. Random
access at all 60 ZDD-derived offsets produced:

| shard | orientations covered | byte 1 (Win / DTM 1) | byte 4 (Lose / DTM 4) | total | mismatches |
|---|---|---:|---:|---:|---:|
| `db02.bin` | mirror of 15 reps | 15 | 0 | 15 | 0 |
| `db09.bin` | primary of 15 reps | 15 | 0 | 15 | 0 |
| `db14.bin` | both orientations of 15 reps | 24 | 6 | 30 | 0 |
| **total** | **both orientations of all 30 reps** | **54** | **6** | **60** | **0** |

The 60 rows contain 60 distinct author IDs and no duplicate. Their split exactly
matches the independently solved 54 Win/DTM1 + 6 Lose/DTM4 unfolded positions.

The downloaded shard fingerprints are:

```text
c81f27b3c376e4bb46117474566b68f594b1917b542578eecc6cc91556c37071  db02.bin
0df9f19a72fe32c263980663e6686a6c9616678418091549edb5cadafc739c63  db09.bin
803dba7e9a004124ed9e72d7ec226db942d02d15730db2e077b034dafe81557e  db14.bin
```

The byte-level evidence is committed as [`db02_verification.csv`](db02_verification.csv),
[`db09_verification.csv`](db09_verification.csv), and
[`db14_verification.csv`](db14_verification.csv). Their SHA-256 fingerprints are,
respectively, `4412301df9c8ff44d3c64719f84f0002c4c2ca1b154e43a826c0bf9bf547b2a7`,
`1b80b639005a76621a1ba1b0ee35c061ce78984d7251c374bb4f909bb5f49c3b`, and
`9d628abf089624e37bee48639d32fea3ebc66a63aa1734143acb6824b26e2fb8`.
The large database shards are not redistributed.

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
