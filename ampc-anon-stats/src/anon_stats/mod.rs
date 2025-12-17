use ampc_actor_utils::protocol::ops::translate_threshold_a;
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

pub(crate) const MATCH_THRESHOLD_RATIO_REAUTH: f64 = 0.5;

pub fn calculate_iris_threshold_a(n_buckets: usize, upper_match_threshold_ratio: f64) -> Vec<u32> {
    (1..=n_buckets)
        .map(|x: usize| {
            translate_threshold_a(upper_match_threshold_ratio / (n_buckets as f64) * (x as f64))
        })
        .collect_vec()
}
