#!/usr/bin/env bash
#
# network_flamegraph.sh — profile ONE test case of the MPC (raw TCP) and gRPC
# networking stacks with cargo flamegraph, producing one SVG per stack.
#
# Unlike network_benchmark.sh (which sweeps delay × distribution × reps), this
# runs a single fixed point:
#   * zero added RTT   — no netem on `lo` (pure loopback)
#   * one distribution — DIST (default: bimodal 32B / 16KB)
#
# How it profiles a 3-party protocol: parties 1 and 2 are launched raw in the
# background as load generators; party 0 is run *under* `flamegraph` (perf) and
# is the one we sample. When party 0 exits, its collapsed stacks are rendered to
# an SVG. We do this once for the MPC binary and once for the gRPC binary.
#
# The release profile already carries `debug = 1` (see Cargo.toml), so frames
# resolve to real symbol names.
#
# Requires: cargo-flamegraph (`cargo install flamegraph`), perf, taskset, sudo,
# a >=9-core box.
#
# Usage:
#   ./network_flamegraph.sh
#   ROUNDS=5000 DIST=bimodal LARGE=65536 ./network_flamegraph.sh
#   DIST=fixed ./network_flamegraph.sh
#   SKIP_BUILD=1 ./network_flamegraph.sh

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$ROOT"

# ── Tunables ─────────────────────────────────────────────────────────────────
SESSIONS=${SESSIONS:-21}
CONNECTIONS=${CONNECTIONS:-1}
ROUNDS=${ROUNDS:-5000}       # more rounds than the bench: flamegraphs want samples
WORKERS=${WORKERS:-3}
PAYLOAD=${PAYLOAD:-32}
LARGE_FRAC=${LARGE_FRAC:-0.5}

# The single distribution to profile.
DIST=${DIST:-bimodal}        # bimodal | fixed
LARGE=${LARGE:-65536}        # large-message size for bimodal (16KB default)

FREQ=${FREQ:-997}            # perf sampling frequency (Hz)
OUT_DIR=${OUT_DIR:-"$ROOT/flamegraphs"}

CORES=("0-2" "3-5" "6-8")

MPC_BIN="$ROOT/target/release/examples/mpc_node"
GRPC_BIN="$ROOT/target/release/examples/grpc_node"

mkdir -p "$OUT_DIR"

# Build the per-stack distribution args once.
if [[ "$DIST" == "fixed" ]]; then
  DIST_ARGS=(--dist fixed --payload "$PAYLOAD")
  DIST_LABEL="fixed-${PAYLOAD}b"
else
  DIST_ARGS=(--dist bimodal --payload "$PAYLOAD" --large "$LARGE" --large-frac "$LARGE_FRAC")
  DIST_LABEL="bimodal-$((LARGE / 1024))k"
fi

# ── perf permissions ─────────────────────────────────────────────────────────
# flamegraph shells out to `perf record`; on a stock box perf_event_paranoid=2
# blocks unprivileged sampling. Relax it for the run and restore on exit.
ORIG_PARANOID="$(cat /proc/sys/kernel/perf_event_paranoid 2>/dev/null || echo 2)"
ORIG_KPTR="$(cat /proc/sys/kernel/kptr_restrict 2>/dev/null || echo 1)"

relax_perf() {
  sudo sysctl -q kernel.perf_event_paranoid=-1 || true
  sudo sysctl -q kernel.kptr_restrict=0 || true
}
restore_perf() {
  sudo sysctl -q kernel.perf_event_paranoid="$ORIG_PARANOID" || true
  sudo sysctl -q kernel.kptr_restrict="$ORIG_KPTR" || true
}
trap 'restore_perf' EXIT

# ── Build ────────────────────────────────────────────────────────────────────
if [[ "${SKIP_BUILD:-0}" != "1" ]]; then
  echo "Building binaries..."
  cargo build --release --example mpc_node
  cargo build --release --features grpc --example grpc_node
fi

# ── Profile one stack ────────────────────────────────────────────────────────
# Launches parties 1 & 2 as raw background load, runs party 0 under flamegraph.
profile_stack() {
  local stack_name=$1
  local bin=$2
  local out_svg="$OUT_DIR/flamegraph_${stack_name}_${DIST_LABEL}.svg"

  echo -e "\n============================================================"
  echo "FLAMEGRAPH: $stack_name | DIST: $DIST_LABEL | ROUNDS: $ROUNDS"
  echo "  -> $out_svg"
  echo "============================================================"

  # Background load generators (parties 1 and 2).
  local pids=()
  for p in 1 2; do
    echo "--- Launching load party $p ---"
    taskset -c "${CORES[$p]}" "$bin" \
      --party "$p" --workers "$WORKERS" \
      --sessions "$SESSIONS" --connections "$CONNECTIONS" \
      --rounds "$ROUNDS" "${DIST_ARGS[@]}" >/dev/null 2>&1 &
    pids+=($!)
  done

  # Profiled party 0. flamegraph wraps perf around the whole command (taskset
  # execs the binary in place, so the samples land on mpc_node/grpc_node).
  echo "--- Profiling party 0 ---"
  flamegraph --freq "$FREQ" --output "$out_svg" -- \
    taskset -c "${CORES[0]}" "$bin" \
      --party 0 --workers "$WORKERS" \
      --sessions "$SESSIONS" --connections "$CONNECTIONS" \
      --rounds "$ROUNDS" "${DIST_ARGS[@]}"

  # Reap the load generators.
  for pid in "${pids[@]}"; do
    wait "$pid" || true
  done
  sleep 1
}

# ── Executions ───────────────────────────────────────────────────────────────
relax_perf

profile_stack "MPC"  "$MPC_BIN"
profile_stack "gRPC" "$GRPC_BIN"

echo -e "\nDone! Flamegraphs written to: $OUT_DIR"
ls -1 "$OUT_DIR"/*.svg
