use aes_prng::AesRng;
use ampc_secret_sharing::{
    shares::{
        bit::Bit,
        primefield::PrimeElement,
        share::{AdditiveShare, AdditiveSharePrime},
    },
    IntRing2k, RingElement,
};
use eyre::{bail, eyre, Error, Result};
use num_traits::{One, PrimInt, Zero};
use rand::{Rng, SeedableRng};
use tracing::instrument;

use crate::{
    execution::session::{NetworkSession, Session},
    network::mpc::{NetworkInt, NetworkValue},
    protocol::PrfSeed,
};

// Shared seed setup
/// Setup a shared seed across first two parties in dealer model.
/// Each party (of 1, 2) sends their seed to the other and receives from each other.
/// The final shared seed is the XOR of both seeds.
pub async fn setup_shared_seed_dealer_model(
    session: &mut NetworkSession,
    my_seed: PrfSeed,
) -> Result<PrfSeed> {
    let my_msg = NetworkValue::PrfKey(my_seed);

    let decode = |msg| match msg {
        Ok(NetworkValue::PrfKey(seed)) => Ok(seed),
        _ => Err(eyre!("Could not deserialize PrfKey")),
    };

    let shared_seed = match session.own_role.index() {
        0 => {
            session.send_next(my_msg.clone()).await?;
            let other_seed = decode(session.receive_next().await)?;
            std::array::from_fn(|i| my_seed[i] ^ other_seed[i])
        }
        1 => {
            session.send_prev(my_msg).await?;
            let other_seed = decode(session.receive_prev().await)?;
            std::array::from_fn(|i| my_seed[i] ^ other_seed[i])
        }
        _ => {
            bail!("Cannot deal with roles that have index outside of the set [0, 1]")
        }
    };

    Ok(shared_seed)
}

// 1. Helper
// Binary-to-primefield conversion
pub async fn bin_to_primefield16(
    session: &mut Session,
    values: Vec<RingElement<Bit>>,
    modulus: u16,
) -> Result<Vec<AdditiveSharePrime<PrimeElement<u16>>>, Error> {
    let network = &mut session.network_session;
    let shares = match network.own_role.index() {
        0 => {
            let share_from_previous = network
                .receive_prev()
                .await
                .map_err(|e| eyre!("Error in receiving in open_bin operation: {}", e))?;
            if values.len() == 1 {
                match share_from_previous {
                    NetworkValue::PrimeElement16(message) => {
                        Ok(vec![AdditiveSharePrime::new(message)])
                    }
                    _ => Err(eyre!("Wrong value type is received in open_bin operation")),
                }
            } else {
                match NetworkValue::vec_from_network(share_from_previous) {
                    Ok(v) => {
                        if matches!(v[0], NetworkValue::PrimeElement16(_)) {
                            Ok(v.into_iter()
                                .map(|x| match x {
                                    NetworkValue::PrimeElement16(message) => {
                                        AdditiveSharePrime::new(message)
                                    }
                                    _ => unreachable!(),
                                })
                                .collect())
                        } else {
                            Err(eyre!("Wrong value type is received in open_bin operation"))
                        }
                    }
                    Err(e) => Err(eyre!("Error in receiving in open_bin operation: {}", e)),
                }
            }?
        }
        1 => {
            let share_from_next = network
                .receive_next()
                .await
                .map_err(|e| eyre!("Error in receiving in open_bin operation: {}", e))?;
            if values.len() == 1 {
                match share_from_next {
                    NetworkValue::PrimeElement16(message) => {
                        Ok(vec![AdditiveSharePrime::new(message)])
                    }
                    _ => Err(eyre!("Wrong value type is received in open_bin operation")),
                }
            } else {
                match NetworkValue::vec_from_network(share_from_next) {
                    Ok(v) => {
                        if matches!(v[0], NetworkValue::PrimeElement16(_)) {
                            Ok(v.into_iter()
                                .map(|x| match x {
                                    NetworkValue::PrimeElement16(message) => {
                                        AdditiveSharePrime::new(message)
                                    }
                                    _ => unreachable!(),
                                })
                                .collect())
                        } else {
                            Err(eyre!("Wrong value type is received in open_bin operation"))
                        }
                    }
                    Err(e) => Err(eyre!("Error in receiving in open_bin operation: {}", e)),
                }
            }?
        }
        2 => {
            let mut rng = AesRng::from_entropy();
            let (shares_0, shares_1): (
                Vec<AdditiveSharePrime<PrimeElement<u16>>>,
                Vec<AdditiveSharePrime<PrimeElement<u16>>>,
            ) = values
                .iter()
                .map(|value| {
                    let bit_as_mod19 =
                        PrimeElement::<u16>::new(u8::from(value.convert()) as u16, modulus);
                    let rand_mod19_share = PrimeElement::<u16>::rand(&mut rng, modulus);
                    let other_share = bit_as_mod19 - rand_mod19_share;
                    (
                        AdditiveSharePrime::new(other_share),
                        AdditiveSharePrime::new(rand_mod19_share),
                    )
                })
                .unzip();
            let message_next = if shares_0.len() == 1 {
                NetworkValue::PrimeElement16(shares_0[0].value)
            } else {
                let values = shares_0
                    .iter()
                    .map(|x| NetworkValue::PrimeElement16(x.value))
                    .collect::<Vec<_>>();
                NetworkValue::vec_to_network(values)
            };
            network.send_next(message_next).await?;
            let message_prev = if shares_1.len() == 1 {
                NetworkValue::PrimeElement16(shares_1[0].value)
            } else {
                let values = shares_1
                    .iter()
                    .map(|x| NetworkValue::PrimeElement16(x.value))
                    .collect::<Vec<_>>();
                NetworkValue::vec_to_network(values)
            };
            network.send_prev(message_prev).await?;
            vec![]
        }
        _ => bail!("Cannot deal with roles that have index outside of the set [0, 1, 2]"),
    };
    Ok(shares)
}

