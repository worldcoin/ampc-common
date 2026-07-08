//! 5-party protocol layer (semi-honest, honest majority, tolerates 2
//! corruptions), extending the 3-party replicated protocols in this crate.
//!
//! Values are shared with 2-out-of-5 *replicated secret sharing* (RSS): a
//! value `x` is split into `C(5,2) = 10` additive components `x_T`, one per
//! 2-subset `T` of the parties, and `x_T` is held by the 3 parties *not* in
//! `T` (6 components per party). Any 2 colluding parties miss the component
//! indexed by their own pair and therefore learn nothing. This is the
//! smallest replicated scheme with threshold 2 — the ABY3-style rep3 sharing
//! used by [`crate::protocol::ops`] is the same construction for
//! `n = 3, t = 1`.
//!
//! The primary use case mirrors the 3-party distance pipeline
//! (`galois_ring_to_rep3` + `lt_zero_and_open_u16`): the inputs are additive
//! 5-sharings of u16 values (e.g. produced by local dot products over
//! degree-2 Galois-ring Shamir shares, see
//! `ampc_secret_sharing::galois::degree4::ShamirGaloisRingShare::encode_5`),
//! and the output is the opened "less than zero" bit per value:
//!
//! * **Redistribute 5 -> 3** ([`redistribute`]): parties 3 and 4 hand their
//!   masked shares to parties 0 and 1, leaving three addends
//!   `v_0 = d_0 + d̂_3` (party 0), `v_1 = d_1 + d̂_4` (party 1),
//!   `v_2 = d_2 - masks` (party 2), each known *in the clear* to one party.
//!   Raw additive shares carry algebraic structure from the Shamir layer, so
//!   each transferred share is padded with two pairwise-PRF masks whose
//!   compensation is split across two parties — no single party besides the
//!   sender knows the whole mask, so no coalition with the recipient can
//!   strip it. Three addends is the minimum for threshold 2: with two, the
//!   two holders would jointly know the value (the 3-party analogue,
//!   `two_way_split`, reduces to `t + 1 = 2` addends the same way).
//! * **Input sharing** ([`share_adder_inputs`]): each addend holder
//!   bit-decomposes its value locally and shares the 16 bit-planes as binary
//!   RSS — 5 of the 6 components come from PRF keys shared with the other
//!   holders (no communication), and the designated correction component is
//!   sent to its 2 other holders. Whoever knows a value can share its bits
//!   directly, so no arithmetic-to-binary conversion is needed.
//! * **AND / multiplication**: since `2t < n`, the product of two RSS
//!   sharings is computed locally — every component product `x_S * y_R` has
//!   `|S u R| <= 4`, so some party outside `S u R` holds both factors; each
//!   product is assigned to the lowest-index such party. The local sums form
//!   a binary additive 5-sharing of the product, which is converted back to
//!   RSS with the same redistribute-then-input-share pipeline (8
//!   element-sends per gate instead of the 10 of a direct 5-sharer
//!   resharing, at the cost of one extra message leg per gate).
//! * **MSB extraction** ([`extract_msb`]): the 3 shared inputs are summed
//!   mod 2^16 by a full adder followed by a ripple-carry adder (29 bit-plane
//!   ANDs), mirroring the structure of the 3-party binary adder.
//! * **Open** ([`open_bit_plane`]): every party XORs its 6 held components;
//!   each of the 4 components it misses is delivered by that component's
//!   designated provider (its lowest-index holder), aggregated per
//!   provider/receiver pair — 10 messages in total. As with any replicated
//!   opening, the revealed components are determined by the opened value and
//!   the receivers' own shares, so nothing beyond the value leaks.
//!
//! Sessions run over the crate's [`NetworkSession`] with 5 role assignments;
//! per-component and pairwise PRF streams are derived from a per-party seed
//! at session setup ([`setup_rss5_session`]) and advanced in lockstep, which
//! assumes all parties execute the same deterministic protocol schedule.

use crate::{
    execution::{
        player::Role,
        session::{NetworkSession, SessionHandles},
    },
    network::mpc::{NetworkInt, NetworkValue},
    protocol::prf::PrfSeed,
};
use aes_prng::AesRng;
use ampc_secret_sharing::{IntRing2k, RingElement};
use eyre::{bail, eyre, Result};
use rand::{distributions::Standard, prelude::Distribution, Rng, SeedableRng};

pub const NUM_PARTIES: usize = 5;
/// All 2-subsets of the 5 parties, in lexicographic order.
pub const NUM_SETS: usize = 10;
pub const SETS: [(usize, usize); NUM_SETS] = [
    (0, 1),
    (0, 2),
    (0, 3),
    (0, 4),
    (1, 2),
    (1, 3),
    (1, 4),
    (2, 3),
    (2, 4),
    (3, 4),
];

/// Bit planes of a u16 value.
const PLANES: usize = 16;

fn set_contains(t: usize, p: usize) -> bool {
    SETS[t].0 == p || SETS[t].1 == p
}

fn set_index(a: usize, b: usize) -> usize {
    let (a, b) = if a < b { (a, b) } else { (b, a) };
    SETS.iter()
        .position(|&s| s == (a, b))
        .expect("not a valid 2-subset")
}

