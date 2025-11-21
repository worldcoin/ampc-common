use ampc_actor_utils::execution::session::Session;
use ampc_actor_utils::protocol::binary::{bit_inject_ot_2round, extract_msb_u16_batch};
use ampc_actor_utils::protocol::ops::{open_ring, sub_pub};
use ampc_secret_sharing::shares::VecShare;
use ampc_secret_sharing::{RingElement, Share};
use ampc_server_utils::{AnonStatsResultSource, BucketResult, BucketStatistics, Eye};
use chrono::Utc;
use eyre::Result;
use itertools::Itertools;

use crate::{AnonStatsOrigin, AnonStatsServerConfig};

pub type FaceDistance = Share<u16>;

fn build_thresholds(config: &AnonStatsServerConfig) -> Vec<i16> {
    (config.face_threshold_start..=config.face_threshold_end)
        .step_by(config.face_threshold_step as usize)
        .collect()
}

pub async fn process_face_distance_job(
    session: &mut Session,
    job: Vec<FaceDistance>,
    _origin: &AnonStatsOrigin,
    config: &AnonStatsServerConfig,
) -> Result<BucketStatistics> {
    let thresholds = build_thresholds(config);
    let n_buckets = thresholds.len();
    let job_size = job.len();

    let mut buckets = Vec::with_capacity(thresholds.len());
    for &threshold in &thresholds {
        let mut bucket_distances = job.clone();
        bucket_distances.iter_mut().for_each(|share| {
            sub_pub(session, share, RingElement(threshold as u16));
        });

        let bits = extract_msb_u16_batch(session, &bucket_distances).await?;
        let sums: VecShare<u32> = bit_inject_ot_2round(session, VecShare::new_vec(bits)).await?;
        let sums = sums.inner();
        let sum = sums
            .into_iter()
            .fold(Share::<u32>::default(), |acc, x| acc + x);
        buckets.push(sum);
    }
    // open the buckets
    let buckets_opened = open_ring(session, &buckets).await?;

    // build a BucketStatistics from this
    // TODO: the BucketStatistics is almost reusable, except for the Eye field which does not make sense here.
    let mut stats = BucketStatistics::new(job_size, n_buckets, config.party_id, Eye::Left);
    stats.source = AnonStatsResultSource::Aggregator;
    stats.start_time_utc_timestamp = Utc::now();

    // we want non-cumulative data, so we iterate over pairs of (current, next) and subtract the start of the end
    for ((bucket, threshold), (bucket_end, threshold_end)) in buckets_opened
        .into_iter()
        .zip_eq(thresholds.into_iter())
        .tuple_windows()
    {
        stats.buckets.push(BucketResult {
            count: bucket_end as usize - bucket as usize,
            hamming_distance_bucket: [threshold as f64, threshold_end as f64],
        });
    }

    Ok(stats)
}

pub mod test_helper {
    use ampc_secret_sharing::{RingElement, Share};
    use ampc_server_utils::BucketResult;

    use crate::anon_stats::face::FaceDistance;

    pub struct TestDistances {
        pub distances: Vec<i16>,
        pub shares0: Vec<FaceDistance>,
        pub shares1: Vec<FaceDistance>,
        pub shares2: Vec<FaceDistance>,
    }

    impl TestDistances {
        pub fn generate_ground_truth_input(
            rng: &mut impl rand::Rng,
            num_distances: usize,
        ) -> TestDistances {
            let distances: Vec<i16> = (0..num_distances)
                .map(|_| rng.gen_range(-1000..=5000))
                .collect();
            let mut shares0 = Vec::with_capacity(num_distances);
            let mut shares1 = Vec::with_capacity(num_distances);
            let mut shares2 = Vec::with_capacity(num_distances);

            for &distance in &distances {
                let share0: u16 = rng.gen();
                let share1: u16 = rng.gen();
                let share2: u16 = distance
                    .wrapping_sub(share0 as i16)
                    .wrapping_sub(share1 as i16) as u16;
                let share0 = RingElement(share0);
                let share1 = RingElement(share1);
                let share2 = RingElement(share2);

                shares0.push(Share {
                    a: share0,
                    b: share2,
                });
                shares1.push(Share {
                    a: share1,
                    b: share0,
                });
                shares2.push(Share {
                    a: share2,
                    b: share1,
                });
            }

            TestDistances {
                distances,
                shares0,
                shares1,
                shares2,
            }
        }

        pub fn ground_truth_buckets(&self, thresholds: &[i16]) -> Vec<BucketResult> {
            let mut buckets = vec![0; thresholds.len()];
            for &distance in &self.distances {
                for (i, &threshold) in thresholds.iter().enumerate() {
                    if distance < threshold as i16 {
                        buckets[i] += 1;
                    }
                }
            }
            // de-accumulate
            let bucket_results = thresholds
                .windows(2)
                .enumerate()
                .map(|(i, window)| BucketResult {
                    count: buckets[i + 1] - buckets[i],
                    hamming_distance_bucket: [window[0] as f64, window[1] as f64],
                })
                .collect();
            bucket_results
        }
    }
}

#[cfg(test)]
mod tests {
    use ampc_actor_utils::execution::local::LocalRuntime;
    use rand::thread_rng;

    use crate::{
        anon_stats::face::{build_thresholds, test_helper::TestDistances},
        AnonStatsContext, AnonStatsOrientation, AnonStatsOrigin, AnonStatsServerConfig,
    };

    #[tokio::test]
    async fn test_face_distances() {
        let sessions = LocalRuntime::mock_sessions_with_channel().await.unwrap();

        let config = AnonStatsServerConfig {
            party_id: 0,
            face_threshold_start: -2000,
            face_threshold_end: 5000,
            face_threshold_step: 200,
            service: None,
            aws: None,
            environment: "test".to_string(),
            results_topic_arn: "foo".to_string(),
            n_buckets_1d: 0,
            min_1d_job_size: 0,
            poll_interval_secs: 10,
            max_sync_failures_before_reset: 10,
            db_url: "foo".to_string(),
            db_schema_name: "foo".to_string(),
            server_coordination: None,
            service_ports: Vec::new(),
            shutdown_last_results_sync_timeout_secs: 10,
        };
        let thresholds = build_thresholds(&config);
        let ground_truth = TestDistances::generate_ground_truth_input(&mut thread_rng(), 10000);
        let ground_truth_buckets = ground_truth.ground_truth_buckets(&thresholds);
        let TestDistances {
            distances: _,
            shares0,
            shares1,
            shares2,
        } = ground_truth;
        let anon_stats_origin = AnonStatsOrigin {
            side: None,
            orientation: AnonStatsOrientation::Normal,
            context: AnonStatsContext::FACE,
        };

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

                let stats = crate::anon_stats::face::process_face_distance_job(
                    &mut session,
                    shares,
                    &anon_stats_origin,
                    &config,
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
                ground_truth_buckets.len(),
                "Number of buckets mismatch"
            );
            for (i, bucket) in stats.buckets.iter().enumerate() {
                assert_eq!(
                    bucket, &ground_truth_buckets[i],
                    "Bucket {} mismatch: expected {:?}, got {:?}",
                    i, ground_truth_buckets[i], bucket
                );
            }
        }
    }
}