// 2. Helper
// Primefield-to-binary conversion
pub async fn primefield16_to_bin_one_hot(
    session: &mut Session,
    values: Vec<PrimeElement<u16>>,
) -> Result<Vec<AdditiveShare<Bit>>, Error> {
    let network = &mut session.network_session;
    let shares = match network.own_role.index() {
        0 => {
            let share_from_previous = network
                .receive_prev()
                .await
                .map_err(|e| eyre!("Error in receiving in open_bin operation: {}", e))?;
            if values.len() == 1 {
                match share_from_previous {
                    NetworkValue::RingElementBit(message) => Ok(vec![AdditiveShare::new(message)]),
                    _ => Err(eyre!("Wrong value type is received in open_bin operation")),
                }
            } else {
                match NetworkValue::vec_from_network(share_from_previous) {
                    Ok(v) => {
                        if matches!(v[0], NetworkValue::RingElementBit(_)) {
                            Ok(v.into_iter()
                                .map(|x| match x {
                                    NetworkValue::RingElementBit(message) => {
                                        AdditiveShare::new(message)
                                    }
                                    _ => unreachable!(),
                                })
                                .collect())
                        } else {
                            Err(eyre!("Wrong value type is received in open_bin operation"))
                        }
                    }
                    Err(e) => Err(eyre!("Error in receiving in open_bin operation: {}", e)),
                }
            }?
        }
        1 => {
            let share_from_next = network
                .receive_next()
                .await
                .map_err(|e| eyre!("Error in receiving in open_bin operation: {}", e))?;
            if values.len() == 1 {
                match share_from_next {
                    NetworkValue::RingElementBit(message) => Ok(vec![AdditiveShare::new(message)]),
                    _ => Err(eyre!("Wrong value type is received in open_bin operation")),
                }
            } else {
                match NetworkValue::vec_from_network(share_from_next) {
                    Ok(v) => {
                        if matches!(v[0], NetworkValue::RingElementBit(_)) {
                            Ok(v.into_iter()
                                .map(|x| match x {
                                    NetworkValue::RingElementBit(message) => {
                                        AdditiveShare::new(message)
                                    }
                                    _ => unreachable!(),
                                })
                                .collect())
                        } else {
                            Err(eyre!("Wrong value type is received in open_bin operation"))
                        }
                    }
                    Err(e) => Err(eyre!("Error in receiving in open_bin operation: {}", e)),
                }
            }?
        }
        2 => {
            let mut rng = AesRng::from_entropy();
            let (shares_0, shares_1): (Vec<AdditiveShare<Bit>>, Vec<AdditiveShare<Bit>>) = values
                .iter()
                .map(|value| {
                    let rand_bit_as_bool = rng.gen_range(0..=1) != 0;
                    let rand_bit = RingElement(Bit::from(rand_bit_as_bool));
                    let other_share = if value.is_zero() {
                        RingElement(Bit::one()) - rand_bit
                    } else {
                        RingElement(Bit::zero()) - rand_bit
                    };
                    (
                        AdditiveShare::new(other_share),
                        AdditiveShare::new(rand_bit),
                    )
                })
                .unzip();
            let message_next = if shares_0.len() == 1 {
                NetworkValue::RingElementBit(shares_0[0].value)
            } else {
                let values = shares_0
                    .iter()
                    .map(|x| NetworkValue::RingElementBit(x.value))
                    .collect::<Vec<_>>();
                NetworkValue::vec_to_network(values)
            };
            network.send_next(message_next).await?;
            let message_prev = if shares_1.len() == 1 {
                NetworkValue::RingElementBit(shares_1[0].value)
            } else {
                let values = shares_1
                    .iter()
                    .map(|x| NetworkValue::RingElementBit(x.value))
                    .collect::<Vec<_>>();
                NetworkValue::vec_to_network(values)
            };
            network.send_prev(message_prev).await?;
            vec![]
        }
        _ => bail!("Cannot deal with roles that have index outside of the set [0, 1, 2]"),
    };
    Ok(shares)
}