/// The 3 holders of component `t` (parties not in `SETS[t]`), ascending.
pub fn holders(t: usize) -> Vec<usize> {
    (0..NUM_PARTIES).filter(|&p| !set_contains(t, p)).collect()
}

/// The designated correction component of sharer `i`: `{i+1, i+2}`.
/// When party `i` (re-)shares a value, this is the component that carries
/// its correction; the other components not containing `i` come from PRFs.
fn corr_set(i: usize) -> usize {
    set_index((i + 1) % NUM_PARTIES, (i + 2) % NUM_PARTIES)
}

/// The 2 parties (besides the sharer) holding the correction component.
fn corr_receivers(i: usize) -> [usize; 2] {
    [(i + 3) % NUM_PARTIES, (i + 4) % NUM_PARTIES]
}

/// The designated provider of component `t` for opening: its lowest-index
/// holder. It sends the component to the 2 parties in `SETS[t]` (aggregated
/// per receiver with the other components it provides).
fn open_provider(t: usize) -> usize {
    holders(t)[0]
}

/// Static per-party tables.
pub struct Topology {
    pub party: usize,
    /// The 6 component sets this party holds (global indices, ascending).
    pub held: Vec<usize>,
    /// Global set index -> local component slot (None if not held).
    pub local_idx: [Option<usize>; NUM_SETS],
    /// Local component index pairs `(s, r)` of the ordered component products
    /// `x_S * y_R` assigned to this party (lowest-index party outside
    /// `S u R`).
    pub and_pairs: Vec<(usize, usize)>,
}

impl Topology {
    pub fn new(party: usize) -> Self {
        let held: Vec<usize> = (0..NUM_SETS).filter(|&t| !set_contains(t, party)).collect();
        let mut local_idx = [None; NUM_SETS];
        for (li, &t) in held.iter().enumerate() {
            local_idx[t] = Some(li);
        }
        let mut and_pairs = Vec::new();
        for (ls, &s) in held.iter().enumerate() {
            for (lr, &r) in held.iter().enumerate() {
                let assignee = (0..NUM_PARTIES)
                    .find(|&q| !set_contains(s, q) && !set_contains(r, q))
                    .expect("|S u R| <= 4, some party is outside");
                if assignee == party {
                    and_pairs.push((ls, lr));
                }
            }
        }
        Topology {
            party,
            held,
            local_idx,
            and_pairs,
        }
    }
}

/// A 2-out-of-5 replicated share: this party's copies of the 6 components it
/// holds, ordered by `Topology::held`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Share5<T: IntRing2k> {
    pub c: [RingElement<T>; 6],
}

impl<T: IntRing2k> Share5<T> {
    pub fn zero() -> Self {
        Share5 {
            c: [RingElement(T::default()); 6],
        }
    }
}

/// One bit-plane of RSS-shared u64 words (64 values per word).
pub type Plane5 = Vec<Share5<u64>>;
/// A bit-sliced 16-bit value: 16 planes of RSS-shared u64 words.
pub type Value5 = Vec<Plane5>;

/// This party's clear addend for the binary adder after redistribution:
/// parties 0, 1 and 2 each hold one 16-bit value per input; parties 3 and 4
/// hold none.
pub struct AdderInput {
    pub len: usize,
    value: Option<Vec<RingElement<u16>>>,
}

impl AdderInput {
    pub fn value(&self) -> Option<&[RingElement<u16>]> {
        self.value.as_deref()
    }
}

/// A 5-party RSS session: the underlying [`NetworkSession`] plus the
/// per-component and pairwise PRF streams.
pub struct Rss5Session {
    pub network_session: NetworkSession,
    pub topo: Topology,
    /// One PRF stream per held component set, shared with the other 2
    /// holders and advanced in lockstep. Used for (re-)sharing corrections.
    streams: Vec<Option<AesRng>>,
    /// One PRF stream per party pair this party belongs to (indexed by the
    /// same `SETS` table). Used to mask the redistributed additive shares.
    pair_streams: Vec<Option<AesRng>>,
}

/// Establish the shared PRF keys over a 5-party [`NetworkSession`]: for each
/// 2-subset, the lowest-index holder samples a seed for the 3 holders
/// (component streams); for each pair, the lower party samples a seed for
/// the two of them (mask streams). All seeds are derived from `my_seed`.
///
/// The 5-party analogue of [`crate::protocol::ops::setup_replicated_prf`].
pub async fn setup_rss5_session(
    mut network_session: NetworkSession,
    my_seed: PrfSeed,
) -> Result<Rss5Session> {
    if network_session.num_parties() != NUM_PARTIES {
        bail!(
            "rss5 requires 5 role assignments, got {}",
            network_session.num_parties()
        );
    }
    let party = network_session.own_role().index();
    let topo = Topology::new(party);
    let mut seed_rng = AesRng::from_seed(my_seed);

    let recv_seed = |value: NetworkValue| -> Result<[u8; 16]> {
        match value {
            NetworkValue::PrfKey(seed) => Ok(seed),
            _ => Err(eyre!("expected PrfKey during rss5 setup")),
        }
    };

    let mut streams: Vec<Option<AesRng>> = (0..NUM_SETS).map(|_| None).collect();
    for t in 0..NUM_SETS {
        let holders = holders(t);
        if party == holders[0] {
            let seed: PrfSeed = seed_rng.gen();
            for &h in &holders[1..] {
                network_session
                    .send_to_role(Role::new(h), NetworkValue::PrfKey(seed))
                    .await?;
            }
            streams[t] = Some(AesRng::from_seed(seed));
        } else if holders.contains(&party) {
            let seed = recv_seed(
                network_session
                    .receive_from_role(Role::new(holders[0]))
                    .await?,
            )?;
            streams[t] = Some(AesRng::from_seed(seed));
        }
    }
    let mut pair_streams: Vec<Option<AesRng>> = (0..NUM_SETS).map(|_| None).collect();
    for (t, stream) in pair_streams.iter_mut().enumerate() {
        let (a, b) = SETS[t];
        if party == a {
            let seed: PrfSeed = seed_rng.gen();
            network_session
                .send_to_role(Role::new(b), NetworkValue::PrfKey(seed))
                .await?;
            *stream = Some(AesRng::from_seed(seed));
        } else if party == b {
            let seed = recv_seed(network_session.receive_from_role(Role::new(a)).await?)?;
            *stream = Some(AesRng::from_seed(seed));
        }
    }
    Ok(Rss5Session {
        network_session,
        topo,
        streams,
        pair_streams,
    })
}

