use crate::execution::session::Session;
use crate::network::mpc::NetworkValue;
use eyre::bail;

pub const JOB_HASH_LEN: usize = 32;

/// Broadcast a fixed-size hash digest and check that all parties agree.
///
/// Each party sends its local `hash` (e.g. SHA-256 of an SNS MessageId) to the
/// other two parties and receives theirs.  Returns `Ok(true)` when all three
/// digests are identical, `Ok(false)` on mismatch.
pub async fn sync_on_job_hash(
    session: &mut Session,
    hash: &[u8; JOB_HASH_LEN],
) -> eyre::Result<bool> {
    tracing::info!("Synchronizing on job hash: {}", hex::encode(hash));
    let local = NetworkValue::Bytes(hash.to_vec().into());

    session.network_session.send_next(local.clone()).await?;
    session.network_session.send_prev(local.clone()).await?;

    let from_next = session.network_session.receive_next().await?;
    let from_prev = session.network_session.receive_prev().await?;

    let all = match session.network_session.own_role.index() {
        0 => [local, from_next, from_prev],
        1 => [from_prev, local, from_next],
        2 => [from_next, from_prev, local],
        _ => bail!("Invalid party id"),
    };

    let all_bytes: Vec<&[u8]> = all
        .iter()
        .map(|nv| match nv {
            NetworkValue::Bytes(b) if b.len() == JOB_HASH_LEN => Ok(&b[..]),
            _ => Err(eyre::eyre!(
                "Unexpected network value in job hash sync (expected {JOB_HASH_LEN} bytes)"
            )),
        })
        .collect::<eyre::Result<_>>()?;

    if all_bytes[0] == all_bytes[1] && all_bytes[1] == all_bytes[2] {
        Ok(true)
    } else {
        tracing::error!(
            "Mismatched job hashes: party0={}, party1={}, party2={}",
            hex::encode(all_bytes[0]),
            hex::encode(all_bytes[1]),
            hex::encode(all_bytes[2]),
        );
        Ok(false)
    }
}