// 3. Helper
// Open additive shares
#[instrument(level = "trace", target = "searcher::network", skip_all)]
pub async fn open_additive_share<T: IntRing2k + NetworkInt, K: PrimInt>(
    session: &mut Session,
    share: &[AdditiveShare<T>],
) -> Result<Vec<T>, Error> {
    let network = &mut session.network_session;
    let message = if share.len() == 1 {
        T::new_network_element(share[0].value)
    } else {
        T::new_network_vec(
            share
                .iter()
                .map(|additive_share| additive_share.value)
                .collect(),
        )
    };

    network.send_next(message.clone()).await?;
    network.send_prev(message.clone()).await?;

    // Receiving share from previous party
    let share_from_previous =
        // receiving from previous party
        network
            .receive_prev()
            .await
            .and_then(|v| {
                T::into_vec(v)
            })
            .map_err(|e| eyre!("Error in receiving in open operation: {}", e))?;

    // Receiving share from next party
    let share_from_next = {
        // receiving from previous party
        network
            .receive_next()
            .await
            .and_then(|v| T::into_vec(v))
            .map_err(|e| eyre!("Error in receiving in open operation: {}", e))?
    };

    share
        .iter()
        .zip(share_from_previous.iter())
        .zip(share_from_next.iter())
        .map(|((share_a, share_b), share_c)| Ok((share_a.value + share_b + share_c).convert()))
        .collect::<Result<Vec<_>>>()
}

// 3b. Helper (test utility)
// Open a single additive bit share
#[instrument(level = "trace", target = "searcher::network", skip_all)]
pub async fn open_additive_share_bit(
    session: &mut Session,
    share: &AdditiveShare<Bit>,
) -> Result<RingElement<Bit>, Error> {
    let network = &mut session.network_session;
    let message = NetworkValue::RingElementBit(share.value);

    network.send_next(message.clone()).await?;
    network.send_prev(message).await?;

    // Receiving share from previous party
    let share_from_previous = {
        let prev_share = network
            .receive_prev()
            .await
            .map_err(|e| eyre!("Error in receiving in open_Bit operation: {}", e))?;

        match prev_share {
            NetworkValue::RingElementBit(message) => Ok(message),
            _ => Err(eyre!("Wrong value type is received in open_Bit operation")),
        }
    }?;

    // Receiving share from next party
    let share_from_next = {
        let next_share = network
            .receive_next()
            .await
            .map_err(|e| eyre!("Error in receiving in open_Bit operation: {}", e))?;

        match next_share {
            NetworkValue::RingElementBit(message) => Ok(message),
            _ => Err(eyre!("Wrong value type is received in open_Bit operation")),
        }
    }?;

    Ok(share.value + share_from_previous + share_from_next)
}