impl Rss5Session {
    async fn send_elems<T: IntRing2k + NetworkInt>(
        &mut self,
        to: usize,
        elems: Vec<RingElement<T>>,
    ) -> Result<()> {
        self.network_session
            .send_to_role(Role::new(to), T::new_network_vec(elems))
            .await
    }

    async fn recv_elems<T: IntRing2k + NetworkInt>(
        &mut self,
        from: usize,
        expected_len: usize,
    ) -> Result<Vec<RingElement<T>>> {
        let v = T::into_vec(
            self.network_session
                .receive_from_role(Role::new(from))
                .await?,
        )?;
        if v.len() != expected_len {
            bail!(
                "rss5 message size mismatch: expected {expected_len}, got {}",
                v.len()
            );
        }
        Ok(v)
    }

    /// Draw `len` mask elements from the pairwise stream of `{a, b}`;
    /// both members draw the same values in lockstep.
    fn draw_pair_mask<T: IntRing2k>(
        &mut self,
        a: usize,
        b: usize,
        len: usize,
    ) -> Vec<RingElement<T>>
    where
        Standard: Distribution<T>,
    {
        let stream = self.pair_streams[set_index(a, b)]
            .as_mut()
            .expect("party is a member of the pair");
        (0..len).map(|_| stream.gen::<RingElement<T>>()).collect()
    }

    /// Binary (XOR) variant of [`redistribute`]: turn an additive 5-sharing
    /// of u64 words into 3 addends held in the clear by parties 0, 1 and 2
    /// (`Some` for them, `None` for parties 3 and 4). The transferred shares
    /// are padded with the same split-compensation pairwise masks as the
    /// arithmetic version.
    async fn redistribute_words(
        &mut self,
        items: &[RingElement<u64>],
    ) -> Result<Option<Vec<RingElement<u64>>>> {
        let len = items.len();
        let xor3 = |items: &[RingElement<u64>],
                    a: Vec<RingElement<u64>>,
                    b: Vec<RingElement<u64>>|
         -> Vec<RingElement<u64>> {
            items
                .iter()
                .zip(a)
                .zip(b)
                .map(|((&x, y), z)| x ^ y ^ z)
                .collect()
        };
        Ok(match self.topo.party {
            0 => {
                let alpha_p = self.draw_pair_mask::<u64>(0, 4, len);
                let z3_hat = self.recv_elems::<u64>(3, len).await?;
                Some(xor3(items, z3_hat, alpha_p))
            }
            1 => {
                let alpha = self.draw_pair_mask::<u64>(1, 3, len);
                let z4_hat = self.recv_elems::<u64>(4, len).await?;
                Some(xor3(items, z4_hat, alpha))
            }
            2 => {
                let beta = self.draw_pair_mask::<u64>(2, 3, len);
                let beta_p = self.draw_pair_mask::<u64>(2, 4, len);
                Some(xor3(items, beta, beta_p))
            }
            3 => {
                let alpha = self.draw_pair_mask::<u64>(1, 3, len);
                let beta = self.draw_pair_mask::<u64>(2, 3, len);
                self.send_elems(0, xor3(items, alpha, beta)).await?;
                None
            }
            4 => {
                let alpha_p = self.draw_pair_mask::<u64>(0, 4, len);
                let beta_p = self.draw_pair_mask::<u64>(2, 4, len);
                self.send_elems(1, xor3(items, alpha_p, beta_p)).await?;
                None
            }
            _ => unreachable!(),
        })
    }

