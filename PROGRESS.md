# PROGRESS

Team puck, Shopping Sessions, Rust. Read this and `git log` first when resuming.

## Status
- Phase 0 (setup): done.
- Phase 1 (correct Rust pipeline): done. Gate 1 passed.
- Phase 2 (Docker): done. Multi-stage (rust:1.95-slim-bookworm -> debian:bookworm-slim, needs `chmod` for
  docker_run.sh cleanup), mimalloc, fat LTO, x86-64-v3. Docker: fixtures 36/36, dev MATCH (0.53 s wall incl. container start).
- Next: Phase 3 (performance).

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

## Timings (native, laptop, 8 threads, v1)
| dataset | total |
|---|---|
| dev | 0.27 s |
| practice | 3.9 s |
| final | 9.8 s (maxrss 12.5 GB) |
