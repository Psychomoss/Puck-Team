# Results: Shopping Sessions in C++ (team puck)

The pipeline is C++20 (GCC, one static binary, zstd for the Parquet pages; no Arrow). It started from
the MIT-licensed C++ entry [mliao0128/fraun-shopping-sessions](https://github.com/mliao0128/fraun-shopping-sessions)
(see `LICENSE`) and was rebuilt around the design of our earlier Rust pipeline (branch `rust-final`),
whose techniques had measured about 2x less CPU on the same data.

All times are on the development laptop (Intel Core Ultra 5 228V: 4 P-cores + 4 low-power E-cores, no
hyperthreading, no AVX-512), through Docker with the scorer's flags (`scripts/docker_run.sh`,
`CPUS=8 MEM=24g`), wall clock from `docker run` start to exit, including about 0.4 s of container
overhead on this machine. The scorer has 8 vCPUs that are hyperthreads of ~4 physical Xeon cores.

## Wall time (Docker, 3 runs each, interleaved)

| dataset | reference C++ entry | this pipeline |
|---|---|---|
| practice-like (seed 7, 20M events) | 1.98, 1.88, 1.82 s | **1.14, 1.16, 1.10 s** |
| final-like (seed 11, 60M events) | 4.87, 4.86, 4.81 s | **2.66, 2.71, 2.66 s** |

Native whole-process wall (interleaved medians): practice 1561 ms (reference) -> 835 ms; our Rust
pipeline is 797 ms on the same runs, final 2338 ms (Rust) vs 2373 ms (C++).

## Stage breakdown (inside the container, one run, ms; CPU ms summed over threads)

| stage | practice | (cpu user / sys) | final | (cpu user / sys) |
|---|---|---|---|---|
| reference files | 36 | 29 / 9 | 34 | 31 / 4 |
| links | 81 | 331 / 66 | 143 | 880 / 122 |
| events parse | 415 | 2221 / 318 | 1131 | 7477 / 832 |
| dedup | 75 | 326 / 34 | 207 | 1300 / 116 |
| clicks | 78 | 488 / 72 | 250 | 1619 / 162 |
| sessions + attribution | 144 | 1036 / 34 | 434 | 3209 / 60 |
| write parquet | 67 | 275 / 49 | 185 | 841 / 109 |
| **total in process** | **896** | | **2383** | |

Peak RSS at final: 4.7 GB (limit 24 GB). Output: 11 MB at practice, 32 MB at final.

## Correctness

36/36 fixtures, the dev answer (117,038 user rows, 27,400 campaign rows), and the golden SHA-256 hashes
of practice and final (also with 1 thread and with the input file order reversed), natively and
through Docker: `scripts/verify.sh` (`DOCKER=1` for the image). `tests/test.cpp` checks the parsers
against plain scalar references (timestamp fuzzing, every date shape, the SIMD key packer against the
scalar one for every byte, the scanner against a second scanner); it passes under ASan and UBSan.

## Design

Work-stealing task pool (1 MB input chunks), every stage parallel:

1. **Input**: every `*.csv` cut into 1 MB ranges; a task `pread`s its range and owns the lines that
   start in it (each file's header skipped). A background thread `posix_fadvise(WILLNEED)`s all inputs.
2. **CSV scanner**: AVX2 masks of `\n`, `,`, `"` per 64-byte block; pass 1 flattens all comma and
   newline positions of the chunk, pass 2 walks lines from that list. Quoting is validated
   branch-free by counting quotes (legal only as first and last byte of a field); `\r` is data.
3. **IDs** become exact 8-byte keys: up to 10 chars of `[0-9A-Z_a-z]` packed into order-preserving
   6-bit codes with two `pshufb` nibble lookups, so integer order equals byte order; anything else
   goes through an interner and comparisons use the bytes.
4. **Timestamps**: F1/F2 dates come from a read-only table of the dates near the window (exact
   10-byte compare), `HH:MM:SS` is validated with SWAR; other dates take a full SIMD path; F3 is a
   right-aligned 16-digit SIMD parse with an overflow-checked fallback.
5. **Links**: lock-free open-addressing table of all linked IDs, lock-free union-find (CAS, path
   halving), group name = atomic minimum; one table maps each linked ID to its person.
6. **Events**: each copy validated on parse; valid copies get their person while parsing (prefetched,
   resolved in batches of 32 lines); copies go to 1024 event-id buckets (32-byte records, per-thread
   huge-page arenas); per bucket a small hash table keeps the smallest `ingest_ts` (dedup before
   validation), survivors re-bucketed by person (16-byte records).
7. **Clicks** validated, mapped to persons, bucketed the same way; no click dedup (copies are
   identical, so they change neither the credited click nor the set of assists).
8. **Sessions** per person bucket: LSD radix sort on (person id, ts), one sweep per person for
   sessions, last-click credit with a two-pointer window (ties by smallest `click_id`), assists as
   the union of `[click_ts, click_ts + 7d]` per campaign.
9. **Output**: our own Parquet writer: one row group per file, one file per thread, PLAIN data pages
   of 64K rows, zstd level 1, Thrift compact metadata. The process `_exit`s after the files are closed.

Build: `build.sh` (`-O3 -march=x86-64-v3 -flto`, static). Docker: `debian:bookworm-slim` build and
runtime images, `GLIBC_TUNABLES=glibc.malloc.hugetlb=1` (huge pages for malloc), uid 1000.

## Things we were unsure about in rules.md

- Click duplicates are assumed identical, as the rules state, so they are not deduplicated.
- Event copies with equal `ingest_ts` are assumed identical; any one is kept.
- An unparseable `ingest_ts` (the rules say it never happens) counts as +infinity and makes the copy invalid.
- An F3 value too long for int64 is a bad timestamp (it is outside every window anyway).
- Category `unknown` is sorted in with the real categories, so ties go by byte order.
- The window plus 7 days must be shorter than 2^32 ms (49 days) for the relative timestamps; checked at
  startup. Threads are capped at 62.