    /// Share a vector of u64 words known in the clear to `sharer` (an
    /// "input sharing"): 5 of the 6 components come from the component
    /// streams (derived locally by their holders), and the sharer sends the
    /// correction component to its 2 other holders. Components containing
    /// the sharer are zero.
    async fn input_share_words(
        &mut self,
        sharer: usize,
        len: usize,
        values: Option<Vec<RingElement<u64>>>,
    ) -> Result<Vec<Share5<u64>>> {
        let p = self.topo.party;
        let d = corr_set(sharer);
        debug_assert_eq!(values.is_some(), p == sharer);
        let mut out = vec![Share5::<u64>::zero(); len];
        // The sharer's correction starts as the plaintext words and
        // accumulates the XOR of its PRF components.
        let mut corr = values;

        for t in 0..NUM_SETS {
            let Some(li) = self.topo.local_idx[t] else {
                continue;
            };
            if set_contains(t, sharer) || t == d {
                continue;
            }
            let stream = self.streams[t].as_mut().expect("held set has a stream");
            for (k, out_word) in out.iter_mut().enumerate() {
                let r = stream.gen::<RingElement<u64>>();
                out_word.c[li] ^= r;
                if let Some(corr) = corr.as_mut() {
                    corr[k] ^= r;
                }
            }
        }

        if p == sharer {
            let corr = corr.expect("sharer has plaintext words");
            let [r1, r2] = corr_receivers(sharer);
            self.send_elems(r1, corr.clone()).await?;
            self.send_elems(r2, corr.clone()).await?;
            let li = self.topo.local_idx[d].expect("sharer holds its correction set");
            for (out_word, c) in out.iter_mut().zip(corr) {
                out_word.c[li] ^= c;
            }
        } else if corr_receivers(sharer).contains(&p) {
            let v = self.recv_elems::<u64>(sharer, len).await?;
            let li = self.topo.local_idx[d].expect("receiver holds the correction set");
            for (out_word, c) in out.iter_mut().zip(v) {
                out_word.c[li] ^= c;
            }
        }
        Ok(out)
    }

    /// Convert a binary additive 5-sharing into a fresh RSS sharing the
    /// cheap way: redistribute the 5 shares into 3 clear addends (2
    /// element-sends), input-share each addend (3 x 2 element-sends) and
    /// XOR the three sharings locally. 8 element-sends total instead of the
    /// 10 of a direct 5-sharer resharing, at the cost of one extra message
    /// leg (the addend holders wait for the redistributed shares).
    async fn reshare_via_redistribution(
        &mut self,
        z: &[RingElement<u64>],
    ) -> Result<Vec<Share5<u64>>> {
        let len = z.len();
        let mut addend = self.redistribute_words(z).await?;
        let mut out = vec![Share5::<u64>::zero(); len];
        for sharer in 0..3 {
            let values = if self.topo.party == sharer {
                Some(addend.take().expect("addend holder has a value"))
            } else {
                None
            };
            let sh = self.input_share_words(sharer, len, values).await?;
            for (out_word, sh_word) in out.iter_mut().zip(sh) {
                for (o, s) in out_word.c.iter_mut().zip(sh_word.c) {
                    *o ^= s;
                }
            }
        }
        Ok(out)
    }

    /// AND of two vectors of binary RSS shares: local cross products of the
    /// assigned component pairs, then the cheap resharing.
    async fn and_words(
        &mut self,
        x: &[Share5<u64>],
        y: &[Share5<u64>],
    ) -> Result<Vec<Share5<u64>>> {
        assert_eq!(x.len(), y.len());
        let z: Vec<RingElement<u64>> = x
            .iter()
            .zip(y.iter())
            .map(|(xs, ys)| {
                let mut acc = 0u64;
                for &(ls, lr) in &self.topo.and_pairs {
                    acc ^= xs.c[ls].0 & ys.c[lr].0;
                }
                RingElement(acc)
            })
            .collect();
        self.reshare_via_redistribution(&z).await
    }
}

/// Redistribute the 5 additive u16 shares into 3 clear adder addends (the
/// 5-party analogue of `galois_ring_to_rep3` + `two_way_split`): party 3
/// hands its share to party 0 and party 4 to party 1, so parties 0, 1 and 2
/// end up with values `v_0, v_1, v_2` with `v_0 + v_1 + v_2 = value mod
/// 2^16`.
///
/// The transferred shares are padded with two pairwise-PRF masks each
/// (`d̂_3 = d_3 + α + β`, `α` shared by {1,3}, `β` by {2,3}; symmetrically
/// `d̂_4` with `α'` shared by {0,4} and `β'` by {2,4}); the compensating
/// parties subtract the masks from their own addends. Splitting each mask
/// across two compensators means no coalition of 2 containing the recipient
/// can strip it, and every 2-coalition misses at least one addend.
pub async fn redistribute(
    sess: &mut Rss5Session,
    items: &[RingElement<u16>],
) -> Result<AdderInput> {
    let len = items.len();
    let value = match sess.topo.party {
        0 => {
            let alpha_p = sess.draw_pair_mask::<u16>(0, 4, len);
            let d3_hat = sess.recv_elems::<u16>(3, len).await?;
            Some(
                items
                    .iter()
                    .zip(d3_hat)
                    .zip(alpha_p)
                    .map(|((&d, dh), m)| d + dh - m)
                    .collect(),
            )
        }
        1 => {
            let alpha = sess.draw_pair_mask::<u16>(1, 3, len);
            let d4_hat = sess.recv_elems::<u16>(4, len).await?;
            Some(
                items
                    .iter()
                    .zip(d4_hat)
                    .zip(alpha)
                    .map(|((&d, dh), m)| d + dh - m)
                    .collect(),
            )
        }
        2 => {
            let beta = sess.draw_pair_mask::<u16>(2, 3, len);
            let beta_p = sess.draw_pair_mask::<u16>(2, 4, len);
            Some(
                items
                    .iter()
                    .zip(beta)
                    .zip(beta_p)
                    .map(|((&d, m1), m2)| d - m1 - m2)
                    .collect(),
            )
        }
        3 => {
            let alpha = sess.draw_pair_mask::<u16>(1, 3, len);
            let beta = sess.draw_pair_mask::<u16>(2, 3, len);
            let masked: Vec<RingElement<u16>> = items
                .iter()
                .zip(alpha)
                .zip(beta)
                .map(|((&d, m1), m2)| d + m1 + m2)
                .collect();
            sess.send_elems(0, masked).await?;
            None
        }
        4 => {
            let alpha_p = sess.draw_pair_mask::<u16>(0, 4, len);
            let beta_p = sess.draw_pair_mask::<u16>(2, 4, len);
            let masked: Vec<RingElement<u16>> = items
                .iter()
                .zip(alpha_p)
                .zip(beta_p)
                .map(|((&d, m1), m2)| d + m1 + m2)
                .collect();
            sess.send_elems(1, masked).await?;
            None
        }
        _ => unreachable!(),
    };
    Ok(AdderInput { len, value })
}

