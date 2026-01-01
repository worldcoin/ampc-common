use crate::fast_metrics::FastHistogram;
use crate::{
    execution::session::{Session, SessionHandles},
    network::value::{NetworkInt, NetworkValue},
};
use ampc_secret_sharing::shares::vecshare_bittranspose::Transpose64;
use ampc_secret_sharing::shares::{
    bit::Bit,
    int_ring::IntRing2k,
    ring_impl::{RingElement, VecRingElement},
    share::Share,
    vecshare::{SliceShare, VecShare},
};
use eyre::{bail, eyre, Error, Result};
use itertools::{izip, repeat_n, Itertools};
use num_traits::Zero;
use rand::{distributions::Standard, prelude::Distribution, Rng};
use std::{cell::RefCell, ops::SubAssign};
use tracing::{instrument, trace_span, Instrument};

thread_local! {
    static ROUNDS_METRICS: RefCell<FastHistogram> = RefCell::new(
        FastHistogram::new("smpc.rounds")
    );
}

struct VecBinShare<T: IntRing2k> {
    inner: VecShare<T>,
}

impl<T: IntRing2k> VecBinShare<T> {
    fn from_ab(a: Vec<RingElement<T>>, b: Vec<RingElement<T>>) -> Self {
        Self {
            inner: VecShare::from_ab(a, b),
        }
    }

    #[allow(dead_code)]
    fn len(&self) -> usize {
        self.inner.len()
    }

    #[allow(dead_code)]
    fn get_word_at(&self, index: usize) -> Share<T> {
        self.inner.get_at(index)
    }
}

/// Splits the components of the given arithmetic share into 3 secret shares as described in Section 5.3 of the ABY3 paper.
///
/// The parties own the following arithmetic shares of x = x1 + x2 + x3:
///
/// |share component|Party 0 | Party 1 | Party 2 |
/// |---------------|--------|---------|---------|
/// |a              |x1      |x2       |x3       |
/// |b              |x3      |x1       |x2       |
///
/// The function returns the shares in the following order:
///
/// shares of x1
/// |share component|Party 0 | Party 1 | Party 2 |
/// |---------------|--------|---------|---------|
/// |a              |x1      |0        |0        |
/// |b              |0       |x1       |0        |
///
/// shares of x2
/// |share component|Party 0 | Party 1 | Party 2 |
/// |---------------|--------|---------|---------|
/// |a              |0       |x2       |0        |
/// |b              |0       |0        |x2       |
///
/// shares of x3
/// |share component|Party 0 | Party 1 | Party 2 |
/// |---------------|--------|---------|---------|
/// |a              |0       |0        |x3       |
/// |b              |x3      |0        |0        |
fn a2b_pre<T: IntRing2k>(session: &Session, x: Share<T>) -> Result<(Share<T>, Share<T>, Share<T>)> {
    let (a, b) = x.get_ab();

    let mut x1 = Share::zero();
    let mut x2 = Share::zero();
    let mut x3 = Share::zero();

    match session.own_role().index() {
        0 => {
            x1.a = a;
            x3.b = b;
        }
        1 => {
            x2.a = a;
            x1.b = b;
        }
        2 => {
            x3.a = a;
            x2.b = b;
        }
        _ => {
            bail!("Cannot deal with roles that have index outside of the set [0, 1, 2]")
        }
    }
    Ok((x1, x2, x3))
}

/// Computes in place binary XOR of two vectors of bit-sliced shares.
fn transposed_pack_xor_assign<T: IntRing2k>(x1: &mut [VecShare<T>], x2: &[VecShare<T>]) {
    let len = x1.len();
    debug_assert_eq!(len, x2.len());

    for (x1, x2) in x1.iter_mut().zip(x2.iter()) {
        *x1 ^= x2.as_slice();
    }
}

/// Computes binary XOR of two vectors of bit-sliced shares.
fn transposed_pack_xor<T: IntRing2k>(x1: &[VecShare<T>], x2: &[VecShare<T>]) -> Vec<VecShare<T>> {
    let len = x1.len();
    debug_assert_eq!(len, x2.len());

    let mut res = Vec::with_capacity(len);
    for (x1, x2) in x1.iter().zip(x2.iter()) {
        res.push(x1.as_slice() ^ x2.as_slice());
    }
    res
}

/// Computes and sends a local share of the AND of two vectors of bit-sliced shares.
async fn and_many_iter_send<T: IntRing2k + NetworkInt>(
    session: &mut Session,
    a: impl Iterator<Item = Share<T>>,
    b: impl Iterator<Item = Share<T>>,
    size_hint: usize,
) -> Result<Vec<RingElement<T>>, Error>
where
    Standard: Distribution<T>,
{
    // Caller should ensure that size_hint == a.len() == b.len()
    let mut shares_a = VecRingElement::with_capacity(size_hint);
    for (a_, b_) in a.zip(b) {
        let rand = session.prf.gen_binary_zero_share::<T>();
        let mut c = &a_ & &b_;
        c ^= rand;
        shares_a.push(c);
    }

    #[cfg(feature = "networking_metrics")]
    {
        let num_ring_elements = shares_a.len() as u64;
        metrics::counter!("network.num_AND_ops").increment(num_ring_elements);
    }

    let network = &mut session.network_session;
    network.send_ring_vec_next(&shares_a).await?;
    Ok(shares_a.0)
}

async fn and_many_send<T: IntRing2k + NetworkInt>(
    session: &mut Session,
    a: SliceShare<'_, T>,
    b: SliceShare<'_, T>,
) -> Result<Vec<RingElement<T>>, Error>
where
    Standard: Distribution<T>,
{
    if a.len() != b.len() {
        bail!("InvalidSize in and_many_send");
    }
    let mut shares_a = VecRingElement::with_capacity(a.len());
    for (a_, b_) in a.iter().zip(b.iter()) {
        let rand = session.prf.gen_binary_zero_share::<T>();
        let mut c = a_ & b_;
        c ^= rand;
        shares_a.push(c);
    }

    #[cfg(feature = "networking_metrics")]
    {
        let num_ring_elements = shares_a.len() as u64;
        metrics::counter!("network.num_AND_ops").increment(num_ring_elements);
    }

    let network = &mut session.network_session;
    network.send_ring_vec_next(&shares_a).await?;
    Ok(shares_a.0)
}

/// Receives a share of the AND of two vectors of bit-sliced shares.
async fn and_many_receive<T: IntRing2k + NetworkInt>(
    session: &mut Session,
) -> Result<Vec<RingElement<T>>, Error> {
    let shares_b = {
        let other_share = session.network_session.receive_prev().await;

        match other_share {
            Ok(v) => T::into_vec(v),
            Err(e) => Err(eyre!("Error in and_many_receive: {e}")),
        }
    }?;
    Ok(shares_b)
}

/// Low-level SMPC protocol to compute the AND of two vectors of bit-sliced shares.
#[allow(dead_code)]
async fn and_many<T: IntRing2k + NetworkInt>(
    session: &mut Session,
    a: SliceShare<'_, T>,
    b: SliceShare<'_, T>,
) -> Result<VecShare<T>, Error>
where
    Standard: Distribution<T>,
{
    let shares_a = and_many_send(session, a, b).await?;
    let shares_b = and_many_receive(session).await?;
    let complete_shares = VecShare::from_ab(shares_a, shares_b);
    Ok(complete_shares)
}

