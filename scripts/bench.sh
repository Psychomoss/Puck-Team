#!/bin/bash
# Builds the image, runs it like the scorer (CPUS=8 MEM=24g) 3x on practice and final,
# prints median wall times and the stage timings of the last run, then runs verify.sh.
#   SETS="dev practice final"  RUNS=3  NOVERIFY=1
cd "$(dirname "$0")/.."
export CPUS=8 MEM=24g
make build >/dev/null 2>&1 || { echo "build failed"; exit 1; }
for d in ${SETS:-practice final}; do
  times=()
  for i in $(seq ${RUNS:-3}); do
    rm -rf out_b
    a=$(date +%s.%N)
    scripts/docker_run.sh datasets/$d out_b 2>/tmp/bench_stages.txt || { echo "run failed"; cat /tmp/bench_stages.txt; exit 1; }
    b=$(date +%s.%N)
    times+=("$(echo "$b - $a" | bc)")
  done
  med=$(printf '%s\n' "${times[@]}" | sort -n | awk '{a[NR]=$1} END {print a[int((NR+1)/2)]}')
  echo "$d: median ${med}s  runs: ${times[*]}"
  sed 's/^/    /' /tmp/bench_stages.txt
done
rm -rf out_b
[ "${NOVERIFY:-0}" = 1 ] || scripts/verify.sh
