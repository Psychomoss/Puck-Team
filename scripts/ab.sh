#!/bin/bash
# Interleaved A/B timing of two native binaries: ab.sh BIN_A BIN_B [DATASET] [RUNS]
# Prints each binary's median and min wall time (ms) from the pipeline's own "total" line.
cd "$(dirname "$0")/.."
A=$1; B=$2; D=${3:-final}; N=${4:-7}
ta=(); tb=()
for i in $(seq $N); do
  for x in A B; do
    bin=$A; [ $x = B ] && bin=$B
    rm -rf out_ab
    t=$(DATA_DIR=datasets/$D OUTPUT_DIR=out_ab $bin 2>&1 | awk '/total/ {print $2}')
    if [ $x = A ]; then ta+=($t); else tb+=($t); fi
  done
done
rm -rf out_ab
stat() { printf '%s\n' "$@" | sort -n | awk '{a[NR]=$1} END {printf "median %7.1f  min %7.1f", a[int((NR+1)/2)], a[1]}'; }
echo "A: $(stat "${ta[@]}")  ($A)"
echo "B: $(stat "${tb[@]}")  ($B)"
