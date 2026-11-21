# PROGRESS

Team puck, Shopping Sessions, C++ (Rust until Phase 5; that code is on branch `rust-final`). Read this and `git log` first when resuming.

## Status
- Phase 0 (setup): done.
- Phase 1 (correct Rust pipeline): done. Gate 1 passed.
- Phase 2 (Docker): done. Multi-stage (rust:1.95-slim-bookworm -> debian:bookworm-slim, needs `chmod` for
  docker_run.sh cleanup), mimalloc, fat LTO, x86-64-v3. Docker: fixtures 36/36, dev MATCH (0.53 s wall incl. container start).
- Phase 4 (final check + report): done. Fresh no-cache image build, fixtures 36/36, dev MATCH, verify.sh
  PASS through Docker and natively (incl. 1-thread and reversed order). TEAM.md and RESULTS.md written.
- Phase 3 (performance): done (see log; stopped after 7 consecutive <2% ideas). `scripts/bench.sh` = docker median of 3 (CPUS=8 MEM=24g) + verify.sh.
  `perf` is unavailable (perf_event_paranoid=2, no root), so profiling uses stage timers + micro-benchmarks.

- Phase 5 (2026-11-21, after board score 1.29 s vs 0.64 s for the C++ entry mliao0128/fraun-shopping-sessions):
  compared both on the laptop. The C++ entry is slower here (practice docker 1.9 s vs ours 1.2 s; CPU 9.1 s vs
  5.9 s, RSS 3.6 GB vs 1.7 GB), so the board gap is environmental. The clearest difference was output size
  (ours 80 MB uncompressed, theirs 22 MB Snappy): output is now zstd(1) with dictionaries on the
  low-cardinality columns, 9.4 MB at practice. See the Phase 5 log below.

## How to check
- `scripts/verify.sh` runs fixtures (36), dev compare vs `dev-answer/`, golden hashes on practice and final,
  plus 1-thread (`NCPU=1`) and reversed-file-order (`REVERSE_FILES=1`) runs. `QUICK=1` skips the last two,
  `DOCKER=1` runs the image through `scripts/docker_run.sh`.
- `scripts/canon.py OUT DEST` runs the rules' self-checks and writes sorted canonical CSVs + SHA-256.

## Environment notes
- Rust was not installed: installed rustup (stable 1.100) into ~/.cargo. Python polars lives in `ROOT/.venv`.
- Datasets in `datasets/` (dev seed 1, practice seed 7 20M, final seed 11 60M); golden CSVs in `datasets/golden/`.

## Golden outputs (sorted canonical CSV, SHA-256)
| dataset | table | rows | sha256 |
|---|---|---|---|
| practice | user_daily | 2242393 | a8122fa5f73125faea45abc580c702405f2976f39c5548b4994cd0105f5dbaf7 |
| practice | campaign_daily | 28014 | d7326aea1c44bbf122e003685164c668a431794030392cbf9cc14d6efac2136e |
| final | user_daily | 6633859 | dbe08ea3681b2e0533b732acbbb0f3b428595386c3bd29503d5e84bbb4b076f1 |
| final | campaign_daily | 28014 | cf7b1ffb9eb94439fb0bd8405bfd128b5c3deca2bb6556cc24bdd04b2a1dad24 |

Also in `scripts/golden.sha256` (used by verify.sh).

## Decisions
- Click dedup is skipped: rules say copies of a click are identical, so duplicates change neither the
  credited click (max ts, then min click_id) nor the set of assisting campaigns.
- Event dedup ties (equal ingest_ts) keep any copy: rules say such copies are identical.
- An unparseable `ingest_ts` (never happens; rules say always valid F1) is treated as +inf and the copy invalid.
- An F3 timestamp overflowing i64 is treated as bad (it is outside every window anyway).
- IDs are exact keys: <=15 bytes packed big-endian into u128 with length in the low byte (u128 order ==
  byte order); longer strings go through a global interner (never hit by generator data; unit-tested).
- Category `unknown` is added to the sorted category list, so index order == byte order for ties.
- Session day = floor(start / 1 day) as Date32; campaign arrays indexed by day relative to window start.

## Final timings (Docker median of 3, laptop): dev 0.39 s, practice 1.13 s, final 2.81 s

## Timings (native, laptop, 8 threads, v1)
| dataset | total |
|---|---|
| dev | 0.27 s |
| practice | 3.9 s |
| final | 9.8 s (maxrss 12.5 GB) |

## Phase 3 log (Docker medians, laptop; practice / final)
| # | change | practice | final | kept |
|---|---|---|---|---|
| 0 | v1 baseline | 2.54 s | 7.48 s | - |
| 1 | AVX2 64-byte-block scanner (bitmasks for `\n` `,` `"`; quote validity by quote count), 6-bit order-preserving u64 ID keys (interner fallback), 40 B event records, NB=1024 buckets, lock-free link hash table + CAS union-find + atomic fetch_min names, counting-sort grouping per person bucket, prefetched person lookups, mimalloc purge_delay=-1 | 1.68 s | 4.07 s | yes |

