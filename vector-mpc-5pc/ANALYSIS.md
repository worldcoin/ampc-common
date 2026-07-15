# Communication & cloud-cost analysis

Summary of the measured network traffic for the secure vector-comparison
pipeline (512-dim int4 vectors, inner product + threshold via MSB extraction,
all arithmetic mod 2^16 — the 16-bit variant used by
[`vector-mpc-poc`](https://github.com/worldcoin/vector-mpc-poc)), and what
that traffic costs when the parties run on different cloud providers.

All byte figures come from the harness in this crate, which wraps the real
protocol implementations in a byte-counting transport and verifies every
opened result against the plaintext computation. Both 3PC adder
configurations are reproducible without editing anything:

```bash
cargo run --release -p vector-mpc-5pc -- 4096 2                    # default (prefix adder)
cargo run --release -p vector-mpc-5pc --features ripple -- 4096 2  # byte-optimal (ripple)
```

## At a glance

Full-scan queries; traffic and cost are fleet totals. **Every byte figure
is measured** (counted at the transport, results verified against
plaintext); the cost and latency columns are computed from those bytes with
the pricing/bandwidth model stated below and detailed in sections 2–3.
Vector = the 16-bit pipeline, one comparison per DB item. Iris = the 32-bit
FHD pipeline × 31 rotations per DB item. 3PC cells show *default (prefix
adder) / `ripple` feature*; 5PC has one configuration. Cost: first-tier
internet egress across AWS/Azure/GCP. Latency: bottleneck party's egress at
25 Gbps + pipeline fill at 2 ms RTT, fully pipelined single query.

| workload | comparisons/query | protocol | traffic/query | cost/query | latency @ 25 Gbps |
|---|---|---|---|---|---|
| vector, 1 M | 1.0 × 10⁶ | 3PC | 23.0 MB / 14.1 MB | $0.0023 / $0.0014 (×4: $0.0092 / $0.0056) | ≈ 10 ms / 20 ms (×4: 17 ms / 24 ms) |
| | | 5PC | 46 MB | $0.0043 (×4: $0.017) | ≈ 40 ms (×4: 48 ms) |
| vector, 20 M | 2.0 × 10⁷ | 3PC | 461 MB / 281 MB | $0.046 / $0.028 (×4: $0.18 / $0.11) | ≈ 56 ms / 48 ms (×4: 0.20 s / 0.14 s) |
| | | 5PC | 928 MB | $0.086 (×4: $0.34) | ≈ 0.11 s (×4: 0.34 s) |
| iris, 1 M | 3.1 × 10⁷ | 3PC | 3.00 GB / 2.36 GB | $0.30 / $0.23 (×4: $1.19 / $0.94) | ≈ 0.34 s / 0.28 s (×4: 1.3 s / 1.0 s) |
| | | 5PC | 6.07 GB | $0.56 (×4: $2.23) | ≈ 0.6 s (×4: 2.1 s) |
| iris, 20 M | 6.2 × 10⁸ | 3PC | 60.0 GB / 47.2 GB | $5.94 / $4.68 (×4: $23.8 / $18.7) | ≈ 6.4 s / 5.1 s (×4: 26 s / 20 s) |
| | | 5PC | 121 GB | $11.2 (×4: $44.6) | ≈ 10 s (×4: 40 s) |

Bracketed totals cover a full uniqueness check: 2 eyes × 2 orientations
(the mirror-flip scan for Mirror PAD) = 4 independent DB scans. Traffic and
cost are exactly ×4. Latency assumes the 4 scans run concurrently over the
shared links: wire time quadruples but the pipeline fill is paid once
(4 × wire + fill), which matters for the fill-dominated small-scan cells —
strictly sequential scans would instead pay 4 × (wire + fill), e.g. 80 ms
rather than 24 ms for vector-1M 3PC ripple. Wire-dominated cells (all iris
rows at 20 M) are the same under either schedule.

5PC latency uses the heaviest party (P0, ~25.5% of fleet bytes in both
pipelines) against the 25 Gbps aggregate VM egress cap; 3PC uses the
symmetric per-party share on the ring link. Note the vector-1M inversion: at small scans the prefix
adder's shorter pipeline fill (~7 vs ~18 legs) beats the ripple adder's
smaller wire time. Sections below give the per-phase measurements, pricing
model, and assumptions behind every cell.

## 1. Bytes transferred per comparison

Bytes are the serialized protocol messages summed over **all** parties
(excluding TCP/TLS transport framing and one-time PRF-seed setup).

| configuration | share inputs | lift to u32 | MSB extraction | open | **total** | per party |
|---|---|---|---|---|---|---|
| **3PC 16-bit, as configured in `vector-mpc-poc`** (parallel-prefix adder, default) | 6.00 B | — | 16.65 B | 0.38 B | **23.03 B** | 7.68 B |
| **3PC 16-bit, byte-optimal** (`ripple_carry_adder` feature) | 6.00 B | — | 7.68 B | 0.38 B | **14.07 B** | 4.69 B |
| **3PC iris FHD 32-bit, as shipped by iris-mpc** (prefix adder, default) | 12.00 B | 48.07 B | 36.28 B | 0.38 B | **96.72 B** | 32.24 B |
| **3PC iris FHD 32-bit** (`ripple_carry_adder` feature) | 12.00 B | 48.07 B | 15.74 B | 0.38 B | **76.19 B** | 25.40 B |
| **5PC 16-bit extension** (`rss5`, ripple-style compressor adder) | 16.01 B | — | 29.15 B | 1.26 B | **46.42 B** | 9.28 B |
| **5PC iris FHD 32-bit** (`rss5` FHD ops) | 32.01 B | 44.17 B | 118.30 B | 1.26 B | **195.75 B** | 39.15 B |

Notes:

* `vector-mpc-poc` calls `iris_mpc_cpu::protocol::ops::{galois_ring_to_rep3,
  sub_pub, lt_zero_and_open_u16}` with default features — the identical
  pipeline measured here as the 3PC baseline. The two 3PC rows differ only in
  the binary adder: the default parallel-prefix adder pays ~2× the MSB bytes
  to cut sequential AND rounds from ~15 to ~4; the ripple-carry feature is
  the byte-optimal choice when latency is hidden by pipelining DB chunks
  across parallel sessions (as the poc does).
* The 5PC ratio is 2.02× against the poc's default configuration and 3.30×
  against the byte-optimal 3PC.
* The **iris FHD rows are the actual production iris comparison** — the
  32-bit lifted pipeline that `iris-mpc` imports from this repo
  (`ampc-actor-utils`): rep3-share *two* dot products (code and mask), lift
  both u16 → u32 (`batch_signed_lift_vec`: bit-sliced two-carry binary adder
  + bit injection), multiply by the public threshold constants and extract
  the u32 sign (`fhd_greater_than_threshold`), open. Measured in the same
  harness with synthetic in-range dots and verified against plaintext. The
  lift dominates at 48 B (63% of the ripple total) — the 32-bit pipeline is
  **5.4×** the 16-bit one (ripple; 4.2× comparing default configs), not the
  ~2× a naive width-scaling argument suggests, because the lift has no
  16-bit counterpart. iris-mpc also has an NHD mode lifting to a 48-bit
  ring (`nhd_ops`, `Ring48`), which would cost more again.
* The **5PC FHD row** is the `rss5` 32-bit pipeline. It needs no share lift
  and no bit injection: multiplying by `B = 2^16` lifts the code addends for
  free, and the mask addends' two wrap bits (one 16-bit carry circuit — its
  "lift" phase, 44.2 B, is *cheaper* than the 3PC lift's 48.1 B) are folded
  into a 32-plane zero-aware adder as sparse public-coefficient addends.
  Result: **2.57×** the 3PC FHD bytes, well under the ~3.3× the 16-bit
  pipelines exhibit, because the wrap-bit trick replaces the two most
  expensive parts of a naive port (a second lift and a 5PC bit injection).
* Per-comparison values are independent of DB size up to amortized
  per-message framing (~0.4% at `db_size = 4096`).
* This workload compares each pair once. Iris matching performs 31 rotations
  per pair, so iris per-pair costs are 31× the FHD per-comparison figures.

## 2. Cross-cloud egress cost (3PC)

Scenario: the three parties run on AWS, Azure, and GCP respectively, so all
protocol traffic crosses the public internet. Cloud providers charge for
**egress only** (ingress is free on all three), and 3PC traffic is symmetric
— each party sends total/3 bytes per comparison.

List prices for internet egress, first pricing tier, on-demand (mid-2026;
1 GB taken as 10^9 bytes):

| provider | $/GB | poc default (7.68 B/party) | byte-optimal (4.69 B/party) |
|---|---|---|---|
| AWS (us-east-1) | $0.090 | $0.69 /10⁹ comp. | $0.42 /10⁹ comp. |
| Azure (via Microsoft network) | $0.087 | $0.67 /10⁹ comp. | $0.41 /10⁹ comp. |
| GCP (premium tier) | $0.120 | $0.92 /10⁹ comp. | $0.56 /10⁹ comp. |
| **fleet total** | — | **$2.28 per 10⁹ comparisons** | **$1.39 per 10⁹ comparisons** |

Equivalently, per comparison the whole fleet pays **≈ $2.3 × 10⁻⁹** (poc
default) or **≈ $1.4 × 10⁻⁹** (byte-optimal).

Per-query full-scan costs. Vector rows use the measured 16-bit pipeline;
iris rows use the **measured 32-bit FHD pipeline** (96.72 B default /
76.19 B ripple per comparison) × 31 rotations per DB item. "Default" is
each stack as it ships (parallel-prefix adder); "ripple" enables
`ripple_carry_adder`:

| workload | comparisons/query | fleet traffic/query (default / ripple) | cost/query (default / ripple) |
|---|---|---|---|
| vector, 1 M DB | 1.0 × 10⁶ | 23.0 MB / 14.1 MB | $0.0023 / $0.0014 |
| vector, 20 M DB | 2.0 × 10⁷ | 461 MB / 281 MB | $0.046 / $0.028 |
| iris, 1 M DB | 3.1 × 10⁷ | 3.00 GB / 2.36 GB | $0.30 / $0.23 |
| iris, 20 M DB | 6.2 × 10⁸ | 60.0 GB / 47.2 GB | $5.94 / $4.68 |

Traffic and cost are fleet totals (sum over the three parties); each party
carries one third. At the 20 M-iris scale any meaningful query rate moves
every party deep past the first egress pricing tier (10 k queries/day ≈
157–200 TB/day/party), where AWS/Azure rates fall toward $0.05–0.07/GB.

### Query latency at 25 Gbps between parties

Cross-cloud, same-metro deployment (e.g. AWS us-east-1 / Azure East US /
GCP us-east4): ~2 ms RTT between parties; per-VM internet-egress caps are
the binding constraint, with GCP the tightest (7 Gbps standard, 25 Gbps
with per-VM Tier_1 networking). Assuming 25 Gbps sustained on each directed
ring link (= 3.125 GB/s carrying one party's egress) and a fully pipelined
chunked scan, single-query latency ≈ wire time + one pipeline fill of
sequential protocol rounds (~18 one-hop legs ≈ 18 ms for the ripple-carry
adder, ~7 legs ≈ 7 ms for the parallel-prefix adder, at ~1 ms/leg):

| workload | ripple feature | default (prefix) |
|---|---|---|
| vector, 1 M DB | 1.5 ms wire → **≈ 20 ms** | 2.5 ms wire → **≈ 10 ms** |
| vector, 20 M DB | 30 ms wire → **≈ 48 ms** | 49 ms wire → **≈ 56 ms** |
| iris, 1 M DB | 252 ms wire → **≈ 0.28 s** | 320 ms wire → **≈ 0.34 s** |
| iris, 20 M DB | 5.04 s wire → **≈ 5.1 s** | 6.40 s wire → **≈ 6.4 s** |

Iris rows use the measured 32-bit FHD per-party figures (25.40 B ripple /
32.24 B prefix per comparison); the FHD lift adds ~10 further sequential
legs of pipeline fill, negligible at these wire times.

The bytes-vs-rounds trade-off crosses over around ~11 M comparisons/query:
below it (vector 1 M) the prefix adder's fewer rounds win despite ~1.6× the
bytes; above it (iris workloads) the ripple adder's smaller wire time
dominates. Figures are network-only and assume local compute keeps pace
(at 667 M comparisons/s the 512-dim u16 dot products alone are ~3.4 × 10¹¹
multiply-adds/s — a well-parallelized multicore per party); under
concurrent load, queries share the link, so these are floor latencies at
the previously stated throughput ceilings (~667 M/s byte-optimal, ~407 M/s
default).

Caveats:

* First-tier list prices. Volume tiers (>10 TB/mo), committed-use discounts,
  or GCP's standard network tier ($0.085/GB, dropping the fleet total to
  ≈ $1.23/10⁹ byte-optimal) reduce these; at the 10¹³-comparison scale above
  every party is deep into cheaper tiers.
* Measured bytes exclude TCP/IP + TLS framing. With chunked batching the
  messages are KB–MB sized, so real overhead is a few percent; unbatched
  operation would inflate it substantially.
* Egress dominated by protocol messages only — one-time setup (PRF seeds,
  session establishment) is negligible (51 B total for 3PC).

### 5PC egress cost

5PC traffic is asymmetric — measured per-party egress per comparison:

| pipeline | P0 | P1 | P2 | P3 | P4 | fleet |
|---|---|---|---|---|---|---|
| 16-bit | 11.79 B | 11.79 B | 11.54 B | 5.65 B | 5.65 B | 46.42 B |
| 32-bit FHD | 50.13 B | 50.13 B | 49.88 B | 22.81 B | 22.81 B | 195.75 B |

With five parties on three providers, a 2+2+1 placement keeps any single
compromised provider within the `t = 2` threshold. Cost-aware placement
(Azure hosts P0+P1, AWS hosts P2+P4, GCP hosts P3) gives
**≈ $4.3 × 10⁻⁹ per comparison** for the 16-bit pipeline and
**≈ $1.80 × 10⁻⁸** for the FHD pipeline — ~3.1× and ~2.4× the respective
ripple-3PC costs (the cost ratios sit below the byte ratios because the
placement puts the heavy senders on cheap egress).

| workload | comparisons/query | 5PC fleet traffic/query | 5PC cost/query | 3PC (ripple) |
|---|---|---|---|---|
| vector, 1 M DB | 1.0 × 10⁶ | 46 MB | $0.0043 | $0.0014 |
| vector, 20 M DB | 2.0 × 10⁷ | 928 MB | $0.086 | $0.028 |
| iris, 1 M DB | 3.1 × 10⁷ | 6.07 GB | $0.56 | $0.23 |
| iris, 20 M DB | 6.2 × 10⁸ | 121 GB | $11.2 | $4.68 |

(5PC has a single adder configuration — the ripple-style compressor; there
is no parallel-prefix variant to compare. Placing two parties on one
provider means two colluding *providers* could exceed `t = 2`; five
independent operators would be the more conservative deployment.)

## 3. Where the numbers come from

* Protocol implementations: 3PC in `ampc-actor-utils` (production code,
  identical to `iris-mpc-cpu` at the rev pinned by the poc); 5PC in
  `ampc_actor_utils::protocol::rss5`.
* Harness: `vector-mpc-5pc/src/main.rs` — all four pipelines (16-bit and
  32-bit FHD, at 3 and 5 parties) run over an in-process mesh wrapped in a
  counting `Networking` implementation; every opened bit is checked against
  the plaintext computation.
* Protocol details, trade-offs, and the 5PC security argument: see
  [README.md](README.md) and the `rss5` module documentation.