/// Reduce the given vector of bit-vector shares by computing their element-wise AND.
///
/// Each vector in `v` is expected to have `len` bits.
#[allow(dead_code)]
pub async fn and_product(
    session: &mut Session,
    v: Vec<VecShare<Bit>>,
    len: usize,
) -> Result<VecShare<Bit>, Error> {
    if v.is_empty() {
        bail!("Input vector is empty");
    }
    for vec_share in &v {
        if vec_share.len() != len {
            bail!("Input vector shares have different lengths");
        }
    }

    let mut res = v;
    while res.len() > 1 {
        // if the length is odd, we save the last column to add it back later
        let maybe_last_column = if res.len() % 2 == 1 { res.pop() } else { None };
        let half_len = res.len() / 2;
        let left_bits: VecShare<u64> =
            VecShare::new_vec(res.drain(..half_len).flatten().collect_vec()).pack();
        let right_bits: VecShare<u64> =
            VecShare::new_vec(res.drain(..).flatten().collect_vec()).pack();
        let and_bits = and_many(session, left_bits.as_slice(), right_bits.as_slice()).await?;
        let mut and_bits = and_bits.convert_to_bits();
        let num_and_bits = half_len * len;
        and_bits.truncate(num_and_bits);
        res = and_bits
            .inner()
            .chunks(len)
            .map(|chunk| VecShare::new_vec(chunk.to_vec()))
            .collect_vec();
        res.extend(maybe_last_column);
    }
    res.pop().ok_or(eyre!("Not enough elements"))
}

/// Computes binary AND of two vectors of bit-sliced shares.
#[instrument(level = "trace", target = "searcher::network", skip(session, x1, x2))]
async fn transposed_pack_and<T: IntRing2k + NetworkInt>(
    session: &mut Session,
    x1: Vec<VecShare<T>>,
    x2: Vec<VecShare<T>>,
) -> Result<Vec<VecShare<T>>, Error>
where
    Standard: Distribution<T>,
{
    if x1.len() != x2.len() {
        bail!("Inputs have different length {} {}", x1.len(), x2.len());
    }

    let chunk_sizes = x1.iter().map(VecShare::len).collect::<Vec<_>>();
    for (chunk_size1, chunk_size2) in izip!(chunk_sizes.iter(), x2.iter().map(VecShare::len)) {
        if *chunk_size1 != chunk_size2 {
            bail!("VecShare lengths are not equal");
        }
    }

    let flattened_len: usize = chunk_sizes.iter().sum();
    let x1 = VecShare::flatten(x1);
    let x2 = VecShare::flatten(x2);
    let shares_a = and_many_iter_send(session, x1, x2, flattened_len).await?;
    let shares_b = and_many_receive(session).await?;

    // Unflatten the shares vectors
    let mut res = Vec::with_capacity(chunk_sizes.len());
    let mut offset = 0;
    for l in chunk_sizes {
        let a = shares_a[offset..offset + l].iter().copied();
        let b = shares_b[offset..offset + l].iter().copied();
        res.push(VecShare::from_iter_ab(a, b));
        offset += l;
    }
    Ok(res)
}

/// Computes the sum of three integers using the binary ripple-carry adder and return two resulting overflow carries.
/// Input integers are given in binary form.
#[allow(dead_code)]
#[instrument(level = "trace", target = "searcher::network", skip_all)]
async fn binary_add_3_get_two_carries<T: IntRing2k + NetworkInt>(
    session: &mut Session,
    x1: Vec<VecShare<T>>,
    x2: Vec<VecShare<T>>,
    x3: Vec<VecShare<T>>,
    truncate_len: usize,
) -> Result<(VecShare<Bit>, VecShare<Bit>), Error>
where
    Standard: Distribution<T>,
{
    let len = x1.len();
    if len != x2.len() || len != x3.len() {
        bail!(
            "Inputs have different length {} {} {}",
            len,
            x2.len(),
            x3.len()
        );
    };

    if len < 16 {
        bail!("Input length should be at least 16: {len}");
    }

    // Let x1, x2, x3 are integers modulo 2^k.
    //
    // Full adder where x3 plays the role of an input carry yields
    // c = (x1 AND x2) XOR (x3 AND (x1 XOR x2)) and
    // s = x1 XOR x2 XOR x3
    // Note that x1 + x2 + x3 = 2 * c + s mod 2^k

    let mut x2x3 = x2;
    transposed_pack_xor_assign(&mut x2x3, &x3);
    // x1 XOR x2 XOR x3
    let mut s = transposed_pack_xor(&x1, &x2x3);
    let mut x1x3 = x1;
    transposed_pack_xor_assign(&mut x1x3, &x3);
    // (x1 XOR x3) AND (x2 XOR x3) = (x1 AND x2) XOR (x3 AND (x1 XOR x2)) XOR x3
    let mut c = transposed_pack_and(session, x1x3, x2x3).await?;
    // (x1 AND x2) XOR (x3 AND (x1 XOR x2))
    transposed_pack_xor_assign(&mut c, &x3);

    // Find the MSB of 2 * c + s using the parallel prefix adder
    let and_many_span = trace_span!(target: "searcher::network", "and_many_calls", n = c.len());

    // First full adder (carry is 0)
    // The LSB of 2 * c is zero, so we can ignore the LSB of s
    let mut carry = and_many(session, s[1].as_slice(), c[0].as_slice())
        .instrument(and_many_span.clone())
        .await?;

    // Keep the MSB of c to compute the carries
    let mut c_msb = c.pop().ok_or(eyre!("Not enough elements"))?;

    // Compute carry of the sum of 2*c without MSB and s
    for (s_, c_) in s.iter_mut().skip(2).zip(c.iter_mut().skip(1)) {
        *s_ ^= carry.as_slice();
        *c_ ^= carry.as_slice();
        let tmp_c = and_many(session, s_.as_slice(), c_.as_slice())
            .instrument(and_many_span.clone())
            .await?;
        carry ^= tmp_c;
    }

    // Top carry
    let res2 = and_many(session, c_msb.as_slice(), carry.as_slice())
        .instrument(and_many_span)
        .await?;
    // Carry next to top
    c_msb ^= carry;

    // Extract bits for outputs
    let mut res1 = c_msb.convert_to_bits();
    res1.truncate(truncate_len);
    let mut res2 = res2.convert_to_bits();
    res2.truncate(truncate_len);

    Ok((res1, res2))
}