| 2 | branch-free quote stripping in scanner (46 -> 30 ns/line), batched+prefetched click person lookups, person stage rewritten: LSD radix sort on packed (person id, ts) 16 B records, click ties resolved at credit time, assists via per-campaign interval union (O(clicks) instead of O(sessions x window clicks)) | 1.32 s | 3.28 s | yes |
| 3 | pread into per-thread buffers instead of mmap; append-only block lists (1024-record blocks) for all bucket stores instead of over-reserved Vecs; per-stage user/sys CPU in timings. Peak RSS final 15 GB -> 5.7 GB | 1.24 s | 3.22 s | yes |
| 4 | SIMD timestamps (F3: right-aligned 16-digit maddubs/madd; F1/F2: two 16-byte compares + shuffle), `Line` API keeping the chunk buffer so ID packing (6-bit, SSE) and F3 parsing read 16 bytes in-bounds without copies; differential tests vs scalar references | 1.21 s | 3.02 s | yes |
| 5 | Parquet: no dictionary, no statistics, Int32 counts, rows split into equal 256K-row files written by the work-stealing pool; person stage: assists in a separate u32 campaign x day array (112 KB, L2-resident), credited counters 16 B, single pass for gap+revenue, skip category argmax without purchases | 1.10 s | 2.76 s | yes |
| 6 | Compact records: events 40 -> 32 B (ts u32 relative to window start, u32::MAX = invalid; purchases as handles into per-thread price/category side tables), person-bucket events 24 -> 16 B, clicks 32 -> 24 B | 1.14 s | 2.82 s | yes (A/B native interleaved, 7 runs: final 2588 -> 2539 ms, practice 1048 -> 888 ms median; docker runs noisy) |
| 7 | zero-initialized union-find arrays (parent = index+1, group min = max of !key) | - | links uf 85 -> 72 ms | yes |
| 8 | dedup table as one array of (key, idx) 16 B slots | - | dedup 510 -> 590 ms | no |
| 9 | dedup table with 8 B slots (32-bit fingerprint + idx) | - | no change | no |
| 10 | L3-sized bitmap filter in front of the person table (skip DRAM lookup for IDs that need no remap; 61% of lookups hit linked IDs) | - | dedup 510 -> 610 ms (unpredictable branch; prefetch already hides latency) | no |
| 11 | person stage: counting sort by person + insertion/pdq sort per person instead of LSD radix | - | sessions 440 -> 480 ms | no |
| 12 | software write-combining (2 records per bucket) + non-temporal 64 B stores into aligned event blocks | - | parse 985 -> 1035 ms, dedup slower (reads now miss cache) | no |
| 13 | partitioned join for ID -> person (route by user ID, links emit partitioned remaps, per-bucket resolve pass with cache-resident tables) | +2% | -2% | no (mixed; more code/memory; resolve pass costs ~220 ms at final vs ~270 ms of lookups saved) |

Phase 3 stopped: seven consecutive ideas (8-13 plus prefetch distance) each under 2%.

Notes from experiments:
- split() was 138 ns/line (79% of lines contain a quote); new scanner 45 ns/line, full field parse ~105 ns/line (P-core).
- Person lookup in the global table cost ~500 ms of dedup at final (128 MB table, random DRAM); prefetch 16 ahead cut it to ~250 ms.
- mimalloc large OS pages / eager commit: no gain.
- Old person stage at final: per-person comparison sorts ~2.0 thread-s, assists loop ~1.1 thread-s.
- THP is active by default (mimalloc madvise): disabling it (prctl) made final 0.6 s slower.
- Over-reserved bucket Vecs (2x estimate) made THP zero ~6 GB instead of 2.5 GB (sys 3.1 s); no reserve: sys 1.6 s but realloc copies; block lists: best of both.
- pread vs mmap: same speed; pread kept (no page-table teardown, no RSS from file pages).
- Bucket count 512/1024/2048: within noise (laptop noise ~5%: background VM). Kept 1024.
- Dedup at final: hash pass ~27 ns/record, emit (random person lookup + scatter) ~48 ns/record.
- Radix digit width in person stage: 11 bits best (8: +70 ms, 10: +35 ms at final).
- Final has 10.0M sessions, 58.8M surviving events, 2.0M persons, 6.6M user rows, 104M assist pairs.
- Regression caught by fixtures: zero user rows must still produce one (empty) parquet file.
- Limitation: window (+7 days) must be < 2^32 ms (49 days) for the u32 relative timestamps; asserted at startup
  (the generator always uses 14 days). Threads are clamped to 62 (purchase handles use 6 bits of thread id).
- Laptop timing noise is large (thermal + background VM): docker final varied 2.70-2.97 s for one binary.
  `scripts/ab.sh BIN_A BIN_B DATASET RUNS` interleaves two native binaries; used for keep/revert decisions.
- Docker no-op container with scorer flags: ~0.25 s fixed overhead on this laptop.
- In-pipeline cost of the 1024-way event scatter at final: ~200 ms of 1020 ms parse (0.4 s sys page faults).
- Remaining random person lookups cost ~230 ms at final (dedup ~190, clicks ~40), hidden mostly by prefetch.
- Prefetch distance 8/16/32/64 for person lookups: within noise; using 32. purge_delay=-1: ~250 ms gain at final (less page faulting).

