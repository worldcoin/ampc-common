use crate::anon_stats::{calculate_iris_threshold_a, MATCH_THRESHOLD_RATIO_REAUTH};
use crate::server::config::AnonStatsServerConfig;
use crate::types::AnonStatsResultSource;
use crate::{AnonStatsMapping, AnonStatsOperation, AnonStatsOrigin, BucketStatistics};
use ampc_actor_utils::{
    constants::MATCH_THRESHOLD_RATIO,
    execution::session::Session,
    protocol::{
        anon_stats::compare_min_threshold_buckets,
        ops::{batch_signed_lift_vec, open_ring},
    },
};
use ampc_secret_sharing::shares::DistanceShare;
use chrono::{DateTime, Utc};
use eyre::Result;
use itertools::Itertools;

pub type DistanceBundle1D = Vec<DistanceShare<u16>>;
pub type LiftedDistanceBundle1D = Vec<DistanceShare<u32>>;

pub async fn lift_bundles_1d(
    session: &mut Session,
    bundles: &[DistanceBundle1D],
) -> Result<Vec<LiftedDistanceBundle1D>> {
    let flattened = bundles
        .iter()
        .flat_map(|x| x.iter().flat_map(|y| [y.code_dot, y.mask_dot]))
        .collect_vec();
    let lifted_flattened = batch_signed_lift_vec(session, flattened.clone()).await?;

    // reconstruct lifted bundles in original shape
    let mut idx = 0;
    let lifted = bundles
        .iter()
        .map(|b| b.len())
        .map(|chunk_size| {
            let mut lifted_bundle = Vec::with_capacity(chunk_size);
            for _ in 0..chunk_size {
                let code_dot = lifted_flattened[idx];
                let mask_dot = lifted_flattened[idx + 1];
                idx += 2;
                lifted_bundle.push(DistanceShare { code_dot, mask_dot });
            }
            lifted_bundle
        })
        .collect();
    assert!(idx == lifted_flattened.len());
    Ok(lifted)
}

pub async fn process_1d_anon_stats_job(
    session: &mut Session,
    job: AnonStatsMapping<DistanceBundle1D>,
    origin: &AnonStatsOrigin,
    config: &AnonStatsServerConfig,
    operation: Option<AnonStatsOperation>,
    start_timestamp: Option<DateTime<Utc>>,
) -> Result<BucketStatistics> {
    let job_size = job.len();
    let job_data = job.into_bundles();
    let lifted_data = lift_bundles_1d(session, &job_data).await?;
    let translated_thresholds = match operation {
        Some(AnonStatsOperation::Reauth) => {
            calculate_iris_threshold_a(config.n_buckets_1d_reauth, MATCH_THRESHOLD_RATIO_REAUTH)
        }
        None | Some(AnonStatsOperation::Uniqueness) => {
            calculate_iris_threshold_a(config.n_buckets_1d, MATCH_THRESHOLD_RATIO)
        }
    };

    // execute anon stats MPC protocol
    let bucket_result_shares = compare_min_threshold_buckets(
        session,
        translated_thresholds.as_slice(),
        lifted_data.as_slice(),
    )
    .await?;

    let buckets = open_ring(session, &bucket_result_shares).await?;
    let mut anon_stats = BucketStatistics::new(
        job_size,
        config.n_buckets_1d,
        config.party_id,
        origin.side,
        AnonStatsResultSource::Aggregator,
        operation,
    );

    anon_stats.fill_buckets(&buckets, MATCH_THRESHOLD_RATIO, start_timestamp);
    Ok(anon_stats)
}

pub async fn process_1d_lifted_anon_stats_job(
    session: &mut Session,
    job: AnonStatsMapping<LiftedDistanceBundle1D>,
    origin: &AnonStatsOrigin,
    config: &AnonStatsServerConfig,
    operation: Option<AnonStatsOperation>,
    start_timestamp: Option<DateTime<Utc>>,
) -> Result<BucketStatistics> {
    let job_size = job.len();
    let job_data = job.into_bundles();
    let translated_thresholds = match operation {
        Some(AnonStatsOperation::Reauth) => {
            calculate_iris_threshold_a(config.n_buckets_1d_reauth, MATCH_THRESHOLD_RATIO_REAUTH)
        }
        None | Some(AnonStatsOperation::Uniqueness) => {
            calculate_iris_threshold_a(config.n_buckets_1d, MATCH_THRESHOLD_RATIO)
        }
    };

    // execute anon stats MPC protocol
    let bucket_result_shares = compare_min_threshold_buckets(
        session,
        translated_thresholds.as_slice(),
        job_data.as_slice(),
    )
    .await?;

    let buckets = open_ring(session, &bucket_result_shares).await?;
    let mut anon_stats = BucketStatistics::new(
        job_size,
        config.n_buckets_1d,
        config.party_id,
        origin.side,
        AnonStatsResultSource::Aggregator,
        operation,
    );
    anon_stats.fill_buckets(&buckets, MATCH_THRESHOLD_RATIO, start_timestamp);
    Ok(anon_stats)
}