/// Conducts a 3 party protocol to inject bits into shares of type T.
/// The protocol is given in <https://eprint.iacr.org/2025/919.pdf>, see Section 6.2 and Protocol 22.
///
/// Protocol description:
/// - At the start of the protocol, each party holds a share of each input bit.
///   In particular, for each input bit b party P_i holds shares b_i, b_{i-1} (indices modulo 3)
///   such that b = b_0 XOR b_1 XOR b_2.
/// - The protocol runs in 3 rounds of communication trying to compute arithmetic shares of b as
///
///   b_0 XOR b_1 XOR b_2 = b_0 XOR b_1 + b_2 - 2 * (b_0 XOR b_1 ) * b_2,
///
///   where computations are done modulo 2^k for T being k-bit integer type.
///
/// Round 1: share b_0 XOR b_1
///     1. Parties 0 and 1 generate random mask r_01 using their shared PRF.
///     2. Party 1 sends x = (b_0 XOR b_1) - r_01 to Party 2.
///     As a result, parties share b_0 XOR b_1 = r_01 + x + 0 as follows:
///     - Party 0 holds shares (r_01, 0),
///     - Party 1 holds shares (x, r_01),
///     - Party 2 holds shares (0, x).
///
/// The following two rounds compute the product
/// (b_0 XOR b_1 ) * b_2 = (x + r_01) * b_2 = (x * b_2) + (r_01 * b_2).
/// These terms can be locally computed and shared by Party 2 and Party 0, respectively.
///
/// Round 2: share r_01 * b_2
///     1. Parties 0 and 2 generate random mask r_02 using their shared PRF.
///     2. Party 0 computes y = (r_01 * b_2) - r_02 and sends it to Party 1.
///     As a result, parties share r_01 * b_2 = r_02 + 0 + y as follows:
///     - Party 0 holds shares (y, r_02),
///     - Party 1 holds shares (0, y),
///     - Party 2 holds shares (r_02, 0).
///
/// Round 3: share x * b_2
///     1. Parties 1 and 2 generate random mask r_12 using their shared PRF.
///     2. Party 2 computes z = (x * b_2) - r_12 and sends it to Party 0.
///     As a result, parties share x * b_2 = z + 0 + r_12 as follows:
///     - Party 0 holds shares (0, z),
///     - Party 1 holds shares (r_12, 0),
///     - Party 2 holds shares (z, r_12).
///
/// The final arithmetic shares of b are computed locally by each party as
///
/// [b_0 XOR b_1 XOR b_2] = [b_0 XOR b_1] + [b_2] - 2 * [(b_0 XOR b_1 ) * b_2]
/// = [b_0 XOR b_1] + [b_2] - 2 * ([r_01 * b_2] + [x * b_2]).
///
/// The shares of [b_2] are known to each party at the start of the protocol as follows:
/// - Party 0 holds shares (0, b_2),
/// - Party 1 holds shares (0, 0),
/// - Party 2 holds shares (b_2, 0).
///
/// Rounds 1 and 2 can be computed in parallel.
/// The resulting communication complexity is 2 rounds with each party sending 1 element of T per input bit.
pub async fn bit_inject<T: IntRing2k + NetworkInt>(
    session: &mut Session,
    input: VecShare<Bit>,
) -> Result<VecShare<T>, Error>
where
    Standard: Distribution<T>,
    [T]: Fill,
{
    let role_index = (session.own_role().index() + session.session_id().0 as usize) % 3;
    let res = match role_index {
        0 => bit_inject_party0::<T>(session, input).await?,
        1 => bit_inject_party1::<T>(session, input).await?,
        2 => bit_inject_party2::<T>(session, input).await?,
        _ => {
            bail!("Cannot deal with roles outside of the set [0, 1, 2] in bit_inject_ot")
        }
    };
    Ok(res)
}

/// Returns an iterator that yields `len` zero RingElements of type T.
fn zero_iter<T: IntRing2k>(len: usize) -> impl Iterator<Item = RingElement<T>> {
    repeat_n(RingElement::<T>::zero(), len)
}

/// Implementation of Party 0 in the bit-injection protocol description above.
///
/// Rounds 1 and 2 (computed in parallel):
/// 1. Party 0 generates random masks r_01 and r_02 using their shared PRFs with Party 1 and Party 2, respectively.
/// 2. Party 0 sends y = (r_01 * b_2) - r_02 it to Party 1.
///
/// Round 3:
/// 1. Party 0 receives z from Party 2.
///
/// By the end of Round 3, Party 0 holds the following shares:
/// - s1 = (r_01, 0) of [b_0 XOR b_1]
/// - s2 = (0, b_2) of [b_2]
/// - s3 = (y, r_02) of [r_01 * b_2]
/// - s4 = (0, z) of [x * b_2]
///
/// Local computation of the final shares:
///
/// [b_0 XOR b_1 XOR b_2] = [b_0 XOR b_1] + [b_2] - 2 * [(b_0 XOR b_1 ) * b_2]
/// = [b_0 XOR b_1] + [b_2] - 2 * ([r_01 * b_2] + [x * b_2])
/// = s1 + s2 - 2 * (s3 + s4)
async fn bit_inject_party0<T: IntRing2k + NetworkInt>(
    session: &mut Session,
    input: VecShare<Bit>,
) -> Result<VecShare<T>, Error>
where
    Standard: Distribution<T>,
{
    let len = input.len();
    let prf = &mut session.prf;

    // Prepare b2 shares
    let b2: VecRingElement<T> = input
        .iter()
        .map(|bit_share| {
            // b2 is the share of b owned by Party 0 at the start of the protocol
            // Party 0 holds shares (0, b_2)
            if bit_share.b.convert().into() {
                RingElement(T::one())
            } else {
                RingElement(T::zero())
            }
        })
        .collect();

    // Rounds 1 and 2 (computed in parallel):
    // 1. Party 0 generates random masks r_01 and r_02 using their shared PRFs with Party 1 and Party 2, respectively.
    let r_01: VecRingElement<T> = (0..len)
        .map(|_| prf.get_my_prf().gen::<RingElement<T>>())
        .collect();
    let r_02: VecRingElement<T> = (0..len)
        .map(|_| prf.get_prev_prf().gen::<RingElement<T>>())
        .collect();

    // 2. Party 0 computes y = (r_01 * b_2) - r_02 and sends it to Party 1.
    let y = ((r_01.clone() * &b2)? - &r_02)?;
    let network = &mut session.network_session;
    network.send_ring_vec_next(&y).await?;

    // Round 3:
    // 1. Party 0 receives z from Party 2.
    let z: VecRingElement<T> = network.receive_ring_vec_prev().await?;

    // Pack shares
    // By the end of Round 3, Party 0 holds the following shares:
    // - s1 = (r_01, 0) of [b_0 XOR b_1]
    let s1 = Share::iter_from_iter_ab(r_01.into_iter(), zero_iter(len));
    // - s2 = (0, b_2) of [b_2]
    let s2 = Share::iter_from_iter_ab(zero_iter(len), b2.into_iter());
    // - s3 = (y, r_02) of [r_01 * b_2]
    let s3 = Share::iter_from_iter_ab(y.into_iter(), r_02.into_iter());
    // - s4 = (0, z) of [x * b_2]
    let s4 = Share::iter_from_iter_ab(zero_iter(len), z.into_iter());
    // Local computation of the final shares:
    //
    // [b_0 XOR b_1 XOR b_2] = [b_0 XOR b_1] + [b_2] - 2 * [(b_0 XOR b_1 ) * b_2]
    // = [b_0 XOR b_1] + [b_2] - 2 * ([r_01 * b_2] + [x * b_2])
    // = s1 + s2 - 2 * (s3 + s4)
    Ok(VecShare::new_vec(
        izip!(s1, s2, s3, s4)
            .map(|(s1, s2, s3, s4)| {
                let sum12 = s1 + s2;
                let sum34 = s3 + s4;
                sum12 - sum34 - sum34
            })
            .collect_vec(),
    ))
}

