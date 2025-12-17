use ampc_secret_sharing::{IntRing2k, RingElement, Share};
use rand::{Rng, RngCore};
use rand_distr::{Distribution, Standard};

fn create_single_sharing<R: RngCore, T: IntRing2k>(
    rng: &mut R,
    input: T,
) -> (Share<T>, Share<T>, Share<T>)
where
    Standard: Distribution<T>,
{
    let a = RingElement(rng.gen::<T>());
    let b = RingElement(rng.gen::<T>());
    let c = RingElement(input) - a - b;

    let share1 = Share::new(a, c);
    let share2 = Share::new(b, a);
    let share3 = Share::new(c, b);
    (share1, share2, share3)
}
pub struct LocalShares1D<T: IntRing2k> {
    pub p0: Vec<Share<T>>,
    pub p1: Vec<Share<T>>,
    pub p2: Vec<Share<T>>,
}

impl<T: IntRing2k> LocalShares1D<T> {
    pub fn of_party(&self, party_id: usize) -> &Vec<Share<T>> {
        match party_id {
            0 => &self.p0,
            1 => &self.p1,
            2 => &self.p2,
            _ => panic!("Invalid party id"),
        }
    }
}

pub fn create_array_sharing<R: RngCore, T: IntRing2k>(
    rng: &mut R,
    input: &Vec<T>,
) -> LocalShares1D<T>
where
    Standard: Distribution<T>,
{
    let mut player0 = Vec::new();
    let mut player1 = Vec::new();
    let mut player2 = Vec::new();

    for entry in input {
        let (a, b, c) = create_single_sharing(rng, *entry);
        player0.push(a);
        player1.push(b);
        player2.push(c);
    }
    LocalShares1D {
        p0: player0,
        p1: player1,
        p2: player2,
    }
}
