# Task: fastest exactly-correct "Shopping Sessions" pipeline in Rust

You are working autonomously in `/home/mossy/Desktop/Projects/hackaton` (call it ROOT). Work through every phase below without stopping to ask questions; when something is ambiguous, `rules.md` decides, and if `rules.md` is silent, pick the reading that the fixtures and the dev answer confirm, and record the decision in `PROGRESS.md`.

## The competition

Databricks "Shopping Sessions" buildathon track (team **puck**). A Docker image reads messy click-stream CSVs from `/data` and writes two Parquet tables to `/output/user_daily/` and `/output/campaign_daily/`. The score is the container's wall-clock run time (start to exit), lower is better. The answer is exact: a fast result that is even slightly wrong scores zero. So correctness is a hard gate, and speed is the goal.

- `ROOT/rules.md` is the single source of truth. Read all of it before writing code, and re-read the relevant section whenever you touch that logic. If the starter and `rules.md` disagree, `rules.md` wins.
- Practice runs on the website use hidden data of about 20M events; the final run uses about 60M events with a hard 3-minute cap. Every dataset has a different seed, window, whale users and dirt rates, so nothing about a dataset may be hard-coded.
- Current leaderboard: the best entry is about 1.6 s, several are 2 to 4 s, and the PySpark baseline is 172 s. Aim to beat 1.6 s.

### Scorer environment (from the starter README)

- Google Cloud n2 Intel Xeon (Cascade Lake or newer, AVX-512 available), linux/amd64, built natively from your Dockerfile; build time is not timed.
- 8 vCPUs (hyperthreads, so about 4 physical cores), 24 GB RAM, no swap, no network.
- Runs as uid 1000:1000, root filesystem read-only, `/tmp` is a 64 MB tmpfs, `/data` read-only, `/output` and `/scratch` writable (`/scratch` is fast local disk).
- Env: `DATA_DIR=/data OUTPUT_DIR=/output SCRATCH_DIR=/scratch HOME=/scratch NCPU=8 MEM_GB=24`.
- The image's `CMD` runs with no arguments; exit code 0 means done.
- `scripts/docker_run.sh` in the starter reproduces the exact scorer flags.

### This laptop (where you develop and benchmark)

Intel Core Ultra 5 228V (Lunar Lake: 4 P-cores + 4 low-power E-cores, no hyperthreading, **no AVX-512**), 32 GB RAM, plenty of disk, Linux x86_64. Docker with buildx, Rust and Python are installed. Local times will not match the scorer; use them only for relative comparison. Because the E-cores are much slower than the P-cores, use work-stealing parallelism with fine-grained chunks (this also helps on the scorer). Compile for `x86-64-v3` (AVX2), not `target-cpu=native` and not an AVX-512 target, so the same binary runs here and on the scorer; runtime feature detection for AVX-512 is allowed only if it is tested and gives identical results.

## Ground rules

- Keep the git history local: `git init` in ROOT at the start, commit after each step listed below with a clear message (include timings in performance commits). Never add a remote and never push.
- Do not edit `rules.md`, the fixtures, `dev-answer/`, or the semantics of the starter's test scripts.
- Keep code comments minimal: no explanatory comment blocks, only a short note where something truly non-obvious happens.
- Keep `ROOT/PROGRESS.md` up to date after every phase and every optimization attempt: current phase, what passed, current timings, decisions made, what was tried and rejected, and the next step. If your context is compacted or a new session starts, read `PROGRESS.md` and `git log` first and continue from there.
- Large generated datasets and build outputs must never be committed or sent into the Docker build context: put them under `ROOT/datasets/` and list it, `target/`, `out*/` and the zips in `.gitignore` and `.dockerignore`.

## Phase 0: setup

1. `git init` in ROOT and add a `.gitignore`.
2. Get the track files into ROOT. Needed: `starter-duckdb.zip`, `generator.zip`, `dev-answer.zip` (`rules.md` is already in ROOT). First look in ROOT, then in `~/Downloads` (the browser may have saved them there; copy them into ROOT). If any is missing, download it:
   `https://www.joinbuilders.org/buildathon/tracks/shopping-sessions/files/<name>`
   If downloading fails, stop and tell me which file is missing.