/// Implementation of Party 1 in the bit-injection protocol description above.
///
/// Rounds 1 and 2 (computed in parallel):
/// 1. Party 1 generates a random mask r_01 using their shared PRF with Party 0.
/// 2. Party 1 sends x = (b_0 XOR b_1) - r_01 to Party 2.
/// 3. Party 1 receives y from Party 0.
///
/// Round 3:
/// 1. Party 1 generates a random mask r_12 using their shared PRF with Party 2.
///
/// By the end of Round 3, Party 1 holds the following shares:
/// - s1 = (x, r_01) of [b_0 XOR b_1]
/// - s2 = (0, 0) of [b_2] (we can ignore this shares as they are zero)
/// - s3 = (0, y) of [r_01 * b_2]
/// - s4 = (r_12, 0) of [x * b_2]
///
/// Local computation of the final shares:
///
/// [b_0 XOR b_1 XOR b_2] = [b_0 XOR b_1] + [b_2] - 2 * [(b_0 XOR b_1 ) * b_2]
/// = [b_0 XOR b_1] + [b_2] - 2 * ([r_01 * b_2] + [x * b_2])
/// = s1 - 2 * (s3 + s4)
async fn bit_inject_party1<T: IntRing2k + NetworkInt>(
    session: &mut Session,
    input: VecShare<Bit>,
) -> Result<VecShare<T>, Error>
where
    Standard: Distribution<T>,
{
    let len = input.len();
    let prf = &mut session.prf;

    //Rounds 1 and 2 (computed in parallel):
    // 1. Party 1 generates a random mask r_01 using their shared PRF with Party 0.
    let r_01: VecRingElement<T> = (0..len)
        .map(|_| prf.get_prev_prf().gen::<RingElement<T>>())
        .collect();

    // 2. Party 1 sends x = (b_0 XOR b_1) - r_01 to Party 2.
    let x: VecRingElement<T> = izip!(input, r_01.0.iter())
        .map(|(bit_share, r)| {
            let b0_xor_b1 = T::from((bit_share.a ^ bit_share.b).convert().convert());
            RingElement(b0_xor_b1) - r
        })
        .collect();
    let network = &mut session.network_session;
    network.send_ring_vec_next(&x).await?;

    // 3. Party 1 receives y from Party 0.
    let y: VecRingElement<T> = network.receive_ring_vec_prev().await?;

    // Round 3:
    // 1. Party 1 generates a random mask r_12 using their shared PRF with Party 2.
    let r_12 = (0..len).map(|_| prf.get_my_prf().gen::<RingElement<T>>());

    // Pack shares
    // By the end of Round 3, Party 1 holds the following shares:
    // - s1 = (x, r_01) of [b_0 XOR b_1]
    let s1 = Share::iter_from_iter_ab(x.into_iter(), r_01.into_iter());
    // - s2 = (0, 0) of [b_2] (we can ignore this shares as they are zero)
    // - s3 = (0, y) of [r_01 * b_2]
    let s3 = Share::iter_from_iter_ab(zero_iter(len), y.into_iter());
    // - s4 = (r_12, 0) of [x * b_2]
    let s4 = Share::iter_from_iter_ab(r_12, zero_iter(len));

    // Local computation of the final shares:
    // [b_0 XOR b_1 XOR b_2] = [b_0 XOR b_1] + [b_2] - 2 * [(b_0 XOR b_1 ) * b_2]
    // = [b_0 XOR b_1] + [b_2] - 2 * ([r_01 * b_2] + [x * b_2])
    // = s1 - 2 * (s3 + s4)
    Ok(VecShare::new_vec(
        izip!(s1, s3, s4)
            .map(|(s1, s3, s4)| {
                let sum34 = s3 + s4;
                s1 - sum34 - sum34
            })
            .collect_vec(),
    ))
}

/// Implementation of Party 2 in the bit-injection protocol description above.
///
/// Rounds 1 and 2 (computed in parallel):
///     1. Party 2 receives x from Party 1.
///     2. Party 2 generates a random mask r_02 using their shared PRF with Party 0.
///
/// Round 3:
///     1. Party 2 generates a random mask r_12 using their shared PRF with Party 1.
///     2. Party 2 sends z = (x * b_2) - r_12 to Party 0.
///
/// By the end of Round 3, Party 2 holds the following shares:
/// - s1 = (0, x) of [b_0 XOR b_1]
/// - s2 = (b_2, 0) of [b_2] (we can ignore this shares as they are zero)
/// - s3 = (r_02, 0) of [r_01 * b_2]
/// - s4 = (z, r_12) of [x * b_2]
///
/// Local computation of the final shares:
///
/// [b_0 XOR b_1 XOR b_2] = [b_0 XOR b_1] + [b_2] - 2 * [(b_0 XOR b_1 ) * b_2]
/// = [b_0 XOR b_1] + [b_2] - 2 * ([r_01 * b_2] + [x * b_2])
/// = s1 + s2 - 2 * (s3 + s4)
async fn bit_inject_party2<T: IntRing2k + NetworkInt>(
    session: &mut Session,
    input: VecShare<Bit>,
) -> Result<VecShare<T>, Error>
where
    Standard: Distribution<T>,
{
    let len = input.len();
    let prf = &mut session.prf;
    // Rounds 1 and 2 (computed in parallel):
    // 1. Party 2 receives x from Party 1.
    let network = &mut session.network_session;
    let x: VecRingElement<T> = network.receive_ring_vec_prev().await?;

    // 2. Party 2 generates a random mask r_02 using their shared PRF with Party 0.
    let my_prf = &mut prf.my_prf;
    let r_02 = (0..len).map(|_| my_prf.gen::<RingElement<T>>());

    // Round 3:
    // 1. Party 2 generates a random mask r_12 using their shared PRF with Party 1.
    let prev_prf = &mut prf.prev_prf;
    let r_12: VecRingElement<T> = (0..len).map(|_| prev_prf.gen::<RingElement<T>>()).collect();

    // 2. Party 2 sends z = (x * b_2) - r_12 to Party 0.
    let b2: VecRingElement<T> = input
        .iter()
        .map(|bit_share| {
            // b2 is the share of b owned by Party 2 at the start of the protocol
            // Party 2 holds shares (b_2, 0)
            if bit_share.a.convert().into() {
                RingElement(T::one())
            } else {
                RingElement(T::zero())
            }
        })
        .collect();
    let z = ((x.clone() * &b2)? - &r_12)?;
    network.send_ring_vec_next(&z).await?;

    // By the end of Round 3, Party 2 holds the following shares:
    // - s1 = (0, x) of [b_0 XOR b_1]
    let s1 = Share::iter_from_iter_ab(zero_iter(len), x.into_iter());
    // - s2 = (b_2, 0) of [b_2] (we can ignore this shares as they are zero)
    let s2 = Share::iter_from_iter_ab(b2.into_iter(), zero_iter(len));
    // - s3 = (r_02, 0) of [r_01 * b_2]
    let s3 = Share::iter_from_iter_ab(r_02, zero_iter(len));
    // - s4 = (z, r_12) of [x * b_2]
    let s4 = Share::iter_from_iter_ab(z.into_iter(), r_12.into_iter());

    // Local computation of the final shares:
    // [b_0 XOR b_1 XOR b_2] = [b_0 XOR b_1] + [b_2] - 2 * [(b_0 XOR b_1 ) * b_2]
    // = [b_0 XOR b_1] + [b_2] - 2 * ([r_01 * b_2] + [x * b_2])
    // = s1 + s2 - 2 * (s3 + s4)
    Ok(VecShare::new_vec(
        izip!(s1, s2, s3, s4)
            .map(|(s1, s2, s3, s4)| {
                let sum12 = s1 + s2;
                let sum34 = s3 + s4;
                sum12 - sum34 - sum34
            })
            .collect_vec(),
    ))
}

