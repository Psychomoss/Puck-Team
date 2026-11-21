# Results: Shopping Sessions in Rust (team puck)

All times are on the development laptop (Intel Core Ultra 5 228V: 4 P-cores + 4 low-power E-cores, no
hyperthreading, no AVX-512), through Docker with the scorer's flags (`scripts/docker_run.sh`,
`CPUS=8 MEM=24g`), median of 3 runs, wall clock from `docker run` start to exit. Laptop timings will not
match the scorer: the scorer has 8 vCPUs that are hyperthreads of only ~4 physical Xeon cores, but
AVX-512 and server memory. About 0.25 s of every run is fixed Docker overhead (a no-op container with the
same flags takes 0.25 s here).

Measured code: commit `257e318` (this report is committed right after it; no code changes).

## Wall time (Docker, median of 3)

| dataset | events | median | runs |
|---|---|---|---|
| dev (seed 1) | 1M | **0.39 s** | 0.393, 0.391, 0.378 |
| practice-like (seed 7) | 20M (like the website's practice run) | **1.13 s** | 1.101, 1.134, 1.161 |
| final-like (seed 11) | 60M | **2.81 s** | 2.815, 2.817, 2.799 |

Progress over the optimization phase (practice / final): 2.54 / 7.48 s (first correct version) ->
1.13 / 2.81 s. The full log of attempts, kept and rejected, is in `PROGRESS.md`.

## Stage breakdown (inside the container, one run, ms; CPU ms summed over threads)

| stage | practice | (cpu user / sys) | final | (cpu user / sys) |
|---|---|---|---|---|
| reference files | 14 | 9 / 5 | 14 | 12 / 1 |
| links parse | 20 | 88 / 36 | 50 | 284 / 38 |
| links union-find | 32 | 181 / 25 | 74 | 445 / 50 |
| events parse | 348 | 2225 / 447 | 1031 | 6557 / 1115 |
| dedup + person lookup | 148 | 931 / 90 | 506 | 3664 / 168 |
| clicks | 89 | 456 / 36 | 251 | 1691 / 135 |
| sessions + attribution | 155 | 1042 / 16 | 428 | 3140 / 49 |
| write parquet | 37 | 172 / 32 | 88 | 489 / 90 |
| **total in process** | **844** | | **2441** | |

## Peak memory

Peak RSS of the process (native run, `/usr/bin/time`): practice 1.7 GB, final 4.4 GB (limit 24 GB).

## Correctness

- 36/36 fixtures, through Docker.
- Dev set: exact match with `dev-answer/` (117,038 user rows, 27,400 campaign rows).
- Practice and final: the rules' self-checks hold, and the sorted canonical CSV export matches the
  golden SHA-256 hashes in `PROGRESS.md` / `scripts/golden.sha256`, also with 1 thread and with the
  input file order reversed.
- `scripts/verify.sh` runs all of these (`DOCKER=1` for the image); `scripts/bench.sh` benchmarks.

## Design

One static pass per stage, all stages parallel with a work-stealing task counter (1 MB input chunks, so
the slow E-cores just take fewer chunks):

1. **Input**: every `*.csv` is cut into 1 MB byte ranges; a task `pread`s its range into a reused buffer
   and owns the lines that start inside it (header skipped per file).
2. **CSV scanner**: AVX2 bitmasks of `\n`, `,` and `"` per 64-byte block; fields come from the comma
   and newline bits, quoting is validated branch-free by counting quotes (a quote is legal only as the
   first and last byte of a field). Only `\n` ends a line, so `\r` stays in the data.
3. **IDs** become exact 8-byte keys: up to 10 chars of `[0-9A-Z_a-z]` are packed as order-preserving
   6-bit codes (SSE), so integer order equals byte order; anything else (spaces, long IDs, non-ASCII)
   goes through an interner, and comparisons then use the real bytes.
4. **Timestamps**: F1/F2 validated with two 16-byte compares and converted with a shuffle + multiply-add
   and a days-from-civil formula; F3 is a right-aligned 16-digit SIMD parse (overflow-checked scalar
   path for longer values). Differential tests against a plain scalar reference.
5. **Links**: a lock-free open-addressing table interns all linked IDs in parallel; a lock-free
   union-find (CAS on roots, path halving) joins them; each group's name is an atomic min over keys.
6. **Events**: each copy is validated on parse (validity, lateness), then bucketed by `event_id` into
   1024 buckets of 32-byte records; per bucket a small hash table keeps the copy with the smallest
   `ingest_ts` (dedup before validation, as the rules require), and valid survivors are mapped to their
   person (prefetched lookups) and re-bucketed by person into 16-byte records. Purchases carry a handle
   to a side table with price and category.
7. **Clicks** are validated, mapped to persons and bucketed the same way. Click dedup is skipped:
   copies are identical, so they change neither the credited click nor the set of assisting campaigns.
8. **Sessions**, per person bucket: LSD radix sort on a packed (person id, ts) key; one linear sweep
   per person builds sessions, credits each with a two-pointer window over the person's clicks (ties
   in `ts` broken by the byte-wise smallest `click_id`), and computes assists as the union of
   `[click_ts, click_ts + 7d]` intervals per campaign, so the cost is linear in clicks.
9. **Output**: Parquet via `arrow`/`parquet` (plain encoding, no compression, no statistics), user
   rows split into equal 256K-row files written in parallel; the process exits without freeing memory.

Build: Rust 1.95 in Docker (`rust:1.95-slim-bookworm`), `opt-level=3`, fat LTO, `codegen-units=1`,
`panic=abort`, `target-cpu=x86-64-v3` (AVX2, so the same binary runs on the laptop and the scorer),
mimalloc with page purging off; runtime image `debian:bookworm-slim` (needed for `chmod` in
`docker_run.sh` cleanup), running as uid 1000 with a read-only root.

## Things I was unsure about in rules.md

- **Click duplicates** are assumed identical as the rules state, so no click dedup is done. If two
  copies of a `click_id` ever differed, the result would depend on which is kept anyway.
- **Event copies with equal `ingest_ts`** are assumed identical (rules say so); any one is kept.
- **`ingest_ts` is "always valid F1"**: it is parsed with the general parser; an unparseable one would
  be treated as +infinity (never kept over a parseable copy) and make that copy invalid.
- **F3 too long for i64** is treated as a bad timestamp, which the rules equate with "outside the
  window" for validity.
- **Session day** is the UTC date of the session start; a session that starts inside the window always
  has a day inside the window's days.
- **Category `unknown`** is inserted into the sorted category list, so a tie between `unknown` and a
  real category goes by byte order as the rules say.
- **Limits of this implementation**: the window plus 7 days must be shorter than 2^32 ms (49 days; the
  generator always uses 14 days) because timestamps are stored relative to the window; this is asserted
  at startup. Threads are capped at 62.