/// Subtract a public constant from the shared sum: party 0 subtracts it
/// from its clear addend. Purely local. The 5-party analogue of
/// [`crate::protocol::ops::sub_pub`].
pub fn sub_pub(sess: &Rss5Session, input: &mut AdderInput, rhs: RingElement<u16>) {
    if sess.topo.party == 0 {
        let value = input.value.as_mut().expect("party 0 holds an addend");
        value.iter_mut().for_each(|v| *v = *v - rhs);
    }
}

fn word_count(n: usize) -> usize {
    n.div_ceil(64)
}

/// Transpose values into 16 bit-planes; plane `b`, word `w`, bit `j` is bit
/// `b` of `vals[w * 64 + j]`. Values beyond `vals.len()` are zero-padded.
fn transpose_planes(vals: &[RingElement<u16>]) -> Vec<Vec<RingElement<u64>>> {
    let words = word_count(vals.len());
    let mut planes = vec![vec![0u64; words]; PLANES];
    for (idx, v) in vals.iter().enumerate() {
        let (w, j) = (idx / 64, idx % 64);
        for (b, plane) in planes.iter_mut().enumerate() {
            plane[w] |= (((v.0 >> b) & 1) as u64) << j;
        }
    }
    planes
        .into_iter()
        .map(|plane| plane.into_iter().map(RingElement).collect())
        .collect()
}

fn plane_to_bools(plane: &[RingElement<u64>], n: usize) -> Vec<bool> {
    (0..n)
        .map(|i| (plane[i / 64].0 >> (i % 64)) & 1 == 1)
        .collect()
}

/// Share the bit-planes of the three clear addends as binary RSS values
/// (the adder inputs). Each addend holder sends one 16-plane correction to
/// 2 peers.
pub async fn share_adder_inputs(sess: &mut Rss5Session, input: &AdderInput) -> Result<Vec<Value5>> {
    let words = word_count(input.len);
    let mut values = Vec::with_capacity(3);
    for sharer in 0..3 {
        let flat = if sess.topo.party == sharer {
            Some(
                transpose_planes(input.value().expect("addend holders have a value"))
                    .into_iter()
                    .flatten()
                    .collect(),
            )
        } else {
            None
        };
        let shared = sess.input_share_words(sharer, PLANES * words, flat).await?;
        values.push(shared.chunks(words).map(<[_]>::to_vec).collect());
    }
    Ok(values)
}

fn xor_planes(x: &Plane5, y: &Plane5) -> Plane5 {
    x.iter()
        .zip(y.iter())
        .map(|(x, y)| {
            let mut out = *x;
            for (o, y) in out.c.iter_mut().zip(y.c.iter()) {
                *o ^= *y;
            }
            out
        })
        .collect()
}

fn xor_assign_planes(x: &mut Plane5, y: &Plane5) {
    for (x, y) in x.iter_mut().zip(y.iter()) {
        for (x, y) in x.c.iter_mut().zip(y.c.iter()) {
            *x ^= *y;
        }
    }
}