/// Lifts the given shares of u16 to shares of u32 by multiplying them by 2^k.
///
/// This works since for any k-bit value b = x + y + z mod 2^16 with k < 16, it holds
/// (x >> l) + (y >> l) + (z >> l) = (b >> l) mod 2^32 for any l <= 32-k.
#[allow(dead_code)]
pub fn mul_lift_2k<const K: u64>(val: &Share<u16>) -> Share<u32> {
    let a = (u32::from(val.a.0)) << K;
    let b = (u32::from(val.b.0)) << K;
    Share::new(RingElement(a), RingElement(b))
}

/// Lifts the given shares of u16 to shares of u32 by multiplying them by 2^k.
#[allow(dead_code)]
fn mul_lift_2k_many<const K: u64>(vals: SliceShare<u16>) -> VecShare<u32> {
    VecShare::new_vec(vals.iter().map(mul_lift_2k::<K>).collect())
}

/// Lifts the given shares of u16 to shares of u32.
#[allow(dead_code)]
pub async fn lift(session: &mut Session, shares: VecShare<u16>) -> Result<VecShare<u32>> {
    let len = shares.len();
    let mut padded_len = len.div_ceil(64);
    padded_len *= 64;

    // Interpret the shares as u32
    let mut x_a = VecShare::with_capacity(padded_len);
    for share in shares.iter() {
        x_a.push(Share::new(
            RingElement(share.a.0 as u32),
            RingElement(share.b.0 as u32),
        ));
    }

    // Bit-slice the shares into 64-bit shares
    let x = shares.transpose_pack_u64();

    // Prepare the local input shares to be summed by the binary adder
    let len_ = x.len();
    let mut x1 = Vec::with_capacity(len_);
    let mut x2 = Vec::with_capacity(len_);
    let mut x3 = Vec::with_capacity(len_);

    for x_ in x.into_iter() {
        let len__ = x_.len();
        let mut x1_ = VecShare::with_capacity(len__);
        let mut x2_ = VecShare::with_capacity(len__);
        let mut x3_ = VecShare::with_capacity(len__);
        for x__ in x_.into_iter() {
            let (x1__, x2__, x3__) = a2b_pre(session, x__)?;
            x1_.push(x1__);
            x2_.push(x2__);
            x3_.push(x3__);
        }
        x1.push(x1_);
        x2.push(x2_);
        x3.push(x3_);
    }

    // Sum the binary shares using the binary parallel prefix adder.
    // Since the input shares are u16 and we sum over u32, the two carries arise, i.e.,
    // x1 + x2 + x3 = x + b1 * 2^16 + b2 * 2^17 mod 2^32
    let (mut b1, b2) = binary_add_3_get_two_carries(session, x1, x2, x3, len).await?;

    // Lift b1 and b2 into u16 via bit injection
    // This slightly deviates from Algorithm 10 from ePrint/2024/705 as bit injection to integers modulo 2^15 doesn't give any advantage.
    b1.extend(b2);
    let mut b = bit_inject(session, b1).await?;
    let (b1, b2) = b.split_at_mut(len);

    // Lift b1 and b2 into u32 and multiply them by 2^16 and 2^17, respectively.
    // This can be done by computing b1 as u32 << 16 and b2 as u32 << 17.
    let b1 = mul_lift_2k_many::<16>(b1.to_slice());
    let b2 = mul_lift_2k_many::<17>(b2.to_slice());

    // Compute x1 + x2 + x3 - b1 * 2^16 - b2 * 2^17 = x mod 2^32
    x_a.sub_assign(b1);
    x_a.sub_assign(b2);
    Ok(x_a)
}

