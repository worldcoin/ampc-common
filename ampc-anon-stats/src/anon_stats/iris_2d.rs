use ampc_actor_utils::{
    constants::MATCH_THRESHOLD_RATIO,
    execution::session::Session,
    protocol::{
        anon_stats::{compare_against_thresholds_batched, reduce_to_min_distance_batch},
        ops::open_ring_element_broadcast,
    },
};
use ampc_secret_sharing::RingElement;
use chrono::{DateTime, Utc};
use eyre::Result;
use itertools::{izip, Itertools};

use crate::{
    anon_stats::{
        calculate_iris_threshold_a,
        iris_1d::{lift_bundles_1d, DistanceBundle1D, LiftedDistanceBundle1D},
        MATCH_THRESHOLD_RATIO_REAUTH,
    },
    types::AnonStatsResultSource,
    AnonStatsMapping, AnonStatsOperation, AnonStatsServerConfig, BucketStatistics2D,
};

pub type DistanceBundle2D = (DistanceBundle1D, DistanceBundle1D);
pub type LiftedDistanceBundle2D = (LiftedDistanceBundle1D, LiftedDistanceBundle1D);

pub async fn lift_bundles_2d(
    session: &mut Session,
    bundles: &[DistanceBundle2D],
) -> Result<Vec<LiftedDistanceBundle2D>> {
    let bundle_left = bundles.iter().map(|(left, _)| left.clone()).collect_vec();
    let bundle_right = bundles.iter().map(|(_, right)| right.clone()).collect_vec();
    let lifted_left = lift_bundles_1d(session, &bundle_left).await?;
    let lifted_right = lift_bundles_1d(session, &bundle_right).await?;
    let lifted_bundles = lifted_left
        .into_iter()
        .zip(lifted_right.into_iter())
        .collect_vec();
    Ok(lifted_bundles)
}

pub async fn process_2d_anon_stats_job(
    session: &mut Session,
    job: AnonStatsMapping<DistanceBundle2D>,
    config: &AnonStatsServerConfig,
    operation: Option<AnonStatsOperation>,
    start_timestamp: Option<DateTime<Utc>>,
) -> Result<BucketStatistics2D> {
    let job_size = job.len();
    let job_data = job.into_bundles();
    let (num_buckets, upper_threshold) = match operation {
        Some(AnonStatsOperation::Reauth) => {
            (config.n_buckets_2d_reauth, MATCH_THRESHOLD_RATIO_REAUTH)
        }
        None | Some(AnonStatsOperation::Uniqueness) => (config.n_buckets_2d, MATCH_THRESHOLD_RATIO),
    };
    let translated_thresholds = calculate_iris_threshold_a(num_buckets, upper_threshold);

    let bundle_left = job_data.iter().map(|(left, _)| left.clone()).collect_vec();
    let bundle_right = job_data
        .iter()
        .map(|(_, right)| right.clone())
        .collect_vec();
    drop(job_data);

    // Lift both sides of the 2D bundles
    let lifted_left = lift_bundles_1d(session, &bundle_left).await?;
    drop(bundle_left);
    let lifted_right = lift_bundles_1d(session, &bundle_right).await?;
    drop(bundle_right);

    // Reduce both sides to min distances
    let lifted_min_left = reduce_to_min_distance_batch(session, &lifted_left).await?;
    drop(lifted_left);
    let lifted_min_right = reduce_to_min_distance_batch(session, &lifted_right).await?;
    drop(lifted_right);

    // execute anon stats MPC protocol
    let comparisons_left = compare_against_thresholds_batched(
        session,
        translated_thresholds.as_slice(),
        &lifted_min_left,
    )
    .await?;
    drop(lifted_min_left);
    let comparisons_right = compare_against_thresholds_batched(
        session,
        translated_thresholds.as_slice(),
        &lifted_min_right,
    )
    .await?;
    drop(lifted_min_right);

    // combine left and right comparisons by doing an outer product
    // at the same time also do the summing

    let mut bucket_shares = vec![RingElement::<u32>::default(); num_buckets * num_buckets];

    // prepare the correlated randomness for the bucket sums
    // we want additive shares of 0
    for bucket in &mut bucket_shares {
        *bucket += session.prf.gen_zero_share();
    }

    let mut bucket_ids = 0;
    // TODO: This could be parallelized if needed
    for left_chunk in comparisons_left.chunks(job_size) {
        for right_chunk in comparisons_right.chunks(job_size) {
            assert!(left_chunk.len() == right_chunk.len());

            let product_sum = izip!(left_chunk, right_chunk)
                .map(|(left_share, right_share)| left_share * right_share)
                .fold(RingElement(0u32), |acc, x| acc + x);
            bucket_shares[bucket_ids] += product_sum;
            bucket_ids += 1;
        }
    }

    let buckets = open_ring_element_broadcast(session, &bucket_shares).await?;
    let mut anon_stats = BucketStatistics2D::new(
        job_size,
        num_buckets,
        config.party_id,
        AnonStatsResultSource::Aggregator,
        operation,
    );
    anon_stats.fill_buckets(&buckets, upper_threshold, start_timestamp);
    Ok(anon_stats)
}

