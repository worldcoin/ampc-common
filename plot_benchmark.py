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

    n = len(dists)
    ncols = min(3, n)
    nrows = (n + ncols - 1) // ncols
    fig, axes = plt.subplots(nrows, ncols, figsize=(6 * ncols, 4.5 * nrows),
                             squeeze=False, sharex=True)

    colors = {"MPC": "tab:blue", "gRPC": "tab:orange"}

    for idx, dist in enumerate(dists):
        ax = axes[idx // ncols][idx % ncols]
        for stack in stacks:
            pts = sorted(
                (delay, mean(lats))
                for (d, s, delay), lats in data.items()
                if d == dist and s == stack
            )
            if not pts:
                continue
            xs = [p[0] / 1000.0 for p in pts]  # ms
            ys = [p[1] for p in pts]
            ax.plot(xs, ys, marker="o",
                    color=colors.get(stack), label=stack)
        ax.set_title(dist)
        ax.set_xlabel("network delay (ms, one-way)")
        ax.set_ylabel("per-round latency (µs)")
        ax.grid(True, alpha=0.3)
        ax.legend()

    # hide any unused subplots
    for j in range(n, nrows * ncols):
        axes[j // ncols][j % ncols].axis("off")

    fig.suptitle("MPC vs gRPC: per-round latency vs network delay", fontsize=14)
    fig.tight_layout(rect=[0, 0, 1, 0.97])
    fig.savefig(args.out, dpi=130)
    print(f"wrote {args.out}")


def parse_dist_size(dist):
    m = re.search(r"(\d+)k", dist)
    return int(m.group(1)) if m else 0


if __name__ == "__main__":
    main()