// 4. Helper
// Send binary shares to dealer
#[instrument(level = "trace", target = "searcher::network", skip_all)]
pub async fn send_binary_shares_to_dealer(
    session: &mut Session,
    shares: &[AdditiveShare<Bit>],
) -> Result<Vec<RingElement<Bit>>, Error> {
    let network = &mut session.network_session;
    let message = if shares.len() == 1 {
        NetworkValue::RingElementBit(shares[0].value)
    } else {
        // TODO: could be optimized by packing bits
        let bits = shares
            .iter()
            .map(|x| NetworkValue::RingElementBit(x.value))
            .collect::<Vec<_>>();
        NetworkValue::vec_to_network(bits)
    };

    let values_received = match network.own_role.index() {
        0 => {
            network.send_prev(message.clone()).await?;
            vec![]
        }
        1 => {
            network.send_next(message.clone()).await?;
            vec![]
        }
        2 => {
            let share_from_previous = network
                .receive_prev()
                .await
                .map_err(|e| eyre!("Error in receiving in open_bin operation: {}", e))?;
            let values_from_previous = if shares.len() == 1 {
                match share_from_previous {
                    NetworkValue::RingElementBit(message) => Ok(vec![message]),
                    _ => Err(eyre!("Wrong value type is received in open_bin operation")),
                }
            } else {
                match NetworkValue::vec_from_network(share_from_previous) {
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
            }?;
            let share_from_next = network
                .receive_next()
                .await
                .map_err(|e| eyre!("Error in receiving in open_bin operation: {}", e))?;
            let values_from_next = if shares.len() == 1 {
                match share_from_next {
                    NetworkValue::RingElementBit(message) => Ok(vec![message]),
                    _ => Err(eyre!("Wrong value type is received in open_bin operation")),
                }
            } else {
                match NetworkValue::vec_from_network(share_from_next) {
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
            }?;

            values_from_previous
                .iter()
                .zip(values_from_next.iter())
                .map(|(prev, next)| *prev ^ next)
                .collect()
        }
        _ => {
            bail!("Cannot deal with roles that have index outside of the set [0, 1, 2]")
        }
    };
    Ok(values_received)
}

// 5. Helper
// Send primeshares to dealer
#[instrument(level = "trace", target = "searcher::network", skip_all)]
pub async fn send_prime16_shares_to_dealer(
    session: &mut Session,
    shares: &[AdditiveSharePrime<PrimeElement<u16>>],
) -> Result<Vec<PrimeElement<u16>>, Error> {
    let network = &mut session.network_session;
    let message = if shares.len() == 1 {
        NetworkValue::PrimeElement16(shares[0].value)
    } else {
        // TODO: could be optimized by packing bits
        let bits = shares
            .iter()
            .map(|x| NetworkValue::PrimeElement16(x.value))
            .collect::<Vec<_>>();
        NetworkValue::vec_to_network(bits)
    };
    let values_received = match network.own_role.index() {
        0 => {
            network.send_prev(message.clone()).await?;
            vec![]
        }
        1 => {
            network.send_next(message.clone()).await?;
            vec![]
        }
        2 => {
            let share_from_previous = network
                .receive_prev()
                .await
                .map_err(|e| eyre!("Error in receiving in open_bin operation: {}", e))?;
            let values_from_previous = if shares.len() == 1 {
                match share_from_previous {
                    NetworkValue::PrimeElement16(message) => Ok(vec![message]),
                    _ => Err(eyre!("Wrong value type is received in open_bin operation")),
                }
            } else {
                match NetworkValue::vec_from_network(share_from_previous) {
                    Ok(v) => {
                        if matches!(v[0], NetworkValue::PrimeElement16(_)) {
                            Ok(v.into_iter()
                                .map(|x| match x {
                                    NetworkValue::PrimeElement16(message) => message,
                                    _ => unreachable!(),
                                })
                                .collect())
                        } else {
                            Err(eyre!("Wrong value type is received in open_bin operation"))
                        }
                    }
                    Err(e) => Err(eyre!("Error in receiving in open_bin operation: {}", e)),
                }
            }?;
            let share_from_next = network
                .receive_next()
                .await
                .map_err(|e| eyre!("Error in receiving in open_bin operation: {}", e))?;

            let values_from_next = if shares.len() == 1 {
                match share_from_next {
                    NetworkValue::PrimeElement16(message) => Ok(vec![message]),
                    _ => Err(eyre!("Wrong value type is received in open_bin operation")),
                }
            } else {
                match NetworkValue::vec_from_network(share_from_next) {
                    Ok(v) => {
                        if matches!(v[0], NetworkValue::PrimeElement16(_)) {
                            Ok(v.into_iter()
                                .map(|x| match x {
                                    NetworkValue::PrimeElement16(message) => message,
                                    _ => unreachable!(),
                                })
                                .collect())
                        } else {
                            Err(eyre!("Wrong value type is received in open_bin operation"))
                        }
                    }
                    Err(e) => Err(eyre!("Error in receiving in open_bin operation: {}", e)),
                }
            }?;
            values_from_previous
                .iter()
                .zip(values_from_next.iter())
                .map(|(prev, next)| *prev + *next)
                .collect()
        }
        _ => {
            bail!("Cannot deal with roles that have index outside of the set [0, 1, 2]")
        }
    };
    Ok(values_received)
}