/// Splits every share in `input` into two binary shares such that their sum is equal to a secret share of the originally secret shared value. In other words, if `v` is the input secret share of a value `x`, the function produces two secret shared binary strings `s1` and `s2` such that `s1 + s2` is a secret share of `x`.
///
/// The protocol follows the description of Protocol 18 from <https://eprint.iacr.org/2025/919.pdf>, see Section 6.1.
///
/// Protocol description:
/// - At the start of the protocol
///     - party `P_i` holds an input share `(a, b)`,
///     - party `P_{i+1}` holds an input share `(c, a)`,
///     - party `P_{i-1}` holds an input share `(b, c)`,
/// 1. The input shares are split into 3 pieces of (approximately) equal size: `v_0`, `v_1`, `v_2`. Those pieces are secret shares of the slices `x_0`, `x_1`, `x_2` of the original secret shared valued vector `x`
/// 2. Each party `P_i` processes piece `v_i` as follows:
///     - Every share in `v_i` is split into its `a` and `b` components.
///     - `P_i` generates randomness `r_prev` shared with the previous party, `P_{i-1}`.
///     - `P_i` computes `t = (a + b) XOR r_prev` and sends `t` to the next party, `P_{i+1}`.
///     - `P_i` sets its first output share `s_1_i` to `(t, r_prev)` and its second output share `s_2_i` to `(0, 0)`.
/// 3. Each party `P_i` processes piece `v_{i-1}` as follows:
///    - `P_i` extracts the `c` components of the shares in `v_{i-1}`.
///    - `P_i` receives `t` from the previous party, `P_{i-1}`.
///    - `P_i` sets its first output share `s_1_i` to `(0, t)` and its second output share `s_2_i` to `(c, 0)`.
/// 4. Each party `P_i` processes piece `v_{i+1}` as follows:
///    - `P_i` extracts the `c` components of the shares in `v_{i+1}`.
///    - `P_i` generates randomness `r_next` shared with the next party, `P_{i+1}`.
///    - `P_i` sets its first output share `s_1_i` to `(r_next, 0)` and its second output share `s_2_i` to `(0, c)`.
/// 5. The output shares `s_1` and `s_2` are constructed by concatenating `s_1_0`, `s_1_1`, and `s_1_2`, and `s_2_0`, `s_2_1`, and `s_2_2`, respectively.
///
/// Why it works?
/// For each input piece `v_i`, the output shares are constructed such that:
/// - `P_i` has shares `s1 = (t, r_prev)` and `s2 = (0, 0)`
/// - `P_{i+1}` has shares `s1 = (0, t)` and `s2 = (c, 0)`
/// - `P_{i-1}` has shares `s1 = (r_next, 0)` and `s2 = (0, c)`
///
/// XORing these shares and then opening the result yields:
///
/// `(t XOR r_next) + c = ((a + b) XOR r_prev XOR r_next) + c`
///
/// Since `r_prev` for `P_i` is the same as `r_next` for `P_{i-1}`, they cancel each other out resulting in:
///
/// `a + b + c = x_i`
async fn two_way_split<T: IntRing2k + NetworkInt>(
    session: &mut Session,
    input: VecShare<T>,
) -> Result<(VecBinShare<T>, VecBinShare<T>), Error>
where
    Standard: Distribution<T>,
{
    // Split input into 3 pieces
    let len = input.len();
    let piece_len = len.div_ceil(3);
    let party_index = session.own_role().index();
    let network = &mut session.network_session;
    let mut input_iter = input.iter();
    let mut s1_a = Vec::with_capacity(len);
    let mut s1_b = Vec::with_capacity(len);
    let mut s2_a = Vec::with_capacity(len);
    let mut s2_b = Vec::with_capacity(len);
    for chunk_id in 0..3 {
        if input_iter.len() == 0 {
            break;
        }
        if chunk_id == party_index {
            let my_chunk = input_iter.by_ref().take(piece_len);
            // Share my chunk
            // Split into a and b components
            let (a, b): (VecRingElement<_>, VecRingElement<_>) =
                my_chunk.map(|share| (share.a, share.b)).unzip();
            // Generate randomness shared between the current party and the previous party
            let r_prev: VecRingElement<T> = (0..a.len())
                .map(|_| session.prf.get_prev_prf().gen::<RingElement<T>>())
                .collect();
            // t = a + b XOR r_prev
            let t = ((a + b)? ^ &r_prev)?;
            // Send t to the next party
            network.send_ring_vec_next(&t).await?;
            // Collect first shares (t, r_prev)
            let chunk_len = t.len();
            s1_a.extend(t);
            s1_b.extend(r_prev);
            // Collect second shares (0, 0)
            s2_a.extend(zero_iter(chunk_len));
            s2_b.extend(zero_iter(chunk_len));
        } else if (chunk_id + 1) % 3 == party_index {
            let next_chunk = input_iter.by_ref().take(piece_len);
            // Extract the first component of the shares
            let c: VecRingElement<T> = next_chunk.map(|share| share.a).collect();
            // Receive t from the previous party
            let t: VecRingElement<T> = network.receive_ring_vec_prev().await?;
            // Collect first shares (0, t)
            let chunk_len = t.len();
            s1_a.extend(zero_iter(chunk_len));
            s1_b.extend(t);
            // Collect second shares (c, 0)
            s2_a.extend(c);
            s2_b.extend(zero_iter(chunk_len));
        } else {
            let prev_chunk = input_iter.by_ref().take(piece_len);
            // Extract the second component of the shares
            let c: VecRingElement<T> = prev_chunk.map(|share| share.b).collect();
            // Generate randomness shared between the current party and the next party
            let r_next: VecRingElement<T> = (0..c.len())
                .map(|_| session.prf.get_my_prf().gen::<RingElement<T>>())
                .collect();
            // Collect first shares (r_next, 0)
            let chunk_len = c.len();
            s1_a.extend(r_next);
            s1_b.extend(zero_iter(chunk_len));
            // Collect second shares (0, c)
            s2_a.extend(zero_iter(chunk_len));
            s2_b.extend(c);
        }
    }

    Ok((
        VecBinShare::from_ab(s1_a, s1_b),
        VecBinShare::from_ab(s2_a, s2_b),
    ))
}

/// Returns the MSB of the sum of two integers using the binary ripple-carry adder.
/// Input integers are given in binary form.
///
/// NOTE: This adder has a linear multiplicative depth, which is way worse than the logarithmic depth of the parallel prefix adder below.
/// However, its throughput is almost two times better.
#[allow(dead_code)]
#[cfg(feature = "ripple_carry_adder")]
async fn binary_add_2_get_msb<T: IntRing2k + NetworkInt>(
    session: &mut Session,
    x1: Vec<VecShare<T>>,
    x2: Vec<VecShare<T>>,
) -> Result<VecShare<T>, Error>
where
    Standard: Distribution<T>,
{
    if x1.len() != x2.len() {
        bail!("Inputs have different length {} {}", x1.len(), x2.len());
    };

    // Add a and b using the ripple-carry adder
    let mut a = x1;
    let mut b = x2;

    // To compute the MSB of the sum we have to add the MSB of s and c later
    let a_msb = a.pop().ok_or(eyre!("Not enough elements"))?;
    let b_msb = b.pop().ok_or(eyre!("Not enough elements"))?;

    let and_many_span = trace_span!(target: "searcher::network", "and_many_calls", n = a.len() - 1);

    // Initialize carry for the second LSB of the sum
    let mut carry = and_many(session, a[0].as_slice(), b[0].as_slice())
        .instrument(and_many_span.clone())
        .await?;

    // Compute carry for the MSB of the sum
    for (a_, b_) in a.iter_mut().skip(1).zip(b.iter_mut().skip(1)) {
        // carry = s_ AND c_ XOR carry AND (s_ XOR c_) = (s_ XOR carry) AND (c_ XOR carry) XOR carry
        *a_ ^= carry.as_slice();
        *b_ ^= carry.as_slice();
        let tmp_c = and_many(session, a_.as_slice(), b_.as_slice())
            .instrument(and_many_span.clone())
            .await?;
        carry ^= tmp_c;
    }

    // Return the MSB of the sum
    Ok(a_msb ^ b_msb ^ carry)
}