pub mod test_helper {
    use crate::types::AnonStatsResultSource;
    use crate::{BucketStatistics2D, DistanceBundle2D};
    use ampc_actor_utils::constants::MATCH_THRESHOLD_RATIO;
    use ampc_secret_sharing::shares::{share::DistanceShare, RingElement};
    use ampc_secret_sharing::Share;
    use itertools::Itertools;

    pub struct TestDistances {
        #[allow(clippy::type_complexity)]
        pub distances: Vec<(Vec<[i16; 2]>, Vec<[i16; 2]>)>,
        pub shares0: Vec<DistanceBundle2D>,
        pub shares1: Vec<DistanceBundle2D>,
        pub shares2: Vec<DistanceBundle2D>,
    }

    impl TestDistances {
        pub fn generate_ground_truth_input(
            rng: &mut impl rand::Rng,
            num_bundles: usize,
            max_rotations: usize,
        ) -> TestDistances {
            let sizes = (0..num_bundles)
                .map(|_| rng.gen_range(1..=max_rotations))
                .collect_vec();
            let flat_size = sizes.iter().sum::<usize>();

            let mut generate_items = || {
                let items = (0..flat_size)
                    .map(|_| {
                        let mask = rng.gen_range(6000i16..12000);
                        let code = rng.gen_range(-12000i16..12000);
                        [code, mask]
                    })
                    .collect_vec();
                let (shares1, shares2, shares3): (Vec<_>, Vec<_>, Vec<_>) = items
                    .iter()
                    .map(|&x| {
                        let share1: u16 = rng.gen();
                        let share2: u16 = rng.gen();
                        let share3: u16 = (x[0] as u16).wrapping_sub(share1).wrapping_sub(share2);
                        let mshare1: u16 = rng.gen();
                        let mshare2: u16 = rng.gen();
                        let mshare3: u16 =
                            (x[1] as u16).wrapping_sub(mshare1).wrapping_sub(mshare2);
                        (
                            DistanceShare {
                                code_dot: Share {
                                    a: RingElement(share1),
                                    b: RingElement(share3),
                                },
                                mask_dot: Share {
                                    a: RingElement(mshare1),
                                    b: RingElement(mshare3),
                                },
                            },
                            DistanceShare {
                                code_dot: Share {
                                    a: RingElement(share2),
                                    b: RingElement(share1),
                                },
                                mask_dot: Share {
                                    a: RingElement(mshare2),
                                    b: RingElement(mshare1),
                                },
                            },
                            DistanceShare {
                                code_dot: Share {
                                    a: RingElement(share3),
                                    b: RingElement(share2),
                                },
                                mask_dot: Share {
                                    a: RingElement(mshare3),
                                    b: RingElement(mshare2),
                                },
                            },
                        )
                    })
                    .multiunzip();

                let mut idx = 0;
                let bundles = sizes
                    .iter()
                    .map(|&size| {
                        let mut bundle = Vec::with_capacity(size);
                        for _ in 0..size {
                            bundle.push(items[idx]);
                            idx += 1;
                        }
                        bundle
                    })
                    .collect_vec();
                assert!(idx == items.len());

                let mut idx = 0;
                let bundle_shares1 = sizes
                    .iter()
                    .map(|&size| {
                        let mut bundle = Vec::with_capacity(size);
                        for _ in 0..size {
                            bundle.push(shares1[idx]);
                            idx += 1;
                        }
                        bundle
                    })
                    .collect_vec();
                assert!(idx == items.len());

                let mut idx = 0;
                let bundle_shares2 = sizes
                    .iter()
                    .map(|&size| {
                        let mut bundle = Vec::with_capacity(size);
                        for _ in 0..size {
                            bundle.push(shares2[idx]);
                            idx += 1;
                        }
                        bundle
                    })
                    .collect_vec();
                assert!(idx == items.len());

                let mut idx = 0;
                let bundle_shares3 = sizes
                    .iter()
                    .map(|&size| {
                        let mut bundle = Vec::with_capacity(size);
                        for _ in 0..size {
                            bundle.push(shares3[idx]);
                            idx += 1;
                        }
                        bundle
                    })
                    .collect_vec();
                assert!(idx == items.len());
                (bundles, bundle_shares1, bundle_shares2, bundle_shares3)
            };
            let (items_left, shares0_left, shares1_left, shares2_left) = generate_items();
            let (items_right, shares0_right, shares1_right, shares2_right) = generate_items();

            let bundles = items_left.into_iter().zip(items_right).collect_vec();
            let bundle_shares0 = shares0_left.into_iter().zip(shares0_right).collect_vec();
            let bundle_shares1 = shares1_left.into_iter().zip(shares1_right).collect_vec();
            let bundle_shares2 = shares2_left.into_iter().zip(shares2_right).collect_vec();

            TestDistances {
                distances: bundles,
                shares0: bundle_shares0,
                shares1: bundle_shares1,
                shares2: bundle_shares2,
            }
        }

