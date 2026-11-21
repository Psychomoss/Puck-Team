#!/bin/bash
# Runs every correctness check and prints PASS/FAIL.
#   DOCKER=1  run the image via scripts/docker_run.sh (scorer flags) instead of the native binary
#   QUICK=1   skip the 1-thread and reversed-file-order runs
cd "$(dirname "$0")/.."
export PATH="$PWD/.venv/bin:$PATH"
# Native builds use build.sh; set ZSTD_CFLAGS / ZSTD_LIBS if zstd is not installed system-wide.
fail=0
check() { if "$@" >/dev/null; then echo "  ok: $*"; else echo "  FAILED: $*"; fail=1; fi; }
BIN=build/sessions
if [ "${DOCKER:-0}" = 1 ]; then
  run() { rm -rf "$2"; scripts/docker_run.sh "$1" "$2" 2>/dev/null; }
  fixture_cmd=scripts/docker_run.sh
else
  ./build.sh || { echo FAIL; exit 1; }
  run() { rm -rf "$2"; DATA_DIR="$1" OUTPUT_DIR="$2" "$BIN" 2>/dev/null; }
  fixture_cmd=$BIN
fi
golden() {  # golden DATASET OUT
  local got
  got=$(python3 scripts/canon.py "$2" "$2.canon") || return 1
  rm -rf "$2.canon"
  for t in user_daily campaign_daily; do
    want=$(awk -v d="$1" -v t="$t" '$1==d && $2==t {print $3}' scripts/golden.sha256)
    echo "$got" | grep -q "^$want  $t " || { echo "    $1 $t hash mismatch" >&2; return 1; }
  done
}
echo "fixtures"
check python3 scripts/run_fixtures.py fixtures -- $fixture_cmd
echo "dev"
run datasets/dev out_v_dev; check python3 scripts/compare.py out_v_dev dev-answer
for d in practice final; do
  echo "$d"
  run datasets/$d out_v_$d; check golden $d out_v_$d
  if [ "${QUICK:-0}" != 1 ] && [ "${DOCKER:-0}" != 1 ]; then
    rm -rf out_v_$d; NCPU=1 DATA_DIR=datasets/$d OUTPUT_DIR=out_v_$d $BIN 2>/dev/null; check golden $d out_v_$d
    rm -rf out_v_$d; REVERSE_FILES=1 DATA_DIR=datasets/$d OUTPUT_DIR=out_v_$d $BIN 2>/dev/null; check golden $d out_v_$d
  fi
done
rm -rf out_v_dev out_v_practice out_v_final
[ $fail = 0 ] && echo PASS || echo FAIL
exit $fail
