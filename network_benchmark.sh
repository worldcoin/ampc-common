#!/usr/bin/env bash
#
# network_benchmark.sh — sweep the MPC (raw TCP) and gRPC networking stacks across the
# parameters that drive per-round latency, and print the relationship.
#
# What it varies (everything else held at the production-faithful ratio):
#   * network delay   — netem on `lo`. 0 (loopback) → WAN-ish. Shows the RELATIVE
#                       gap collapse as RTT dominates, while the ABSOLUTE per-round
#                       delta (the number that compounds over 30k rounds) grows.
#   * distribution    — fixed 32B  = pure h2 hop+framing tax (windows never engage)
#                       bimodal     = 32B + 16KB bursts = framing + flow-control cost
#
# Held fixed (see the earlier analysis): connections=1, sessions=21 → one fat
# stream / one socket, ~21 sessions coalesced, matching prod's 1000/48 ratio at
# a core-proportional scale. Workers = cores per party (no oversubscription).
#
# Loss is OFF by default so the delay sweep is clean; enable it separately
# (LOSS=0.05%) to measure the retransmit tail in isolation.
#
# Requires: sudo (for `tc`), taskset, a >=9-core box. Writes raw per-rep numbers
# to a CSV and prints a median summary table.
#
# Usage:
#   ./grp_vs_tcp.sh                       # full sweep, defaults below
#   REPS=5 DELAYS="0 500us 2ms" ./grp_vs_tcp.sh
#   LOSS=0.05% DELAYS="500us" ./grp_vs_tcp.sh
#   SKIP_BUILD=1 ./grp_vs_tcp.sh

#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$ROOT"

# ── Tunables ─────────────────────────────────────────────────────────────────
SESSIONS=${SESSIONS:-21}
CONNECTIONS=${CONNECTIONS:-1}
ROUNDS=${ROUNDS:-2000}
WORKERS=${WORKERS:-3}
REPS=${REPS:-3}
PAYLOAD=${PAYLOAD:-32}
LARGE_FRAC=${LARGE_FRAC:-0.5}

# Swept payload sizes for the large message in bimodal distribution
LARGE_SIZES=( "16384" "65536" "131072" "160768") # 16KB, 64KB, 128KB, 157KB
DELAYS=${DELAYS:-"0 250us 1ms"}
LOSS=${LOSS:-} # 0.05%}
JITTER=${JITTER:-100us}

CORES=("0-2" "3-5" "6-8")

MPC_BIN="$ROOT/target/release/examples/mpc_node"
GRPC_BIN="$ROOT/target/release/examples/grpc_node"
LOG_FILE="$ROOT/benchmark_raw_output_no_loss.log"

# Clear or start new log file
echo "=== Benchmark Run: $(date -Iseconds) ===" | tee "$LOG_FILE"

# ── netem management ─────────────────────────────────────────────────────────
teardown_netem() { sudo tc qdisc del dev lo root 2>/dev/null || true; }

setup_netem() {
  local delay=$1
  teardown_netem
  [[ "$delay" == "0" ]] && return 0
  local spec="delay $delay $JITTER distribution normal"
  if [[ -n "$LOSS" && "$LOSS" != "0" ]]; then
    spec="$spec loss $LOSS"
  fi
  sudo tc qdisc add dev lo root netem $spec
}

trap 'teardown_netem' EXIT

# ── Build ────────────────────────────────────────────────────────────────────
if [[ "${SKIP_BUILD:-0}" != "1" ]]; then
  echo "Building binaries..."
  cargo build --release --example mpc_node
  cargo build --release --features grpc --example grpc_node
fi

# ── Run Benchmark Function ───────────────────────────────────────────────────
run_suite() {
  local stack_name=$1
  local bin=$2
  local dist_name=$3
  shift 3
  local dist_args=("$@")

  for delay in $DELAYS; do
    setup_netem "$delay"
    
    for rep in $(seq 1 "$REPS"); do
      echo -e "\n============================================================" | tee -a "$LOG_FILE"
      echo "STACK: $stack_name | DIST: $dist_name | DELAY: $delay | LOSS: ${LOSS:-0} | REP: $rep/$REPS" | tee -a "$LOG_FILE"
      echo "============================================================" | tee -a "$LOG_FILE"

      # Launch all 3 parties in background
      local pids=()
      for p in 0 1 2; do
        echo "--- Launching Party $p ---" | tee -a "$LOG_FILE"
        taskset -c "${CORES[$p]}" "$bin" \
          --party "$p" --workers "$WORKERS" \
          --sessions "$SESSIONS" --connections "$CONNECTIONS" \
          --rounds "$ROUNDS" "${dist_args[@]}" 2>&1 | tee -a "$LOG_FILE" &
        pids+=($!)
      done

      # Wait for parties to finish
      for pid in "${pids[@]}"; do
        wait "$pid"
      done
      
      sleep 1
    done
  done
}

# ── Executions ───────────────────────────────────────────────────────────────

# Fixed 32B Runs (Uncomment if needed)
# run_suite "MPC"  "$MPC_BIN"  "fixed32" --dist fixed --payload "$PAYLOAD"
# run_suite "gRPC" "$GRPC_BIN" "fixed32" --dist fixed --payload "$PAYLOAD"

# Bimodal Runs across 16K, 64K, and 128K
for large_bytes in "${LARGE_SIZES[@]}"; do
  # Calculate human-readable label (e.g., 16k, 64k, 128k)
  size_label="$((large_bytes / 1024))k"
  dist_label="bimodal-${size_label}"

  echo -e "\n============================================================" | tee -a "$LOG_FILE"
  echo "STARTING SWEEP FOR LARGE PAYLOAD SIZE: ${size_label} (${large_bytes} Bytes)" | tee -a "$LOG_FILE"
  echo "============================================================" | tee -a "$LOG_FILE"

  # Run MPC Stack
  run_suite "MPC" "$MPC_BIN" "$dist_label" \
    --dist bimodal --payload "$PAYLOAD" --large "$large_bytes" --large-frac "$LARGE_FRAC"

  # Run gRPC Stack
  run_suite "gRPC" "$GRPC_BIN" "$dist_label" \
    --dist bimodal --payload "$PAYLOAD" --large "$large_bytes" --large-frac "$LARGE_FRAC"
done

echo -e "\nDone! Raw output saved to: $LOG_FILE"