/// Agree on request identity and accept only when every party's input is valid.
///
/// Invalid input returns `Ok(false)`; mismatched identities, malformed frames,
/// and transport failures return an error. Callers must bound the whole operation
/// with a timeout and discard the session after an error or cancellation.
pub async fn sync_on_job_validity(
    session: &mut Session,
    hash: &[u8; JOB_HASH_LEN],
    valid: bool,
) -> eyre::Result<bool> {
    let mut frame = hash.to_vec();
    frame.push(u8::from(valid));
    let local = NetworkValue::Bytes(frame.into());
    session.network_session.send_next(local.clone()).await?;
    session.network_session.send_prev(local).await?;

    let from_next = session.network_session.receive_next().await?;
    let from_prev = session.network_session.receive_prev().await?;
    let mut all_valid = valid;
    for peer in [from_next, from_prev] {
        let NetworkValue::Bytes(bytes) = peer else {
            bail!("Unexpected network value in job validity sync (expected bytes)");
        };
        if bytes.len() != JOB_HASH_LEN + 1 {
            bail!("Unexpected frame length in job validity sync");
        }
        if bytes[..JOB_HASH_LEN] != hash[..] {
            bail!("Mismatched job hashes in job validity sync");
        }
        match bytes[JOB_HASH_LEN] {
            0 => all_valid = false,
            1 => {}
            _ => bail!("Invalid validity flag in job validity sync"),
        }
    }
    Ok(all_valid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::local::LocalRuntime;

    #[tokio::test]
    async fn test_sync_on_job_hash_matching() {
        let rt = LocalRuntime::mock_setup_with_channel().await.unwrap();
        let hash: [u8; JOB_HASH_LEN] = [0xAB; JOB_HASH_LEN];

        let mut handles = Vec::new();
        for mut session in rt.sessions {
            let h = hash;
            handles.push(tokio::spawn(async move {
                sync_on_job_hash(&mut session, &h).await.unwrap()
            }));
        }

        for handle in handles {
            assert!(handle.await.unwrap(), "all parties should agree");
        }
    }

    #[tokio::test]
    async fn test_sync_on_job_hash_mismatch() {
        let rt = LocalRuntime::mock_setup_with_channel().await.unwrap();
        let good_hash: [u8; JOB_HASH_LEN] = [0xAB; JOB_HASH_LEN];
        let bad_hash: [u8; JOB_HASH_LEN] = [0xCD; JOB_HASH_LEN];

        let mut handles = Vec::new();
        for (i, mut session) in rt.sessions.into_iter().enumerate() {
            let h = if i == 2 { bad_hash } else { good_hash };
            handles.push(tokio::spawn(async move {
                sync_on_job_hash(&mut session, &h).await.unwrap()
            }));
        }

        for handle in handles {
            assert!(!handle.await.unwrap(), "mismatch should be detected");
        }
    }

    #[tokio::test]
    async fn job_validity_agrees_and_keeps_successive_requests_aligned() {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let mut rt = LocalRuntime::mock_setup_with_channel().await.unwrap();
            for validity in [
                [true, true, true],
                [false, false, false],
                [false, true, true],
                [true, false, true],
                [true, true, false],
                [true, true, true],
            ] {
                let expected = validity.iter().all(|valid| *valid);
                let results = futures::future::join_all(rt.sessions.iter_mut().enumerate().map(
                    |(party, session)| {
                        sync_on_job_validity(session, &[0xAB; JOB_HASH_LEN], validity[party])
                    },
                ))
                .await;
                for result in results {
                    assert_eq!(result.unwrap(), expected);
                }
            }
        })
        .await
        .expect("validity agreement timed out");
    }

    #[tokio::test]
    async fn job_validity_hash_mismatch_is_an_error_even_for_invalid_input() {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let rt = LocalRuntime::mock_setup_with_channel().await.unwrap();
            let results = futures::future::join_all(rt.sessions.into_iter().enumerate().map(
                |(party, mut session)| async move {
                    let hash = [if party == 2 { 0xCD } else { 0xAB }; JOB_HASH_LEN];
                    sync_on_job_validity(&mut session, &hash, false).await
                },
            ))
            .await;
            for result in results {
                assert!(result
                    .unwrap_err()
                    .to_string()
                    .contains("Mismatched job hashes"));
            }
        })
        .await
        .expect("mismatch detection timed out");
    }

    #[tokio::test]
    async fn job_validity_rejects_malformed_frames() {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let mut bad_flag = vec![0xAB; JOB_HASH_LEN];
            bad_flag.push(2);
            for malformed in [
                NetworkValue::NetworkVec(vec![]),
                NetworkValue::Bytes(vec![0xAB; JOB_HASH_LEN].into()),
                NetworkValue::Bytes(vec![0xAB; JOB_HASH_LEN + 2].into()),
                NetworkValue::Bytes(bad_flag.into()),
            ] {
                let mut rt = LocalRuntime::mock_setup_with_channel().await.unwrap();
                for peer in rt.sessions.iter_mut().skip(1) {
                    peer.network_session
                        .send_next(malformed.clone())
                        .await
                        .unwrap();
                    peer.network_session
                        .send_prev(malformed.clone())
                        .await
                        .unwrap();
                }
                assert!(
                    sync_on_job_validity(&mut rt.sessions[0], &[0xAB; JOB_HASH_LEN], false)
                        .await
                        .is_err()
                );
            }
        })
        .await
        .expect("malformed frame detection timed out");
    }

    #[tokio::test]
    async fn job_validity_propagates_transport_failure() {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            let mut rt = LocalRuntime::mock_setup_with_channel().await.unwrap();
            let network = crate::network::mpc::LocalNetworkingStore::from_host_ids(&[]);
            rt.sessions[0].network_session.networking =
                Box::new(network.get_local_network("alice".into()));
            let error = sync_on_job_validity(&mut rt.sessions[0], &[0xAB; JOB_HASH_LEN], false)
                .await
                .unwrap_err();
            assert!(error.to_string().contains("p2p channel retrieve error"));
        })
        .await
        .expect("transport failure propagation timed out");
    }
}
