# Paper-B ZDD and database verification

These small C++ tools reproduce the final cross-check against the supplemental
ZDD and byte database published by Yamamoto and Hoki.

The large inputs are intentionally not committed:

- `candidates.csv`: 4,459,740 unreachable mirror representatives emitted by
  `reachproj --stage cand` (547 MB; gzip SHA-256
  `2abbadf791ddd421cee615f9db73e434b60ed67e37a10ea86f8e369382125405`)
- `dbNN.bin`: author database shards, 9,864,659,952 bytes each

The 30-row intersection and all 60 direct author-database byte checks are committed
under [`results/`](../../results/).

## Build

```sh
make -C tools/paper_b_verification
```

`zdd.cpp` and `zdd.hpp` are the authors' public-domain `sample-code-2` ZDD
implementation; see [NOTICE.md](NOTICE.md).

## 1. Generate the unreachable pool

After stages 1 and 2 and the full-space solve have produced `reachable.bits`
and the DTM stream:

```sh
cargo run --release --bin reachproj -- \
  6 5 /path/to/6x5_stream \
  --work-dir /path/to/rp6x5 \
  --stage cand
```

The 6×5 run scanned the 368 GB stream once and wrote `candidates.csv` in about
48 minutes. It enumerated all 4,459,740 clear bits of `reachable.bits`.

## 2. Intersect with the authors' ZDD

```sh
tools/paper_b_verification/filter_candidates \
  /path/to/candidates.csv \
  /tmp/candidates_zdd.csv
cmp /tmp/candidates_zdd.csv results/candidates_zdd.csv
```

The filter:

1. converts `reachproj`'s row/home orientation and stack strings to the authors'
   30-location object IDs;
2. applies the exact material/top-piece condition used to construct the ZDD;
3. computes the author database ID;
4. decodes that ID back through the ZDD and requires a cell-for-cell round trip;
5. repeats the ID calculation for the horizontal mirror.

Expected summary:

```text
author ZDD paths=147969899280
input=4459740 member=30 rejected=4459710
Lose/DTM4=3
Win/DTM1=27
```

All 30 representatives are non-self-symmetric, so they unfold to 60 positions:
54 Win/DTM1 and 6 Lose/DTM4. There are no Draw members in the unreachable
pool.

## 3. Verify author database bytes

The database byte is the DTM: zero means unknown/draw, odd means Win, and even
means Lose. The verifier checks the ID-to-shard mapping, seeks only the listed
bytes, and compares each byte with the independently computed DTM.

```sh
for shard in db02 db09 db14; do
  tools/paper_b_verification/verify_db \
    results/candidates_zdd.csv \
    /path/to/${shard}.bin \
    > /tmp/${shard}_verification.csv
  cmp /tmp/${shard}_verification.csv results/${shard}_verification.csv
done
```

`db02.bin` contains the mirror IDs of 15 representatives, `db09.bin` contains their
primary IDs, and `db14.bin` contains both orientations of the other 15 representatives.
Together they cover 60 distinct IDs: all 54 Win bytes equal `1` and all 6 Lose bytes
equal `4`, with zero mismatches. Input and output SHA-256 fingerprints are recorded in
[`results/paper_b_zdd_verification.md`](../../results/paper_b_zdd_verification.md).