/// Returns the MSB of the sum of two integers of type T using the binary parallel prefix adder tree.
/// Input integers are given in binary form.
#[cfg(not(feature = "ripple_carry_adder"))]
async fn binary_add_2_get_msb<T: IntRing2k + NetworkInt>(
    session: &mut Session,
    x1: Vec<VecShare<T>>,
    x2: Vec<VecShare<T>>,
) -> Result<VecShare<T>, Error>
where
    Standard: Distribution<T>,
{
    if x1.len() != x2.len() {
        bail!("Inputs have different length {} {}", x1.len(), x2.len());
    };

    let mut a = x1;
    let mut b = x2;

    // Compute carry propagates p = a XOR b and carry generates g = a AND b
    let mut p = transposed_pack_xor(&a, &b);

    // The MSB of g is used to compute the carry of the whole sum; we don't need it as there is reduction modulo 2^k, where x1 and x2 are k-bit integers.
    a.pop();
    b.pop();
    let g = transposed_pack_and(session, a, b).await?;

    // The MSB of p is needed to compute the MSB of the sum, but it isn't needed for the carry computation
    let msb_p = p.pop().ok_or(eyre!("Not enough elements"))?;

    // Compute the carry for the MSB of the sum
    //
    // Reduce the above vectors according to the following rule:
    // p = (p0, p1, p2, p3,...) -> (p0 AND p1, p2 AND p3,...)
    // g = (g0, g1, g2, g3,...) -> (g1 XOR g0 AND p1, g3 XOR g2 AND p3,...)
    // Note that p0 is not needed to compute g, thus we can omit it as follows
    // p = (p1, p2, p3,...) -> (p2 AND p3, p4 AND p5...)
    let mut temp_p = p.drain(1..).collect::<Vec<_>>();
    let mut temp_g = g;

    while temp_g.len() != 1 {
        let (maybe_extra_p, maybe_extra_g) = if temp_g.len() % 2 == 1 {
            (temp_p.pop(), temp_g.pop())
        } else {
            (None, None)
        };

        // Split the vectors into even and odd indexed elements
        // Note that the starting index of temp_p is 1 due to removal of p0 above
        // We anticipate concatenations to allocate vecs with the correct capacity
        // and minimize cloning/collecting

        // To assess correctness of these sizes, note that the encompassing while loop
        // maintains the invariant `temp_g.len() - 1 = temp_p.len()`

        let mut even_p_with_even_g = Vec::with_capacity(temp_g.len() - 1);
        let mut odd_p_temp = Vec::with_capacity(temp_p.len() / 2 + 1);
        let mut odd_g = Vec::with_capacity(temp_g.len() / 2);

        for (i, p) in temp_p.into_iter().enumerate() {
            if i % 2 == 1 {
                even_p_with_even_g.push(p);
            } else {
                odd_p_temp.push(p);
            }
        }
        let new_p_len = even_p_with_even_g.len();

        for (i, g) in temp_g.into_iter().enumerate() {
            if i % 2 == 0 {
                even_p_with_even_g.push(g);
            } else {
                odd_g.push(g);
            }
        }

        // Now `even_p_with_even_g` contains merged even_p and even_g to multiply
        // them by odd_p at once
        // This corresponds to computing
        //            (p2 AND p3, p4 AND p5,...) and
        // (g0 AND p1, g2 AND p3, g4 AND p5,...) as above

        let mut odd_p_doubled = Vec::with_capacity(odd_p_temp.len() * 2 - 1);
        // Remove p1 to multiply even_p with odd_p
        odd_p_doubled.extend(odd_p_temp.iter().skip(1).cloned());
        odd_p_doubled.extend(odd_p_temp);

        //            (p2 AND p3, p4 AND p5,...,
        // g0 AND p1, g2 AND p3, g4 AND p5,...)
        let mut tmp = transposed_pack_and(session, even_p_with_even_g, odd_p_doubled).await?;

        // Update p
        temp_p = tmp.drain(..new_p_len).collect();
        if let Some(extra_p) = maybe_extra_p {
            temp_p.push(extra_p);
        }

        // Finish computing (g1 XOR g0 AND p1, g3 XOR g2 AND p3,...) and update g
        temp_g = transposed_pack_xor(&tmp, &odd_g);
        if let Some(extra_g) = maybe_extra_g {
            temp_g.push(extra_g);
        }
    }
    // a_msb XOR b_msb XOR top carry
    let msb = msb_p
        ^ temp_g
            .pop()
            .ok_or(eyre!("Should contain exactly 1 carry"))?;
    Ok(msb)
}

/// Extracts the MSBs of the secret shared input values in a bit-sliced form as u64 shares, i.e., the i-th bit of the j-th u64 secret share is the MSB of the (j * 64 + i)-th input value.
async fn extract_msb<T>(session: &mut Session, x_: VecShare<T>) -> Result<VecShare<u64>, Error>
where
    T: IntRing2k + NetworkInt,
    VecShare<T>: Transpose64,
    Standard: Distribution<T>,
{
    // Split the input shares into the sum of two shares
    let (x1, x2) = two_way_split(session, x_).await?;

    // Bit-slice the shares into 64-bit shares
    let x1_t = x1.inner.transpose_pack_u64();
    let x2_t = x2.inner.transpose_pack_u64();

    // Sum the binary shares using a binary adder and return the MSB.
    binary_add_2_get_msb::<u64>(session, x1_t, x2_t).await
}

/// Extracts the MSB of the secret shared input value.
#[allow(dead_code)]
pub async fn single_extract_msb<T>(session: &mut Session, x: Share<T>) -> Result<Share<Bit>, Error>
where
    T: IntRing2k + NetworkInt,
    VecShare<T>: Transpose64,
    Standard: Distribution<T>,
{
    let (a, b) = extract_msb(session, VecShare::new_vec(vec![x]))
        .await?
        .get_at(0)
        .get_ab();

    Ok(Share::new(a.get_bit_as_bit(0), b.get_bit_as_bit(0)))
}

/// Extracts the secret shared MSBs of the secret shared input values.
#[instrument(level = "trace", target = "searcher::network", skip_all)]
#[allow(dead_code)]
pub async fn extract_msb_batch<T>(session: &mut Session, x: &[Share<T>]) -> Result<Vec<Share<Bit>>>
where
    T: IntRing2k + NetworkInt,
    VecShare<T>: Transpose64,
    Standard: Distribution<T>,
{
    let res_len = x.len();
    let mut res = Vec::with_capacity(res_len);

    let packed_bits = extract_msb(session, VecShare::new_vec(x.to_vec())).await?;

    'outer: for bit_batch in packed_bits.into_iter() {
        let (a, b) = bit_batch.get_ab();
        for i in 0..64 {
            res.push(Share::new(a.get_bit_as_bit(i), b.get_bit_as_bit(i)));
            if res.len() == res_len {
                break 'outer;
            }
        }
    }

    Ok(res)
}

/// Opens a vector of binary additive replicated secret shares as described in the ABY3 framework.
///
/// In particular, each party holds a share of the form `(a, b)` where `a` and `b` are already known to the next and previous parties, respectively.
/// Thus, the current party should send its `b` share to the next party and receive the `b` share from the previous party.
/// `a XOR b XOR previous b` yields the opened bit.
#[instrument(level = "trace", target = "searcher::network", skip_all)]
pub async fn open_bin(session: &mut Session, shares: &[Share<Bit>]) -> Result<Vec<Bit>> {
    let network = &mut session.network_session;
    let message = if shares.len() == 1 {
        NetworkValue::RingElementBit(shares[0].b)
    } else {
        // TODO: could be optimized by packing bits
        let bits = shares
            .iter()
            .map(|x| NetworkValue::RingElementBit(x.b))
            .collect::<Vec<_>>();
        NetworkValue::vec_to_network(bits)
    };

    network.send_next(message).await?;

    // Receiving `b` from previous party
    let b_from_previous = {
        let other_shares = network
            .receive_prev()
            .await
            .map_err(|e| eyre!("Error in receiving in open_bin operation: {}", e))?;
        if shares.len() == 1 {
            match other_shares {
                NetworkValue::RingElementBit(message) => Ok(vec![message]),
                _ => Err(eyre!("Wrong value type is received in open_bin operation")),
            }
        } else {
            match NetworkValue::vec_from_network(other_shares) {
                Ok(v) => {
                    if matches!(v[0], NetworkValue::RingElementBit(_)) {
                        Ok(v.into_iter()
                            .map(|x| match x {
                                NetworkValue::RingElementBit(message) => message,
                                _ => unreachable!(),
                            })
                            .collect())
                    } else {
                        Err(eyre!("Wrong value type is received in open_bin operation"))
                    }
                }
                Err(e) => Err(eyre!("Error in receiving in open_bin operation: {}", e)),
            }
        }
    }?;

    // XOR shares with the received shares
    izip!(shares.iter(), b_from_previous.iter())
        .map(|(s, prev_b)| Ok((s.a ^ s.b ^ prev_b).convert()))
        .collect::<Result<Vec<_>>>()
}