pub mod test_helper {
    use ampc_secret_sharing::shares::{share::DistanceShare, RingElement, Share};
    use itertools::Itertools;

    use crate::{BucketResult, BucketStatistics, DistanceBundle1D};

    pub struct TestDistances {
        pub distances: Vec<Vec<[i16; 2]>>,
        pub shares0: Vec<DistanceBundle1D>,
        pub shares1: Vec<DistanceBundle1D>,
        pub shares2: Vec<DistanceBundle1D>,
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
                    let mshare3: u16 = (x[1] as u16).wrapping_sub(mshare1).wrapping_sub(mshare2);
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
            TestDistances {
                distances: bundles,
                shares0: bundle_shares1,
                shares1: bundle_shares2,
                shares2: bundle_shares3,
            }
        }

        pub fn ground_truth_buckets(&self, translated_thresholds: &[u32]) -> BucketStatistics {
            let thresholds = translated_thresholds
                .iter()
                .map(|&t| 0.5f64 - (t as f64) / (2f64 * 65536f64))
                .collect_vec();
            let num_buckets = translated_thresholds.len();
            let expected = self
                .distances
                .iter()
                .map(|group| {
                    // reduce distances in each group
                    group
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
                        .expect("Expected at least one distance in the group")
                })
                .fold(vec![0; num_buckets], |mut acc, x| {
                    let code_dist = x[0];
                    let mask_dist = x[1];
                    let dist = 0.5f64 - (code_dist as f64) / (2f64 * mask_dist as f64);
                    for (i, &threshold) in translated_thresholds.iter().enumerate() {
                        acc[i] += if dist < 0.5f64 - threshold as f64 / (2f64 * 65536f64) {
                            1
                        } else {
                            0
                        };
                    }
                    acc
                });

            let mut stats = BucketStatistics::new(
                self.distances.len(),
                translated_thresholds.len(),
                0,
                None,
                crate::types::AnonStatsResultSource::Aggregator,
                None,
            );
            let bucket_results = std::iter::once(BucketResult {
                count: expected[0],
                hamming_distance_bucket: [0f64, thresholds[0]],
            })
            .chain(
                thresholds
                    .windows(2)
                    .enumerate()
                    .map(|(i, window)| BucketResult {
                        count: expected[i + 1] - expected[i],
                        hamming_distance_bucket: [window[0], window[1]],
                    }),
            )
            .collect();
            stats.buckets = bucket_results;
            stats
        }
    }
}

#[cfg(test)]
mod tests {
    use ampc_actor_utils::{constants::MATCH_THRESHOLD_RATIO, execution::local::LocalRuntime};
    use rand::thread_rng;

    use crate::{
        anon_stats::{calculate_iris_threshold_a, iris_1d::test_helper::TestDistances},
        AnonStatsOrigin, AnonStatsServerConfig,
    };

    #[tokio::test]
    async fn test_1d_distances() {
        let sessions = LocalRuntime::mock_sessions_with_channel().await.unwrap();
        let num_buckets_1d = 10;
        let thresholds = calculate_iris_threshold_a(num_buckets_1d, MATCH_THRESHOLD_RATIO);

        let config = AnonStatsServerConfig {
            party_id: 0,
            face_bucket_thresholds: vec![],
            service: None,
            aws: None,
            environment: "test".to_string(),
            results_topic_arn: "foo".to_string(),
            n_buckets_1d: num_buckets_1d,
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
            let origin = AnonStatsOrigin {
                side: Some(crate::types::Eye::Left),
                orientation: crate::AnonStatsOrientation::Normal,
                context: crate::AnonStatsContext::GPU,
            };

            tasks.push(tokio::task::spawn(async move {
                let mut session = net.lock().await;
                let shares = shares
                    .into_iter()
                    .enumerate()
                    .map(|(idx, s)| (idx as i64, s))
                    .collect();
                let job = crate::AnonStatsMapping::new(shares);

                let stats = crate::anon_stats::iris_1d::process_1d_anon_stats_job(
                    &mut session,
                    job,
                    &origin,
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
}
