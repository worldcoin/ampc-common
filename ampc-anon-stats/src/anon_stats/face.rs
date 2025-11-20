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

pub type FaceDistanceVec = Vec<Share<u16>>;

fn build_thresholds(config: &AnonStatsServerConfig) -> Vec<u16> {
    (config.face_threshold_start..=config.face_threshold_end)
        .step_by(config.face_threshold_step as usize)
        .collect()
}

pub async fn process_face_distance_job(
    session: &mut Session,
    job: FaceDistanceVec,
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
            sub_pub(session, share, RingElement(threshold));
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
    for (bucket, threshold) in buckets_opened.into_iter().zip_eq(thresholds.into_iter()) {
        stats.buckets.push(BucketResult {
            count: bucket as usize,
            hamming_distance_bucket: [
                threshold as f64,
                (threshold + config.face_threshold_step) as f64,
            ],
        });
    }

    Ok(stats)
}