#[cfg(test)]
mod tests {
    use crate::{
        execution::local::LocalRuntime,
        protocol::{ops::open_ring, test_utils::create_array_sharing},
    };

    use super::*;
    use aes_prng::AesRng;
    use eyre::Result;
    use num_traits::identities::One;
    use rand::Rng;
    use tokio::task::JoinSet;

    async fn test_bit_inject_generic<T: IntRing2k + NetworkInt>() -> Result<()>
    where
        Standard: Distribution<T>,
    {
        let mut rng = AesRng::from_random_seed();
        let len = 10;

        // Generate random bits and their shares
        let bits = (0..len).map(|_| rng.gen_bool(0.5).into()).collect_vec();
        let shares = create_array_sharing::<AesRng, Bit>(&mut rng, &bits);

        let sessions = LocalRuntime::mock_sessions_with_channel().await?;
        let mut jobs = JoinSet::new();

        for (i, session) in sessions.into_iter().enumerate() {
            let session = session.clone();
            let shares_i = VecShare::new_vec(shares.of_party(i).clone());
            jobs.spawn(async move {
                let mut session = session.lock().await;
                let injected = bit_inject::<T>(&mut session, shares_i).await.unwrap();
                open_ring(&mut session, injected.shares()).await
            });
        }
        let res = jobs
            .join_all()
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?;

        assert_eq!(res.len(), 3);
        assert_eq!(res[0], res[1]);
        assert_eq!(res[1], res[2]);

        let mut result_bits = Vec::with_capacity(len);
        for i in 0..len {
            let bit = res[0][i];
            if bit.is_one() {
                result_bits.push(true.into());
            } else if bit.is_zero() {
                result_bits.push(false.into());
            } else {
                panic!("Invalid bit value {:?} at index {}", bit, i);
            }
        }

        assert_eq!(bits, result_bits);

        Ok(())
    }

    #[tokio::test]
    async fn test_bit_inject_u16() -> Result<()> {
        test_bit_inject_generic::<u16>().await
    }

    #[tokio::test]
    async fn test_bit_inject_u32() -> Result<()> {
        test_bit_inject_generic::<u32>().await
    }

    #[tokio::test]
    async fn test_bit_inject_u64() -> Result<()> {
        test_bit_inject_generic::<u64>().await
    }

    async fn test_two_split_generic<T: IntRing2k + NetworkInt>() -> Result<()>
    where
        Standard: Distribution<T>,
    {
        let mut rng = AesRng::from_random_seed();
        let len = 10;

        // Generate random input and their shares
        let ints: Vec<T> = (0..len).map(|_| rng.gen::<T>()).collect();
        let shares = create_array_sharing(&mut rng, &ints);

        let sessions = LocalRuntime::mock_sessions_with_channel().await?;
        let mut jobs = JoinSet::new();

        for (i, session) in sessions.into_iter().enumerate() {
            let session = session.clone();
            let shares_i = VecShare::new_vec(shares.of_party(i).clone());
            jobs.spawn(async move {
                let mut session = session.lock().await;
                two_way_split::<T>(&mut session, shares_i).await
            });
        }
        let res = jobs
            .join_all()
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?;

        assert_eq!(res.len(), 3);
        assert_eq!(res[0].0.len(), res[1].0.len());
        assert_eq!(res[1].0.len(), res[2].0.len());
        assert_eq!(res[0].1.len(), res[1].1.len());
        assert_eq!(res[1].1.len(), res[2].1.len());

        for (i, expected) in ints.into_iter().enumerate() {
            let sum_1 =
                res[0].0.get_word_at(i).a ^ res[1].0.get_word_at(i).a ^ res[2].0.get_word_at(i).a;
            let sum_2 =
                res[0].1.get_word_at(i).a ^ res[1].1.get_word_at(i).a ^ res[2].1.get_word_at(i).a;
            let sum = sum_1 + sum_2;
            assert_eq!(expected, sum.convert());
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_two_split_u16() -> Result<()> {
        test_two_split_generic::<u16>().await
    }

    #[tokio::test]
    async fn test_two_split_u32() -> Result<()> {
        test_two_split_generic::<u32>().await
    }

    #[tokio::test]
    async fn test_two_split_u64() -> Result<()> {
        test_two_split_generic::<u64>().await
    }

    async fn test_extract_msb_generic<T: IntRing2k + NetworkInt>() -> Result<()>
    where
        VecShare<T>: Transpose64,
        Standard: Distribution<T>,
    {
        let mut rng = AesRng::from_random_seed();
        let len = 10;

        // Generate random input and their shares
        let ints: Vec<T> = (0..len).map(|_| rng.gen::<T>()).collect();
        let bits: Vec<Bit> = ints
            .iter()
            .map(|i| {
                let msb = *i >> (T::K - 1);
                if msb.is_zero() {
                    false.into()
                } else {
                    true.into()
                }
            })
            .collect();

        let shares = create_array_sharing(&mut rng, &ints);

        let sessions = LocalRuntime::mock_sessions_with_channel().await?;
        let mut jobs = JoinSet::new();

        for (i, session) in sessions.into_iter().enumerate() {
            let session = session.clone();
            let shares_i = VecShare::new_vec(shares.of_party(i).clone());
            jobs.spawn(async move {
                let mut session = session.lock().await;
                let msbs = extract_msb_batch::<T>(&mut session, shares_i.shares())
                    .await
                    .unwrap();
                open_bin(&mut session, &msbs).await
            });
        }
        let res = jobs
            .join_all()
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?;

        assert_eq!(res.len(), 3);
        assert_eq!(res[0], res[1]);
        assert_eq!(res[1], res[2]);

        let mut result_bits = Vec::with_capacity(len);
        for i in 0..len {
            let bit = res[0][i];
            if bit.is_one() {
                result_bits.push(true.into());
            } else {
                result_bits.push(false.into());
            }
        }

        assert_eq!(bits, result_bits);

        Ok(())
    }

    #[tokio::test]
    async fn test_extract_msb_u16() -> Result<()> {
        test_extract_msb_generic::<u16>().await
    }

    #[tokio::test]
    async fn test_extract_msb_u32() -> Result<()> {
        test_extract_msb_generic::<u32>().await
    }
}
