# vector-mpc-5pc

Measurement harness comparing the 3-party MPC vector-comparison pipeline
(the production implementation in `ampc-actor-utils`) against its 5-party
extension (`ampc_actor_utils::protocol::rss5`), for the workload of
[`vector-mpc-poc`](https://github.com/worldcoin/vector-mpc-poc): secure inner
product + threshold comparison over secret-shared 512-dim int4 vectors.

The protocol code lives in the shared crates — this crate contains only the
harness:

* **`ampc-secret-sharing`** — the Galois-ring Shamir layer. The existing
  3-party encoding (`ShamirGaloisRingShare::encode_3*`,
  `IrisVector::secret_share`) is extended with degree-2 sharing over 5
  points (`encode_5`, `deg_4_lagrange_polys_at_zero`,
  `IrisVector::secret_share_5`, `multiply_lagrange_coeffs_5`).
* **`ampc-actor-utils`** — the protocol layer. The 3-party pipeline is the
  existing production code (`galois_ring_to_rep3`, `sub_pub`,
  `extract_msb_batch`, `open_bin`); the new `protocol::rss5` module is the
  5-party counterpart, built on the same `NetworkSession` / `Networking` /
  `NetworkValue` abstractions (and therefore usable over the crate's TCP/TLS
  transport as well).
* **this crate** — runs both pipelines in one process over
  `LocalNetworkingStore` wrapped in a byte-counting `Networking`
  implementation, verifies every opened bit against the plaintext
  computation, and reports bytes per comparison. Counted bytes are the
  serialized `NetworkValue` messages handed to the transport (excluding
  TCP/TLS framing); the harness enables the `ripple_carry_adder` feature for
  byte-optimal MSB extraction on both sides.

## Encoding

* Vectors are packed 4 coefficients at a time into elements of the Galois
  ring `GR(2^16, 4)` using the basis-A/basis-B trick, so the integer dot
  product of two 4-chunks equals a coefficient of the ring product.
* Inputs are Shamir-shared over the ring at points of the exceptional
  sequence; the query share is pre-multiplied by the Lagrange coefficient at
  zero and moved to basis B, so a plain wrapping `udot` of a query share
  with a DB share yields one **additive share of the dot product mod 2^16**.
* The only change for 5PC is the polynomial degree: degree-1 with 3 shares
  (1 corruption) becomes degree-2 with 5 shares (2 corruptions); the product
  polynomial has degree 4 and is reconstructed with the degree-4 Lagrange
  coefficients over all 5 points.

## Comparison protocol

Both pipelines share the same structure: turn the additive distance shares
into a small number of binary adder inputs, subtract the public threshold,
extract the MSB with a bit-sliced binary adder, and open the resulting bit.

* **3PC (production)**: `galois_ring_to_rep3` replicates each additive share
  to the next party; `extract_msb_batch` reduces the value to `t + 1 = 2`
  addends via `two_way_split` (each party re-shares one third of the batch)
  and runs a 2-input ripple-carry adder (15 bit-plane ANDs at 3 bits each).
* **5PC (`rss5`)**: values are shared with 2-out-of-5 replicated secret
  sharing — one additive component per 2-subset of parties, held by the 3
  parties outside it, so any 2 colluders miss exactly one component. The
  pipeline reduces the 5 additive shares to `t + 1 = 3` clear addends
  (masked redistribution from parties 3/4 to parties 0/1, with each mask
  split across two pairwise-PRF compensators so no coalition containing the
  recipient can strip it), input-shares their bit-planes directly, and runs
  a full adder + ripple-carry adder (29 bit-plane ANDs at 8 bits each; every
  AND gate reshares its local products through the same
  redistribute-then-input-share pipeline). Opens deliver each party's 4
  missing components via per-component designated providers (10 messages).

See the `rss5` module documentation for the full protocol and privacy
arguments.

## Measured communication

`cargo run --release -p vector-mpc-5pc -- 4096 2` (per-comparison values are
independent of `db_size`/`num_queries` up to amortized per-message framing):

| | 3PC (t=1) | 5PC (t=2) |
|---|---|---|
| share adder inputs | 6.004 B | 16.010 B |
| MSB extraction | 7.684 B | 29.146 B |
| open | 0.379 B | 1.262 B |
| **total, all parties** | **14.07 B/comparison** | **46.42 B/comparison** |
| per party average | 4.69 B | 9.28 B |
| one-hop message legs per query | 18 | 33 |

**5PC costs 3.30× the communication of 3PC.** The gap is the price of
tolerating 2 corruptions instead of 1 with replicated sharing: every
secret-dependent value must reach 2 peers instead of 1 (8 vs 3 bits per
bit-plane AND), the comparison needs 3 addends instead of 2 (29 vs 15 ANDs),
and openings and input paths replicate accordingly.

Breakdown per comparison (aggregate, excluding ~0.4% framing): 3PC =
rep3 conversion 6 B + split 2 B + 15 plane-ANDs × 3 bits + 1 open bit ×
3 parties; 5PC = redistribution 4 B + input sharing 12 B + 29 plane-ANDs ×
8 bits + open 10 bits.

History: the first version of this PoC (self-contained, raw-payload
counting) measured 190 B/comparison for 5PC — a full 10-component
arithmetic RSS sharing summed by a 134-AND compressor tree — against a
17.25 B mirror of the iris-mpc 3PC reference. Reducing the adder to 3
clear-addend inputs brought 5PC to 54.75 B, the cheap AND resharing and
provider-assigned open to 46.25 B. The baseline dropped from 17.25 to
14.07 B when switching to the production 3PC implementation, whose
`two_way_split` applies the same addend-reduction idea (2 inputs, no full
adder).

## Trade-offs

**Bytes vs rounds.** Both pipelines are tuned for bytes:

* The harness enables `ripple_carry_adder`; the crate default is the
  parallel-prefix adder, which costs roughly 2× the adder bytes but reduces
  the sequential AND rounds from ~14–15 to ~4.
* The 5PC cheap AND resharing (8 bits/gate) is a two-leg protocol —
  redistribute, then input-share — where a direct 5-sharer resharing
  (10 bits/gate) is one leg (33 vs 19 legs per query). Reverting just the 14
  sequential ripple-carry gates costs +3.5 B/comparison for 19 legs.
* In a real deployment, round latency is hidden the way the reference PoC
  does it: pipeline DB chunks across parallel sessions, so throughput is
  bandwidth-bound, not RTT-bound.

**Price of the corruption threshold.** The 3.30× is intrinsic to (5,2)-RSS:
replication factor 3 means each party holds 6 of 10 components, every
correction reaches 2 co-holders, and the addend floor is `t + 1 = 3`.
Per-party DB storage is unchanged (same `u16` share vectors), but the fleet
stores 5/3× in aggregate and each party does several times the local
bit-AND work plus more PRF evaluations.

**Exactness vs cheaper comparisons.** The pipeline computes the 16-bit
comparison exactly. If the application can bound distances more tightly
(e.g. normalized embeddings with |dot| < 2^11) or tolerate approximate
thresholds, the dominant costs scale linearly with bit width; a two-stage
scan (truncated coarse compare, exact compare only for the uncertainty
band) roughly halves 5PC bytes at the cost of data assumptions this harness
does not make.

**Security model.** Semi-honest, static corruption of any 2 of 5 (3PC: 1 of
3). The privacy of the redistribution masks and input sharings rests on a
hand-checked case analysis over all 10 possible 2-coalitions (see the
`rss5` module docs); a production deployment should have a written
simulation proof, and malicious security would need honest-majority
consistency checking at roughly 2–3× the communication. The PRF-lockstep
design also assumes all parties execute the same deterministic protocol
schedule.

**Alternatives considered and rejected for this workload.**

* *FSS/DCF comparison* (dealer-aided): near-zero online cost, but a fresh
  ~260 B key per comparison — gigabytes of preprocessing per query at
  flat-scan scale — and a non-colluding dealer, which violates any-2-of-5.
* *Prime-field Shamir comparisons*: random bits are cheap mod p, but
  comparisons become per-item field multiplications, losing the 64×-packed
  bit-slicing that makes these ANDs cheap.
* *Packed secret sharing*: no packing room — n=5, t=2 forces degree-2
  polynomials and multiplication already consumes all 5 evaluation points.
* *Delegating the binary phase to 3 parties*: any 3-party sub-protocol is at
  best 1-private, and the 2-corruption bound is global.

**Measurement scope.** Counted bytes are serialized `NetworkValue` messages
(descriptor + length + payload; ~0.4% of the total at `db_size = 4096`),
excluding transport framing, TLS and the one-time PRF-seed setup. Costs are
asymmetric per party (5PC parties 0–2 carry the addends; 3–4 only
redistribute), so the per-party figure is an average. The harness measures
traffic, not wall-clock performance.

## Running

```bash
# measurement harness (defaults: db_size=4096, num_queries=2);
# verifies every opened result against the plaintext computation
cargo run --release -p vector-mpc-5pc -- [db_size] [num_queries]

# protocol unit tests live with the protocol code
cargo test -p ampc-actor-utils rss5
cargo test -p ampc-secret-sharing
```