/// One layer of full adders (3->2 compressors); all AND gates of the layer
/// are batched into a single communication round.
///
/// For each triple: `s = x ^ y ^ z` and, on the low 15 planes,
/// `c = ((x ^ z) & (y ^ z)) ^ z`; the returned carry value is `2c`
/// (planes `[0, c_0..c_14]`), so `x + y + z = s + 2c (mod 2^16)`.
async fn fa_layer(
    sess: &mut Rss5Session,
    triples: &[[Value5; 3]],
) -> Result<Vec<(Value5, Value5)>> {
    let words = triples[0][0][0].len();
    let mut flat_a: Vec<Share5<u64>> = Vec::with_capacity(triples.len() * (PLANES - 1) * words);
    let mut flat_b: Vec<Share5<u64>> = Vec::with_capacity(flat_a.capacity());
    let mut sums: Vec<Value5> = Vec::with_capacity(triples.len());
    for [x, y, z] in triples {
        sums.push(
            (0..PLANES)
                .map(|k| xor_planes(&xor_planes(&x[k], &y[k]), &z[k]))
                .collect(),
        );
        for k in 0..PLANES - 1 {
            flat_a.extend(xor_planes(&x[k], &z[k]));
            flat_b.extend(xor_planes(&y[k], &z[k]));
        }
    }
    let and_res = sess.and_words(&flat_a, &flat_b).await?;

    let mut out = Vec::with_capacity(triples.len());
    let mut offset = 0;
    for ([_, _, z], s) in triples.iter().zip(sums) {
        let mut carry: Value5 = Vec::with_capacity(PLANES);
        carry.push(vec![Share5::zero(); words]); // LSB of 2c is zero
        for z_k in z.iter().take(PLANES - 1) {
            let mut plane: Plane5 = and_res[offset..offset + words].to_vec();
            offset += words;
            xor_assign_planes(&mut plane, z_k);
            carry.push(plane);
        }
        out.push((s, carry));
    }
    Ok(out)
}

/// MSB of `u + v (mod 2^16)` via a ripple-carry adder, where `v` is a carry
/// value with a zero LSB plane (so the carry chain starts at bit 1).
async fn final_adder_msb(sess: &mut Rss5Session, u: &Value5, v: &Value5) -> Result<Plane5> {
    assert!(
        v[0].iter().all(|s| *s == Share5::zero()),
        "final adder expects a carry-form second operand"
    );
    let mut carry = sess.and_words(&u[1], &v[1]).await?;
    for k in 2..PLANES - 1 {
        // carry = ((u_k ^ carry) & (v_k ^ carry)) ^ carry
        let a = xor_planes(&u[k], &carry);
        let b = xor_planes(&v[k], &carry);
        let t = sess.and_words(&a, &b).await?;
        xor_assign_planes(&mut carry, &t);
    }
    let mut msb = xor_planes(&u[PLANES - 1], &v[PLANES - 1]);
    xor_assign_planes(&mut msb, &carry);
    Ok(msb)
}

/// Extract the MSB of the sum of the shared adder inputs as one RSS
/// bit-plane: a compressor tree reduces the inputs to two values (for the
/// 3 redistribution addends this is a single full adder), followed by a
/// ripple-carry adder.
pub async fn extract_msb(sess: &mut Rss5Session, mut values: Vec<Value5>) -> Result<Plane5> {
    if values.len() < 3 {
        bail!("adder needs at least 3 inputs");
    }
    // The reduction always ends with a full adder, so the last value is in
    // carry form for the final adder.
    while values.len() > 2 {
        let groups = values.len() / 3;
        let rest = values.split_off(groups * 3);
        let mut triples: Vec<[Value5; 3]> = Vec::with_capacity(groups);
        let mut it = values.into_iter();
        for _ in 0..groups {
            triples.push([it.next().unwrap(), it.next().unwrap(), it.next().unwrap()]);
        }
        let mut next_values = Vec::with_capacity(groups * 2 + rest.len());
        for (s, c) in fa_layer(sess, &triples).await? {
            next_values.push(s);
            next_values.push(c);
        }
        next_values.extend(rest);
        values = next_values;
    }

    let v = values.pop().unwrap(); // carry-form value from the last full adder
    let u = values.pop().unwrap();
    final_adder_msb(sess, &u, &v).await
}

/// Open an RSS bit-plane: every party XORs its 6 held components; each of
/// the 4 components a party misses is delivered by that component's
/// designated provider, aggregated per (provider, receiver) pair — 10
/// messages in total instead of an all-to-all broadcast.
pub async fn open_bit_plane(sess: &mut Rss5Session, plane: &Plane5, n: usize) -> Result<Vec<bool>> {
    let p = sess.topo.party;
    let mut acc: Vec<RingElement<u64>> = plane
        .iter()
        .map(|s| s.c.iter().fold(RingElement(0u64), |a, &c| a ^ c))
        .collect();
    for receiver in 0..NUM_PARTIES {
        if receiver == p {
            continue;
        }
        // components the receiver misses that this party provides
        let slots: Vec<usize> = (0..NUM_SETS)
            .filter(|&t| set_contains(t, receiver) && open_provider(t) == p)
            .map(|t| sess.topo.local_idx[t].expect("provider holds the component"))
            .collect();
        if slots.is_empty() {
            continue;
        }
        let msg: Vec<RingElement<u64>> = plane
            .iter()
            .map(|s| slots.iter().fold(RingElement(0u64), |a, &li| a ^ s.c[li]))
            .collect();
        sess.send_elems(receiver, msg).await?;
    }
    for provider in 0..NUM_PARTIES {
        if provider == p {
            continue;
        }
        let provides_any =
            (0..NUM_SETS).any(|t| set_contains(t, p) && open_provider(t) == provider);
        if provides_any {
            let v = sess.recv_elems::<u64>(provider, acc.len()).await?;
            for (a, v) in acc.iter_mut().zip(v) {
                *a ^= v;
            }
        }
    }
    Ok(plane_to_bools(&acc, n))
}