## Phase 5 log (native, laptop; wall = whole process incl. exit, interleaved runs)
| # | change | effect | kept |
|---|---|---|---|
| 14 | Parquet zstd(1), dictionary on day/sessions/events/top_category, plain user_id/revenue; one user file per thread | output practice 80 MB -> 9.4 MB, final 155 MB -> 28 MB; write +25 ms practice, ~0 at final (encoding-bound, not compression-bound) | yes |
| 15 | posix_fadvise(WILLNEED) on all inputs from a background thread at start | neutral warm and cold on local NVMe; insurance for slow cold storage | yes |
| 16 | F1/F2 timestamps: read-only date table (window -9 d .. +7 d, filled from the manifest) + SWAR HH:MM:SS; full parser as fallback | F1 16.9 -> 8.1 ns, mixed ts 17.3 -> 11.4 ns (P-core, L2-resident) | yes |
| 17 | CSV scan as two passes: flatten comma/newline positions per 64 B block, then walk lines from the list (no serial line-to-line dependency) | 30 -> 22 ns/line on P-cores (E-cores 36 -> 45) | yes |
| 18 | line-at-a-time scanner with one 96 B window per line | no gain (serial newline chain ~15 ns/line) | no |
| 19 | person lookup moved into the parse loop, prefetched and resolved in batches of 32 lines | dedup 950 -> 540 ms CPU, parse +350 ms; net ~-40 ms wall practice | yes |
| 20 | dedup hash: prefetch 16 records ahead, uninitialized key slots | dedup-hash 390 -> 350 ms CPU | yes |
| 21 | valid copies straight to person buckets + event-id records with back-references, dedup only kills losers | 2048 write streams in parse: practice 836 -> 933 ms | no |
| 22 | BlockVec as raw blocks with a write cursor (push = compare + store) | ~4% practice and final | yes |
| 23 | campaign/product lookups via the SIMD packed key | small, within noise | yes |
| 24 | mmap inputs (plain, MADV_POPULATE_READ, per-chunk MADV_DONTNEED) | -150 ms sys CPU in-process, but file-page teardown at exit (or per chunk) costs the same: wall equal to pread | no |

Result (native wall, interleaved, median): practice 834 -> 779-803 ms, final 2421 -> 2377 ms (min 2413 -> 2297);
total CPU practice 5.95 -> 5.35 s. Docker (laptop, incl. ~0.4 s container overhead): practice ~1.11 s, final ~2.7 s.
Notes: in-process "total" hides exit cost (mmap teardown ~60 ms at practice); compare whole-process wall.

## Phase 6 (2026-11-21): switch to C++
Requested: take the C++ entry mliao0128/fraun-shopping-sessions (MIT, LICENSE kept) as the base and improve it.
Its unmodified code passed every check here, but used 8.6 s CPU at practice (ours 5.35 s): 16-byte keys
and records, a serial union-find, std::sort per person, Arrow writer, and ~0.1 s of exit teardown.
Rebuilt it around the Rust design (ported module by module: parse.hpp/.cpp, links.cpp, person.cpp,
main.cpp, then our own parquet.cpp replacing Arrow). Native whole-process wall, interleaved medians:

| # | change | practice | final | kept |
|---|---|---|---|---|
| 0 | reference C++ as is (+ zstd output, _exit) | 1561 ms | 4.7 s | - |
| 1 | C++ port of the Rust pipeline (glibc malloc) | 1046 ms | | yes |
| 2 | GLIBC_TUNABLES=glibc.malloc.hugetlb=1 (THP for malloc; links sys 335 -> 83 ms) | 949 ms | | yes |
| 3 | pack16 via pshufb nibble lookups (GCC rebuilt ~10 broadcast constants per call): keys 13.2 -> 7.1 ns per 2 | 912 ms | | yes |
| 4 | always_inline on the hot parse path; clang-16 build; mimalloc (plain / large OS pages) | 916 / 891 / 943 / 929 ms | | no |
| 5 | huge-page link tables, uninitialized radix and scan buffers | ~equal | | yes (fewer faults) |
| 6 | own Parquet writer (PLAIN pages + zstd, Thrift compact metadata), static binary, no Arrow | 836 ms | | yes |
| 7 | session scratch presized for the largest bucket on huge pages (faults 27K -> 7.5K) | 835 ms | 2373 ms | yes |

Rust on the same runs: 797 ms / 2338 ms. Docker (laptop): practice 1.88 s (reference image) -> 1.14 s,
final 4.85 s -> 2.66 s. Tests: `tests/test.cpp` (build with `g++ -std=c++20 -O2 -march=x86-64-v3
-fsanitize=address,undefined -Isrc tests/test.cpp src/parse.cpp`), plus `scripts/verify.sh`.
Local native build without zstd headers: `ZSTD_CFLAGS=-I<zstd lib dir> ZSTD_LIBS=-l:libzstd.so.1 ./build.sh`.