        pub fn ground_truth_buckets(&self, translated_thresholds: &[u32]) -> BucketStatistics2D {
            let num_buckets = translated_thresholds.len();
            let expected = self
                .distances
                .iter()
                .map(|(group_left, group_right)| {
                    // reduce distances in each group
                    let red_left = group_left
                        .iter()
                        .reduce(|a, b| {
                            // plain distance formula is (0.5 - code/2*mask), the below is that multiplied by 2
                            if (1f64 - a[0] as f64 / a[1] as f64)
                                < (1f64 - b[0] as f64 / b[1] as f64)
                            {
                                a
                            } else {
                                b
                            }
                        })
                        .expect("Expected at least one distance in the group");
                    let red_right = group_right
                        .iter()
                        .reduce(|a, b| {
                            // plain distance formula is (0.5 - code/2*mask), the below is that multiplied by 2
                            if (1f64 - a[0] as f64 / a[1] as f64)
                                < (1f64 - b[0] as f64 / b[1] as f64)
                            {
                                a
                            } else {
                                b
                            }
                        })
                        .expect("Expected at least one distance in the group");
                    (red_left, red_right)
                })
                .fold(vec![0; num_buckets * num_buckets], |mut acc, (x, y)| {
                    let code_dist_left = x[0];
                    let mask_dist_left = x[1];
                    let code_dist_right = y[0];
                    let mask_dist_right = y[1];
                    let dist_left =
                        0.5f64 - (code_dist_left as f64) / (2f64 * mask_dist_left as f64);
                    let dist_right =
                        0.5f64 - (code_dist_right as f64) / (2f64 * mask_dist_right as f64);
                    for (i, &threshold_left) in translated_thresholds.iter().enumerate() {
                        for (j, &threshold_right) in translated_thresholds.iter().enumerate() {
                            acc[i * num_buckets + j] += if (dist_left
                                < 0.5f64 - threshold_left as f64 / (2f64 * 65536f64))
                                && (dist_right
                                    < 0.5f64 - threshold_right as f64 / (2f64 * 65536f64))
                            {
                                1
                            } else {
                                0
                            };
                        }
                    }
                    acc
                });
            let mut anon_stats = BucketStatistics2D::new(
                self.distances.len(),
                translated_thresholds.len(),
                0,
                AnonStatsResultSource::Aggregator,
                None,
            );
            anon_stats.fill_buckets(&expected, MATCH_THRESHOLD_RATIO, None);
            anon_stats
        }
    }
}

#[cfg(test)]
mod tests {
    use ampc_actor_utils::{constants::MATCH_THRESHOLD_RATIO, execution::local::LocalRuntime};
    use rand::thread_rng;

    use crate::{
        anon_stats::{
            calculate_iris_threshold_a, iris_2d::test_helper::TestDistances,
            MATCH_THRESHOLD_RATIO_REAUTH,
        },
        AnonStatsServerConfig,
    };