/// Compare the redistributed sum to zero and open the "less than zero"
/// bits. The 5-party analogue of
/// [`crate::protocol::ops::lt_zero_and_open_u16`].
pub async fn lt_zero_and_open_u16(sess: &mut Rss5Session, input: &AdderInput) -> Result<Vec<bool>> {
    let values = share_adder_inputs(sess, input).await?;
    let msb = extract_msb(sess, values).await?;
    open_bit_plane(sess, &msb, input.len).await
}

/// Test/debug helper: reconstruct binary RSS-shared values from all parties'
/// shares, checking that replicated components agree.
pub fn reconstruct_rss<T: IntRing2k>(per_party: &[Vec<Share5<T>>]) -> Vec<T> {
    assert_eq!(per_party.len(), NUM_PARTIES);
    let topos: Vec<Topology> = (0..NUM_PARTIES).map(Topology::new).collect();
    let len = per_party[0].len();
    (0..len)
        .map(|k| {
            let mut acc = RingElement(T::default());
            for t in 0..NUM_SETS {
                let hs = holders(t);
                let component = per_party[hs[0]][k].c[topos[hs[0]].local_idx[t].unwrap()];
                for &h in &hs[1..] {
                    assert_eq!(
                        per_party[h][k].c[topos[h].local_idx[t].unwrap()],
                        component,
                        "replicated component {t} disagrees between holders"
                    );
                }
                acc ^= component;
            }
            acc.0
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::{
        player::{Identity, RoleAssignment},
        session::{NetworkSession, SessionId},
    };
    use crate::network::mpc::LocalNetworkingStore;
    use rand::Rng;
    use std::sync::Arc;

    fn make_network_sessions() -> Vec<NetworkSession> {
        let identities: Vec<Identity> = (0..NUM_PARTIES)
            .map(|i| Identity::from(format!("party{i}")))
            .collect();
        let role_assignments: RoleAssignment = identities
            .iter()
            .enumerate()
            .map(|(index, id)| (Role::new(index), id.clone()))
            .collect();
        let role_assignments = Arc::new(role_assignments);
        let store = LocalNetworkingStore::from_host_ids(&identities);
        identities
            .iter()
            .enumerate()
            .map(|(i, id)| NetworkSession {
                session_id: SessionId(0),
                role_assignments: Arc::clone(&role_assignments),
                networking: Box::new(store.get_local_network(id.clone())),
                own_role: Role::new(i),
            })
            .collect()
    }

    async fn make_sessions() -> Vec<Rss5Session> {
        let mut handles = Vec::new();
        for (i, ns) in make_network_sessions().into_iter().enumerate() {
            handles.push(tokio::spawn(async move {
                setup_rss5_session(ns, [i as u8 + 1; 16]).await.unwrap()
            }));
        }
        let mut sessions = Vec::new();
        for h in handles {
            sessions.push(h.await.unwrap());
        }
        sessions
    }

    /// Random additive 5-sharing of the given values, one vector per party.
    fn additive_sharing(values: &[u16]) -> Vec<Vec<RingElement<u16>>> {
        let mut rng = rand::thread_rng();
        let mut parts: Vec<Vec<RingElement<u16>>> = vec![Vec::new(); NUM_PARTIES];
        for &v in values {
            let r: [u16; 4] = rng.gen();
            let last = r.iter().fold(v, |acc, x| acc.wrapping_sub(*x));
            for (p, part) in parts.iter_mut().enumerate() {
                part.push(RingElement(if p < 4 { r[p] } else { last }));
            }
        }
        parts
    }

    /// Reconstruct the u16 values of a bit-sliced shared adder input.
    fn reconstruct_value(per_party: &[Value5], n: usize) -> Vec<u16> {
        let mut out = vec![0u16; n];
        for k in 0..PLANES {
            let plane_shares: Vec<Vec<Share5<u64>>> =
                per_party.iter().map(|v| v[k].clone()).collect();
            let words = reconstruct_rss(&plane_shares);
            for (i, out) in out.iter_mut().enumerate() {
                let bit = (words[i / 64] >> (i % 64)) & 1;
                *out |= (bit as u16) << k;
            }
        }
        out
    }

    #[test]
    fn topology_covers_all_products() {
        // every ordered component pair is assigned to exactly one party
        let topos: Vec<Topology> = (0..NUM_PARTIES).map(Topology::new).collect();
        let total: usize = topos.iter().map(|t| t.and_pairs.len()).sum();
        assert_eq!(total, NUM_SETS * NUM_SETS);
        // every open provider holds the component it provides, and every
        // party receives all 4 components it misses
        for t in 0..NUM_SETS {
            assert!(!set_contains(t, open_provider(t)));
        }
        for p in 0..NUM_PARTIES {
            let received: usize = (0..NUM_SETS)
                .filter(|&t| set_contains(t, p) && open_provider(t) != p)
                .count();
            assert_eq!(received, NUM_PARTIES - 1);
        }
    }

    #[tokio::test]
    async fn redistribute_and_share_reconstructs() {
        const N: usize = 100;
        let values: Vec<u16> = (0..N).map(|_| rand::thread_rng().gen()).collect();
        let parts = additive_sharing(&values);
        // The three redistributed addends must sum to the original values,
        // and their shared bit-planes must reconstruct to the same addends.
        let mut handles = Vec::new();
        for (i, sess) in make_sessions().await.into_iter().enumerate() {
            let z = parts[i].clone();
            handles.push(tokio::spawn(async move {
                let mut sess = sess;
                let input = redistribute(&mut sess, &z).await.unwrap();
                let addend: Vec<u16> = input
                    .value()
                    .map(|v| v.iter().map(|x| x.0).collect())
                    .unwrap_or_default();
                let shared = share_adder_inputs(&mut sess, &input).await.unwrap();
                (addend, shared)
            }));
        }
        let mut outputs = Vec::new();
        for h in handles {
            outputs.push(h.await.unwrap());
        }
        let mut sum = vec![0u16; N];
        for (p, (addend, _)) in outputs.iter().enumerate() {
            if p < 3 {
                for (s, &a) in sum.iter_mut().zip(addend.iter()) {
                    *s = s.wrapping_add(a);
                }
            } else {
                assert!(addend.is_empty());
            }
        }
        assert_eq!(sum, values);
        // reconstruct each shared adder input and compare to the clear addend
        for j in 0..3 {
            let per_party: Vec<Value5> = outputs.iter().map(|(_, s)| s[j].clone()).collect();
            let reconstructed = reconstruct_value(&per_party, N);
            assert_eq!(reconstructed, outputs[j].0);
        }
    }

    #[tokio::test]
    async fn and_words_correct() {
        const N: usize = 50;
        let mut rng = rand::thread_rng();
        let xs: Vec<u64> = (0..N).map(|_| rng.gen()).collect();
        let ys: Vec<u64> = (0..N).map(|_| rng.gen()).collect();
        // XOR-additive 5-sharings of x and y as u64 words
        let mut share_words = |vals: &[u64]| -> Vec<Vec<RingElement<u64>>> {
            let mut parts: Vec<Vec<RingElement<u64>>> = vec![Vec::new(); NUM_PARTIES];
            for &v in vals {
                let r: [u64; 4] = rng.gen();
                let last = r.iter().fold(v, |acc, x| acc ^ x);
                for (p, part) in parts.iter_mut().enumerate() {
                    part.push(RingElement(if p < 4 { r[p] } else { last }));
                }
            }
            parts
        };
        let px = share_words(&xs);
        let py = share_words(&ys);
        let mut handles = Vec::new();
        for (i, sess) in make_sessions().await.into_iter().enumerate() {
            let (zx, zy) = (px[i].clone(), py[i].clone());
            handles.push(tokio::spawn(async move {
                let mut sess = sess;
                let x = sess.reshare_via_redistribution(&zx).await.unwrap();
                let y = sess.reshare_via_redistribution(&zy).await.unwrap();
                sess.and_words(&x, &y).await.unwrap()
            }));
        }
        let mut shares = Vec::new();
        for h in handles {
            shares.push(h.await.unwrap());
        }
        let expected: Vec<u64> = xs.iter().zip(ys.iter()).map(|(x, y)| x & y).collect();
        assert_eq!(reconstruct_rss(&shares), expected);
    }

    #[tokio::test]
    async fn open_matches_reconstruction() {
        const N: usize = 130; // not a multiple of 64 to exercise padding
        let mut rng = rand::thread_rng();
        let words = N.div_ceil(64);
        let xs: Vec<u64> = (0..words).map(|_| rng.gen()).collect();
        let mut parts: Vec<Vec<RingElement<u64>>> = vec![Vec::new(); NUM_PARTIES];
        for &v in &xs {
            let r: [u64; 4] = rng.gen();
            let last = r.iter().fold(v, |acc, x| acc ^ x);
            for (p, part) in parts.iter_mut().enumerate() {
                part.push(RingElement(if p < 4 { r[p] } else { last }));
            }
        }
        let mut handles = Vec::new();
        for (i, sess) in make_sessions().await.into_iter().enumerate() {
            let z = parts[i].clone();
            handles.push(tokio::spawn(async move {
                let mut sess = sess;
                let plane = sess.reshare_via_redistribution(&z).await.unwrap();
                open_bit_plane(&mut sess, &plane, N).await.unwrap()
            }));
        }
        let expected: Vec<bool> = (0..N).map(|i| (xs[i / 64] >> (i % 64)) & 1 == 1).collect();
        for h in handles {
            assert_eq!(h.await.unwrap(), expected);
        }
    }

    #[tokio::test]
    async fn lt_threshold_pipeline() {
        const N: usize = 200;
        const THRESHOLD: u16 = 1000;
        let mut rng = rand::thread_rng();
        let values: Vec<u16> = (0..N).map(|_| rng.gen::<i16>() as u16).collect();
        let parts = additive_sharing(&values);
        let mut handles = Vec::new();
        for (i, sess) in make_sessions().await.into_iter().enumerate() {
            let z = parts[i].clone();
            handles.push(tokio::spawn(async move {
                let mut sess = sess;
                let mut input = redistribute(&mut sess, &z).await.unwrap();
                sub_pub(&sess, &mut input, RingElement(THRESHOLD));
                lt_zero_and_open_u16(&mut sess, &input).await.unwrap()
            }));
        }
        let expected: Vec<bool> = values
            .iter()
            .map(|&v| (v.wrapping_sub(THRESHOLD) as i16) < 0)
            .collect();
        for h in handles {
            assert_eq!(h.await.unwrap(), expected);
        }
    }
}