3. Unzip them. Use the starter's `scripts/`, `fixtures/` (36 cases plus `fixtures/README.md`), `Makefile` and `README.md` as your test harness at ROOT level (you will replace its Python pipeline and Dockerfile with Rust). Read `fixtures/README.md`, `scripts/run_fixtures.py`, `scripts/compare.py` and `scripts/docker_run.sh` so you know exactly how outputs are checked. Install `polars` for the scripts if missing.
4. Build the generator (`cargo build --release` in `generator/`) and generate three datasets with it:
   - `datasets/dev`: `--seed 1 --events 1000000 --clicks 250000` (this is the public dev set matching `dev-answer/`)
   - `datasets/practice`: `--seed 7 --events 20000000 --clicks 5000000` (similar to the website's practice run)
   - `datasets/final`: `--seed 11 --events 60000000 --clicks 15000000` (similar to the final run)
5. Commit.

## Phase 1: a correct Rust pipeline

Write a Cargo project in ROOT that reads `$DATA_DIR`, writes `$OUTPUT_DIR/user_daily/*.parquet` and `$OUTPUT_DIR/campaign_daily/*.parquet`, and exits 0. Correctness first, but choose data structures you will not have to throw away later (interned IDs, flat arrays, sorting instead of nested maps).

Implement `rules.md` exactly. The traps most likely to cost you the whole score:

- **Your own CSV parser.** Do not use a generic CSV library's dialect. Skip each file's header line; read every `*.csv` in `events/`, `clicks/`, `links/` (some files contain only the header). Only `\n` ends a line; `\r` is ordinary data, so it can make a price or timestamp invalid. Quotes are allowed only around a whole field and are stripped; `""` is empty (null). A wrong field count, a quote anywhere else, or an empty line makes the line malformed. Spaces are part of the value. Digits mean ASCII `0`-`9` only.
- **Timestamps F1/F2/F3** with every invalid form listed in the rules: fractional seconds, lowercase `z`/`t`, a space instead of `T`, no zone, offset without a colon or out of range, impossible dates including February 29 outside leap years, hour 24, second 60, and non-digits in F3. F3 allows leading zeros; an F3 value too large for i64 is not malformed but outside the window. Windows: events `[start, end)`, clicks `[start - 7d, end)`.
- **Event order:** parse, then dedup on `event_id` keeping the smallest `ingest_ts` (dedup happens before validation, and a later valid copy never rescues an invalid kept copy), then validate, then drop late events (`ingest_ts - ts > 24h`; exactly 24 h is kept, negative is kept).
- **Linked IDs:** union-find over all valid link lines, transitive and across all files. A person is named by the byte-wise smallest ID in their group, which may be an ID that never appears in an event. IDs not in any link are their own person. Upper-cased IDs are different IDs. Chains hundreds of links long and shared devices joining thousands of people must be handled efficiently.
- **Sessions per person** (all IDs together) sorted by `ts`: a new session starts when the gap is over 1,800,000 ms; equal timestamps always share a session. The day is the UTC date of the session start, and every event in the session counts on that day.
- **Click credit:** the latest valid click of the same person with `session_start - 7d <= ts <= session_start`; ties in `ts` go to the byte-wise smallest `click_id`. No such click means `organic`/`organic`. **Assists:** each distinct campaign with at least one qualifying click in that window (including the chosen one) assists once; `organic` never assists. Campaign-day rows also exist for campaigns that only assisted, with 0 in the three credited columns.
- **Outputs:** exact column names and no extras; `day` is a Parquet `DATE` (Date32); integers of any width; `top_category` is null without purchases, ties go to the byte-wise smallest category, and `unknown` is used for products missing from `products.csv` and competes like any category. All string comparisons are byte-wise and case-sensitive, with no trimming.

**Gate 1 (all must pass before Phase 2):**

1. All 36 fixtures pass: `python3 scripts/run_fixtures.py fixtures -- <your native binary>` (check how the script passes paths and adapt with a small wrapper if needed).
2. The dev set matches exactly: run on `datasets/dev`, then `python3 scripts/compare.py <out> dev-answer`.
3. On `practice` and `final`: the self-checks from `rules.md` hold (summed `sessions` and `revenue_cents` equal across both tables; summed `assisted_sessions` >= summed credited non-`organic` `sessions`), and the output is identical when run with 1 thread and with all threads, and when the input file list is processed in reverse order.
4. Save **golden outputs** for `practice` and `final`: export each table to a canonical sorted CSV under `datasets/golden/` and record their SHA-256 in `PROGRESS.md`. From now on every change must reproduce these hashes exactly, plus fixtures and dev-answer. Write one script, `scripts/verify.sh`, that runs all of these checks and prints PASS/FAIL.

Commit.

## Phase 2: Docker image

Write a multi-stage Dockerfile: build the release binary in a Rust image, run it from a minimal runtime image (for example `debian:bookworm-slim` or a static binary on `scratch`/distroless). It must work as uid 1000 with a read-only root and no network, write only to `/output` and `/scratch`, and have the binary as `CMD`. Release profile: `opt-level=3`, `lto="fat"`, `codegen-units=1`, `panic="abort"`. Use a fast allocator (mimalloc or jemalloc); if you build a static musl binary, you must use one, because musl's allocator is slow.

Verify through Docker exactly as the scorer runs it: `make build`, `make fixtures`, then `scripts/docker_run.sh datasets/dev out && python3 scripts/compare.py out dev-answer`. Commit.

## Phase 3: make it as fast as possible

Add a benchmark script, `scripts/bench.sh`, that builds the image and runs it via `scripts/docker_run.sh` with `CPUS=8 MEM=24g` on `practice` and `final`, 3 runs each, reports the median wall time, and then runs `scripts/verify.sh`. Also add per-stage timings to stderr (read/parse, links, dedup, sessionize, attribution, aggregate, write) so you know where time goes. Profile with `perf` or flamegraphs.

Then iterate: form a hypothesis, implement it, run `verify.sh` and `bench.sh`, and keep the change only if every check passes and the time improves. Otherwise revert it. Commit every kept improvement with before/after timings, and log every attempt in `PROGRESS.md`. Ideas to evaluate, roughly in order of expected payoff:

- mmap the inputs; parse in parallel across files **and inside large files** (the largest file holds about a fifth of the rows; split at `\n` boundaries) with rayon work-stealing and small chunks.
- A branch-light, hand-written byte parser with a fast path for the common unquoted, well-formed line, falling back to a fully rule-checked slow path. Hand-written timestamp parsing with a days-from-civil calculation. SIMD (memchr or AVX2) for finding delimiters.
- Intern every user ID, event ID, click ID, product and campaign as compact integers with a fast hash (FxHash/ahash/gxhash); do it in parallel with sharded tables, avoiding per-field `String` allocations; compare by bytes only where the rules need byte order (person names, `click_id` ties, category ties).
- Dedup events with a parallel hash or sort on the interned `event_id`; carry only the fields you need, packed in small structs.
- Group events and clicks by person with parallel radix or counting sorts keyed by (person, ts); credit and assists with a two-pointer sweep over each person's sorted clicks; aggregate into dense arrays where possible.
- Write Parquet in parallel (both tables at once, several row groups); try no compression versus Snappy; no fsync.
- Skip freeing large structures at the end (flush the output, then `std::process::exit(0)`), keep the image small, and minimize startup cost.
- Watch memory on `final`: stay well under 24 GB with no swap.

Keep going until improvements flatten out (for example, five consecutive ideas each under 2% gain) and you have nothing promising left to try.

## Phase 4: final check and report

1. Fresh `make build`, then `make fixtures`, then dev compare, then `scripts/verify.sh`, all through Docker with the scorer flags.
2. Write `TEAM.md` (Name: puck, Members: Seyran Umudov, Engine: Rust).
3. Write `RESULTS.md` with a table of median wall times (3 runs, Docker, `CPUS=8 MEM=24g`) for **dev (1M)**, **practice-like (20M, like the website's practice run)** and **final-like (60M)**, the stage breakdown for practice and final, peak memory, the final commit SHA, a short summary of the design, and anything you were unsure about in `rules.md`. Note that these are laptop times and that the scorer has only about 4 physical cores but AVX-512 and faster memory.
4. Final commit. Do not push and do not submit anywhere.
