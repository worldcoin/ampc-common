#!/usr/bin/env python3
"""Parse a benchmark_raw_output log and plot per-round latency vs network delay,
comparing the MPC and gRPC stacks, grouped by distribution (bimodal-X).

Usage:
    python3 plot_benchmark.py [log_file] [-o out.png]
"""
import argparse
import re
import sys
from collections import defaultdict
from statistics import mean

HEADER_RE = re.compile(
    r"STACK:\s*(?P<stack>\S+)\s*\|\s*DIST:\s*(?P<dist>\S+)\s*\|\s*"
    r"DELAY:\s*(?P<delay>\S+)\s*\|\s*LOSS:\s*(?P<loss>\S+)\s*\|\s*REP:\s*(?P<rep>\S+)"
)
LAT_RE = re.compile(r"per-round latency:\s*([0-9.]+)\s*µs")


def delay_to_us(token):
    """Convert a delay token like '0', '250us', '1ms' into microseconds."""
    token = token.strip()
    if token in ("0", "0us", "0ms"):
        return 0.0
    m = re.match(r"([0-9.]+)\s*(us|ms|s)?", token)
    if not m:
        raise ValueError(f"cannot parse delay: {token!r}")
    val = float(m.group(1))
    unit = m.group(2) or "us"
    return val * {"us": 1.0, "ms": 1000.0, "s": 1_000_000.0}[unit]


def parse(path):
    # (dist, stack, delay_us) -> list of per-round latencies (µs)
    data = defaultdict(list)
    cur = None
    with open(path) as f:
        for line in f:
            h = HEADER_RE.search(line)
            if h:
                cur = (h.group("dist"), h.group("stack"), delay_to_us(h.group("delay")))
                continue
            if cur is None:
                continue
            m = LAT_RE.search(line)
            if m:
                data[cur].append(float(m.group(1)))
    return data


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("log", nargs="?", default="benchmark_raw_output_with_loss.log")
    ap.add_argument("-o", "--out", default="benchmark_latency.png")
    args = ap.parse_args()

    data = parse(args.log)
    if not data:
        sys.exit("No data parsed — check the log format.")

    import matplotlib
    matplotlib.use("Agg")
    import matplotlib.pyplot as plt

    dists = sorted({k[0] for k in data}, key=lambda d: parse_dist_size(d))
    stacks = sorted({k[1] for k in data})

    # (row label, y-axis label, reducer over the per-config latency samples)
    metrics = [
        ("average", "avg per-round latency (µs)", mean),
        ("tail (p95)", "p95 per-round latency (µs)", percentile95),
    ]

    ncols = len(dists)
    nrows = len(metrics)
    fig, axes = plt.subplots(nrows, ncols, figsize=(6 * ncols, 4.5 * nrows),
                             squeeze=False, sharex=True)

    colors = {"MPC": "tab:blue", "gRPC": "tab:orange"}

    for row, (metric_name, ylabel, reducer) in enumerate(metrics):
        for col, dist in enumerate(dists):
            ax = axes[row][col]
            for stack in stacks:
                pts = sorted(
                    (delay, reducer(lats))
                    for (d, s, delay), lats in data.items()
                    if d == dist and s == stack
                )
                if not pts:
                    continue
                xs = [p[0] / 1000.0 for p in pts]  # ms
                ys = [p[1] for p in pts]
                ax.plot(xs, ys, marker="o",
                        color=colors.get(stack), label=stack)
            if row == 0:
                ax.set_title(dist)
            if row == nrows - 1:
                ax.set_xlabel("network delay (ms, one-way)")
            if col == 0:
                ax.set_ylabel(f"{metric_name}\n{ylabel}")
            ax.grid(True, alpha=0.3)
            ax.legend()

    fig.suptitle("MPC vs gRPC: per-round latency vs network delay", fontsize=14)
    fig.tight_layout(rect=[0, 0, 1, 0.97])
    fig.savefig(args.out, dpi=130)
    print(f"wrote {args.out}")


def percentile95(values):
    """p95 via nearest-rank; falls back gracefully for tiny samples."""
    if not values:
        raise ValueError("empty sample")
    s = sorted(values)
    rank = max(1, int(round(0.95 * len(s))))
    return s[rank - 1]


def parse_dist_size(dist):
    m = re.search(r"(\d+)k", dist)
    return int(m.group(1)) if m else 0


if __name__ == "__main__":
    main()
