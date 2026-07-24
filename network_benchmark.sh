#!/usr/bin/env bash
#
# grp_vs_tcp.sh — sweep the MPC (raw TCP) and gRPC networking stacks across the
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

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$ROOT"

# ── Tunables (override via env) ──────────────────────────────────────────────
SESSIONS=${SESSIONS:-21}          # 21 sessions on 1 connection ≈ prod's 1000/48
CONNECTIONS=${CONNECTIONS:-1}
ROUNDS=${ROUNDS:-2000}            # rounds per rep (kept short; latency is per-round)
WORKERS=${WORKERS:-3}             # ≈ cores per party — do NOT oversubscribe
REPS=${REPS:-3}                   # runs per config; table reports the median
PAYLOAD=${PAYLOAD:-32}
LARGE=${LARGE:-16384}
LARGE_FRAC=${LARGE_FRAC:-0.5}
PROJECT_ROUNDS=${PROJECT_ROUNDS:-30000}   # project per-round delta to a real search
SETTLE=${SETTLE:-2}               # seconds between runs (let ports leave TIME_WAIT)

# netem delay values to sweep. "0" == loopback (no qdisc).
DELAYS=${DELAYS:-"0 250us 500us 1ms 2ms"}
# Packet loss, e.g. "0.05%". Empty/"0" disables. Keep off for a clean delay sweep.
LOSS=${LOSS:-}
# Jitter (normal-distributed) added to each non-zero delay.
JITTER=${JITTER:-100us}

# Core pinning: one disjoint set per party. Adjust for your box (needs >=9 cores).
CORES=("0-2" "3-5" "6-8")

# Distributions to test: "label|extra CLI args"
DISTS=(
  "fixed32|--dist fixed --payload ${PAYLOAD}"
  "bimodal|--dist bimodal --payload ${PAYLOAD} --large ${LARGE} --large-frac ${LARGE_FRAC}"
)

MPC_BIN="$ROOT/target/release/examples/mpc_node"
GRPC_BIN="$ROOT/target/release/examples/grpc_node"
CSV="$ROOT/grpc_vs_tcp_results.csv"

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
  # shellcheck disable=SC2086
  sudo tc qdisc add dev lo root netem $spec
}

trap 'teardown_netem' EXIT

# ── run one config: launch 3 pinned parties, return the max per-round latency ─
# (max across parties == the critical-path party; they should agree closely.)
run_parties() {
  local bin=$1; shift
  local dist_args=("$@")
  local outdir; outdir=$(mktemp -d)
  local pids=()
  local p
  for p in 0 1 2; do
    taskset -c "${CORES[$p]}" "$bin" \
      --party "$p" --workers "$WORKERS" \
      --sessions "$SESSIONS" --connections "$CONNECTIONS" \
      --rounds "$ROUNDS" "${dist_args[@]}" \
      >"$outdir/p$p.out" 2>"$outdir/p$p.err" &
    pids+=($!)
  done
  local ok=1 pid
  for pid in "${pids[@]}"; do wait "$pid" || ok=0; done
  if [[ $ok -ne 0 ]]; then
    echo "  !! run failed ($bin):" >&2
    cat "$outdir"/p*.err >&2
    rm -rf "$outdir"
    return 1
  fi
  # Each party prints: "per-round latency: <float> µs (...)". Take the slowest.
  local lat
  lat=$(grep -h "per-round latency" "$outdir"/p*.out \
        | grep -oE "[0-9]+\.[0-9]+" | sort -rn | head -1)
  rm -rf "$outdir"
  [[ -z "$lat" ]] && return 1
  echo "$lat"
}

# median of the numeric args
median() {
  printf '%s\n' "$@" | sort -n | awk '
    { a[NR]=$1 }
    END { if (NR%2) print a[(NR+1)/2]; else printf "%.2f\n",(a[NR/2]+a[NR/2+1])/2 }'
}

# ── build ────────────────────────────────────────────────────────────────────
if [[ "${SKIP_BUILD:-0}" != "1" ]]; then
  echo "building examples (set SKIP_BUILD=1 to skip)…"
  cargo build --release --example mpc_node
  cargo build --release --features grpc --example grpc_node
fi
[[ -x "$MPC_BIN"  ]] || { echo "missing $MPC_BIN"  >&2; exit 1; }
[[ -x "$GRPC_BIN" ]] || { echo "missing $GRPC_BIN" >&2; exit 1; }

# ── sweep ────────────────────────────────────────────────────────────────────
echo "stack,delay,loss,dist,rep,per_round_us" > "$CSV"

printf '\n%-8s %-9s %-10s %-10s %-7s %-9s %-14s\n' \
  DELAY DIST "MPC_µs" "gRPC_µs" "RATIO" "GAP_µs" "Δ@${PROJECT_ROUNDS}(s)"
printf '%s\n' "────────────────────────────────────────────────────────────────────────────"

for delay in $DELAYS; do
  setup_netem "$delay"
  for entry in "${DISTS[@]}"; do
    label="${entry%%|*}"
    IFS=' ' read -r -a dargs <<< "${entry#*|}"

    mpc_vals=(); grpc_vals=()
    for r in $(seq 1 "$REPS"); do
      if v=$(run_parties "$MPC_BIN"  "${dargs[@]}"); then
        mpc_vals+=("$v"); echo "mpc,$delay,${LOSS:-0},$label,$r,$v" >> "$CSV"
      fi
      sleep "$SETTLE"
      if v=$(run_parties "$GRPC_BIN" "${dargs[@]}"); then
        grpc_vals+=("$v"); echo "grpc,$delay,${LOSS:-0},$label,$r,$v" >> "$CSV"
      fi
      sleep "$SETTLE"
    done

    if [[ ${#mpc_vals[@]} -eq 0 || ${#grpc_vals[@]} -eq 0 ]]; then
      printf '%-8s %-9s %s\n' "$delay" "$label" "(no successful runs)"
      continue
    fi

    mpc_med=$(median "${mpc_vals[@]}")
    grpc_med=$(median "${grpc_vals[@]}")
    read -r ratio gap proj < <(awk -v m="$mpc_med" -v g="$grpc_med" -v n="$PROJECT_ROUNDS" \
      'BEGIN { printf "%.2f %.1f %.2f", g/m, g-m, (g-m)*n/1e6 }')
    printf '%-8s %-9s %-10s %-10s %-7s %-9s %-14s\n' \
      "$delay" "$label" "$mpc_med" "$grpc_med" "${ratio}x" "$gap" "$proj"
  done
done

echo
echo "raw per-rep numbers → $CSV"
echo "RATIO shrinks with delay (fixed hop tax / growing RTT); GAP and Δ@${PROJECT_ROUNDS}"
echo "are the absolute per-search penalty that compounds over the dependent chain."
