use ampc_actor_utils::{constants::MATCH_THRESHOLD_RATIO, protocol::ops::translate_threshold_a};
// Note: The following imports will need to be resolved when integrating with iris-mpc:
// - MATCH_THRESHOLD_RATIO from iris_mpc_common::iris_db::iris
// For now, these remain as iris_mpc_common dependencies
//
// Also check the submodules iris_1d and iris_2d for those dependencies.
use itertools::Itertools;

pub mod buckets;
pub mod face;
pub mod iris_1d;
pub mod iris_2d;

pub fn calculate_iris_threshold_a(n_buckets: usize) -> Vec<u32> {
    (1..=n_buckets)
        .map(|x: usize| {
            translate_threshold_a(MATCH_THRESHOLD_RATIO / (n_buckets as f64) * (x as f64))
        })
        .collect_vec()
}
