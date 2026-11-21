#!/bin/sh
# Builds the pipeline: C++20, one static binary, zstd for the Parquet pages.
#   ZSTD_CFLAGS / ZSTD_LIBS override where zstd comes from (default: system headers, static lib).
set -eu
cd "$(dirname "$0")"
mkdir -p build
${CXX:-g++} -std=c++20 -O3 -DNDEBUG -march=x86-64-v3 -flto=auto -pthread ${EXTRA_FLAGS:-} ${ZSTD_CFLAGS:-} \
  src/main.cpp src/parse.cpp src/links.cpp src/person.cpp src/parquet.cpp \
  ${ZSTD_LIBS:--static -l:libzstd.a} -o "${1:-build/sessions}"