    #[tokio::test]
    async fn test_2d_distances() {
        let sessions = LocalRuntime::mock_sessions_with_channel().await.unwrap();
        let num_buckets_2d = 10;
        let thresholds = calculate_iris_threshold_a(num_buckets_2d, MATCH_THRESHOLD_RATIO);

        let config = AnonStatsServerConfig {
            party_id: 0,
            face_bucket_thresholds: vec![],
            service: None,
            aws: None,
            environment: "test".to_string(),
            results_topic_arn: "foo".to_string(),
            n_buckets_1d: 0,
            n_buckets_1d_reauth: 0,
            min_1d_job_size: 0,
            min_face_job_size: 0,
            poll_interval_secs: 10,
            max_sync_failures_before_reset: 10,
            db_url: "foo".to_string(),
            db_schema_name: "foo".to_string(),
            server_coordination: None,
            service_ports: Vec::new(),
            shutdown_last_results_sync_timeout_secs: 10,
            sns_buffer_bucket_name: "foo".to_string(),
            n_buckets_2d: num_buckets_2d,
            n_buckets_2d_reauth: 0,
            min_2d_job_size: 0,
            min_1d_job_size_reauth: 0,
            min_2d_job_size_reauth: 0,
            max_rows_per_job_1d: 0,
            max_rows_per_job_2d: 0,
            tls: None,
        };
        let ground_truth = TestDistances::generate_ground_truth_input(&mut thread_rng(), 1000, 12);
        let ground_truth_buckets = ground_truth.ground_truth_buckets(&thresholds);
        let TestDistances {
            distances: _,
            shares0,
            shares1,
            shares2,
        } = ground_truth;

        let mut tasks = vec![];
        for (party_id, (shares, net)) in [shares0, shares1, shares2]
            .into_iter()
            .zip(sessions.into_iter())
            .enumerate()
        {
            let config = AnonStatsServerConfig {
                party_id,
                ..config.clone()
            };

            tasks.push(tokio::task::spawn(async move {
                let mut session = net.lock().await;
                let shares = shares
                    .into_iter()
                    .enumerate()
                    .map(|(idx, s)| (idx as i64, s))
                    .collect();
                let job = crate::AnonStatsMapping::new(shares);

                let stats = crate::anon_stats::iris_2d::process_2d_anon_stats_job(
                    &mut session,
                    job,
                    &config,
                    Some(crate::AnonStatsOperation::Uniqueness),
                    None,
                )
                .await
                .unwrap();

                stats
            }));
        }
        let results = futures_util::future::join_all(tasks).await;
        for stats in results {
            let stats = stats.expect("bucket computation works");
            assert_eq!(
                stats.buckets.len(),
                ground_truth_buckets.buckets.len(),
                "Number of buckets mismatch"
            );
            for (i, bucket) in stats.buckets.iter().enumerate() {
                assert_eq!(
                    bucket.count, ground_truth_buckets.buckets[i].count,
                    "Bucket {} mismatch: expected {:?}, got {:?}",
                    i, ground_truth_buckets.buckets[i], bucket
                );
            }
        }
    }

    #[tokio::test]
    async fn test_2d_distances_reauth() {
        let sessions = LocalRuntime::mock_sessions_with_channel().await.unwrap();
        let num_buckets_2d_reauth = 15;
        let thresholds =
            calculate_iris_threshold_a(num_buckets_2d_reauth, MATCH_THRESHOLD_RATIO_REAUTH);

        let config = AnonStatsServerConfig {
            party_id: 0,
            face_bucket_thresholds: vec![],
            service: None,
            aws: None,
            environment: "test".to_string(),
            results_topic_arn: "foo".to_string(),
            n_buckets_1d: 0,
            n_buckets_1d_reauth: 0,
            min_1d_job_size: 0,
            min_face_job_size: 0,
            poll_interval_secs: 10,
            max_sync_failures_before_reset: 10,
            db_url: "foo".to_string(),
            db_schema_name: "foo".to_string(),
            server_coordination: None,
            service_ports: Vec::new(),
            shutdown_last_results_sync_timeout_secs: 10,
            sns_buffer_bucket_name: "foo".to_string(),
            n_buckets_2d: 0,
            n_buckets_2d_reauth: num_buckets_2d_reauth,
            min_2d_job_size: 0,
            min_1d_job_size_reauth: 0,
            min_2d_job_size_reauth: 0,
            max_rows_per_job_1d: 0,
            max_rows_per_job_2d: 0,
            tls: None,
        };
        let ground_truth = TestDistances::generate_ground_truth_input(&mut thread_rng(), 1000, 31);
        let ground_truth_buckets = ground_truth.ground_truth_buckets(&thresholds);
        let TestDistances {
            distances: _,
            shares0,
            shares1,
            shares2,
        } = ground_truth;

        let mut tasks = vec![];
        for (party_id, (shares, net)) in [shares0, shares1, shares2]
            .into_iter()
            .zip(sessions.into_iter())
            .enumerate()
        {
            let config = AnonStatsServerConfig {
                party_id,
                ..config.clone()
            };

            tasks.push(tokio::task::spawn(async move {
                let mut session = net.lock().await;
                let shares = shares
                    .into_iter()
                    .enumerate()
                    .map(|(idx, s)| (idx as i64, s))
                    .collect();
                let job = crate::AnonStatsMapping::new(shares);

                let stats = crate::anon_stats::iris_2d::process_2d_anon_stats_job(
                    &mut session,
                    job,
                    &config,
                    Some(crate::AnonStatsOperation::Reauth),
                )
                .await
                .unwrap();

                stats
            }));
        }
        let results = futures_util::future::join_all(tasks).await;
        for stats in results {
            let stats = stats.expect("bucket computation works");
            assert_eq!(
                stats.buckets.len(),
                ground_truth_buckets.buckets.len(),
                "Number of buckets mismatch"
            );
            for (i, bucket) in stats.buckets.iter().enumerate() {
                assert_eq!(
                    bucket.count, ground_truth_buckets.buckets[i].count,
                    "Bucket {} mismatch: expected {:?}, got {:?}",
                    i, ground_truth_buckets.buckets[i], bucket
                );
            }
        }
    }
}
