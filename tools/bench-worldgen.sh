#!/usr/bin/env bash
# bench-worldgen.sh — T04 performance baseline runner (manual harness).
#
# Reproduces every number in docs/benchmarks.md. Requires a checkout of
# hytale-server-rust next to this repo (WORLDGEN_ROOT, default
# ../hytale-server-rust). The measurements use the committed golden world
# `testdata/Default` — NOT a multi-gigabyte asset pack — so the run is
# portable, but numbers from different world folders must not be compared.
#
# Usage:
#   tools/bench-worldgen.sh [runs] [seed]
# Environment overrides: WORLDGEN_ROOT.
#
# Not wired into CI on purpose: a full run takes ~10 minutes of machine time.
# To append to the regression table in docs/benchmarks.md, run this and paste
# the output block.

set -euo pipefail

RUNS="${1:-5}"
SEED="${2:-0}"
WORLDGEN_ROOT="${WORLDGEN_ROOT:-$(cd "$(dirname "$0")/.." && pwd)/../hytale-server-rust}"

if [ ! -d "$WORLDGEN_ROOT/crates/worldgen-cli" ]; then
    echo "error: WORLDGEN_ROOT ($WORLDGEN_ROOT) is not a hytale-server-rust checkout" >&2
    exit 1
fi

cd "$WORLDGEN_ROOT"

echo "# bench-worldgen.sh runs=$RUNS seed=$SEED"
echo "# date: $(date -Is)"
echo "# hw: $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 | xargs), $(nproc) threads, $(grep MemTotal /proc/meminfo | awk '{print $2, $3}')"
echo "# rustc: $(rustc --version)"
echo "# profile: release; world: testdata/Default"

echo "# ---- release build ----"
cargo build --release -p worldgen-cli -p worldgen-hytale

echo "# ---- column-query + chunk-path probe (median/p95 over $RUNS runs) ----"
./target/release/query_bench testdata/Default "$SEED" "$RUNS" 1

echo "# ---- chunk-grid throughput, default worker pool ----"
./target/release/worldgen-cli bench --world testdata/Default --seed "$SEED" --start 0,0 --chunks 576

echo "# ---- chunk-grid throughput, single worker ----"
./target/release/worldgen-cli bench --world testdata/Default --seed "$SEED" --start 0,0 --chunks 576 --workers 1

echo "# ---- acceptance gate (T04 tests section) ----"
./target/release/worldgen-cli bench --world testdata/Default --seed "$SEED" --chunks 144

echo "# ---- criterion microbenchmarks (optional, slow) ----"
echo "# run manually with: cargo bench -p worldgen-hytale"
