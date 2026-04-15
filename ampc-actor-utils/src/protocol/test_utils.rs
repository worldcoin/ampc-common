use ampc_secret_sharing::{shares::share::AdditiveShare, IntRing2k, ReplicatedShare, RingElement};
use rand::{Rng, RngCore};
use rand_distr::{Distribution, Standard};

pub fn create_single_sharing_additive<R: RngCore, T: IntRing2k>(
    rng: &mut R,
    input: T,
) -> (AdditiveShare<T>, AdditiveShare<T>)
where
    Standard: Distribution<T>,
{
    let a = RingElement(rng.gen::<T>());
    let b = RingElement(input) - a;

    let share1 = AdditiveShare::new(a);
    let share2 = AdditiveShare::new(b);
    (share1, share2)
}

pub fn create_single_sharing_replicated<R: RngCore, T: IntRing2k>(
    rng: &mut R,
    input: T,
) -> (ReplicatedShare<T>, ReplicatedShare<T>, ReplicatedShare<T>)
where
    Standard: Distribution<T>,
{
    let a = RingElement(rng.gen::<T>());
    let b = RingElement(rng.gen::<T>());
    let c = RingElement(input) - a - b;

    let share1 = ReplicatedShare::new(a, c);
    let share2 = ReplicatedShare::new(b, a);
    let share3 = ReplicatedShare::new(c, b);
    (share1, share2, share3)
}
pub struct LocalShares1DReplicated<T: IntRing2k> {
    pub p0: Vec<ReplicatedShare<T>>,
    pub p1: Vec<ReplicatedShare<T>>,
    pub p2: Vec<ReplicatedShare<T>>,
}

impl<T: IntRing2k> LocalShares1DReplicated<T> {
    pub fn of_party(&self, party_id: usize) -> &Vec<ReplicatedShare<T>> {
        match party_id {
            0 => &self.p0,
            1 => &self.p1,
            2 => &self.p2,
            _ => panic!("Invalid party id"),
        }
    }
}

pub struct LocalShares1DAdditive<T: IntRing2k> {
    pub p0: Vec<AdditiveShare<T>>,
    pub p1: Vec<AdditiveShare<T>>,
    pub p2: Vec<AdditiveShare<T>>,
}

impl<T: IntRing2k> LocalShares1DAdditive<T> {
    pub fn of_party(&self, party_id: usize) -> &Vec<AdditiveShare<T>> {
        match party_id {
            0 => &self.p0,
            1 => &self.p1,
            2 => &self.p2,
            _ => panic!("Invalid party id"),
        }
    }
}

pub fn create_array_sharing_replicated<R: RngCore, T: IntRing2k>(
    rng: &mut R,
    input: &Vec<T>,
) -> LocalShares1DReplicated<T>
where
    Standard: Distribution<T>,
{
    let mut player0 = Vec::new();
    let mut player1 = Vec::new();
    let mut player2 = Vec::new();

    for entry in input {
        let (a, b, c) = create_single_sharing_replicated(rng, *entry);
        player0.push(a);
        player1.push(b);
        player2.push(c);
    }
    LocalShares1DReplicated {
        p0: player0,
        p1: player1,
        p2: player2,
    }
}

pub fn create_array_sharing_additive<R: RngCore, T: IntRing2k>(
    rng: &mut R,
    input: &Vec<T>,
) -> LocalShares1DAdditive<T>
where
    Standard: Distribution<T>,
{
    let mut player0 = Vec::new();
    let mut player1 = Vec::new();
    let mut player2 = Vec::new();

    for entry in input {
        let (a, b) = create_single_sharing_additive(rng, *entry);
        player0.push(a);
        player1.push(b);
        player2.push(AdditiveShare::zero());
    }
    LocalShares1DAdditive {
        p0: player0,
        p1: player1,
        p2: player2,
    }
}
