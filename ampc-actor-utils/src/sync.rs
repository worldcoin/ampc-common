use crate::execution::player::Role;
use crate::execution::session::{NetworkSession, Session, SessionHandles};
use crate::network::mpc::NetworkValue;
use ampc_secret_sharing::shares::rss5::ORBIT5_PARTY_COUNT;
use eyre::{bail, ensure, eyre, WrapErr};
use std::{collections::HashSet, time::Duration};
use tokio::time::{timeout_at, Instant};

pub const JOB_HASH_LEN: usize = 32;

/// Exchange a request hash with all four peers in an ORBIT5 network session.
///
/// Returns success only after receiving a matching, 32-byte `Bytes` value from
/// every peer. Disagreement, malformed messages, and transport failures are errors.
/// `timeout` must be positive and bounds the entire exchange, including sends.
/// The session must assign distinct identities to exactly the roles `0..5`.
///
/// All parties must call this at the same point in their protocol, using the same
/// request-hash construction. This is a consistency check over point-to-point
/// messages, not Byzantine reliable broadcast: a peer can send different hashes
/// to different recipients. After an error or cancellation, discard the session;
/// a partial exchange may leave messages queued for subsequent operations.
pub async fn sync_on_job_hash_5pc(
    session: &mut NetworkSession,
    hash: &[u8; JOB_HASH_LEN],
    timeout: Duration,
) -> eyre::Result<()> {
    ensure!(
        session.role_assignments.len() == ORBIT5_PARTY_COUNT,
        "Job hash agreement requires exactly {ORBIT5_PARTY_COUNT} parties"
    );
    let own_id = session.own_role.index();
    ensure!(own_id < ORBIT5_PARTY_COUNT, "Invalid own party ID {own_id}");
    let mut identities = HashSet::with_capacity(ORBIT5_PARTY_COUNT);
    for party in 0..ORBIT5_PARTY_COUNT {
        let identity = session
            .identity(&Role::new(party))
            .wrap_err_with(|| format!("Missing role for party {party} in job hash agreement"))?;
        ensure!(
            identities.insert(identity),
            "Duplicate identity for party {party} in job hash agreement"
        );
    }
    ensure!(
        !timeout.is_zero(),
        "Job hash agreement timeout must be positive"
    );
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| eyre!("Job hash agreement timeout is too large"))?;
    let peers: Vec<_> = (0..ORBIT5_PARTY_COUNT)
        .filter(|&party| party != own_id)
        .collect();
    let local = NetworkValue::Bytes(hash.to_vec().into());

    for &peer in &peers {
        timeout_at(deadline, session.send_to(local.clone(), &Role::new(peer)))
            .await
            .map_err(|_| eyre!("Job hash agreement timed out sending to party {peer}"))?
            .wrap_err_with(|| format!("Failed to send job hash to party {peer}"))?;
    }
    for peer in peers {
        let value = timeout_at(deadline, session.receive_from(&Role::new(peer)))
            .await
            .map_err(|_| eyre!("Job hash agreement timed out receiving from party {peer}"))?
            .wrap_err_with(|| format!("Failed to receive job hash from party {peer}"))?;
        let bytes = match value {
            NetworkValue::Bytes(bytes) if bytes.len() == JOB_HASH_LEN => bytes,
            _ => bail!(
                "Malformed job hash from party {peer}: expected {JOB_HASH_LEN}-byte Bytes value"
            ),
        };
        ensure!(
            bytes.as_ref() == hash,
            "Job hash disagreement with party {peer}"
        );
    }
    Ok(())
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::local::{generate_local_identities_orbit5, LocalRuntime};
    use crate::execution::player::{Identity, RoleAssignment};
    use crate::execution::session::SessionId;
    use crate::network::mpc::{LocalNetworkingStore, Networking};
    use std::sync::Arc;
    use tokio::{task::JoinSet, time::timeout};

    const TEST_TIMEOUT: Duration = Duration::from_secs(5);
    const AGREEMENT_TIMEOUT: Duration = Duration::from_secs(2);
    const HASH: [u8; JOB_HASH_LEN] = [0xAB; JOB_HASH_LEN];

    fn five_party_sessions() -> Vec<NetworkSession> {
        let identities = generate_local_identities_orbit5();
        let assignments: Arc<RoleAssignment> = Arc::new(
            identities
                .iter()
                .enumerate()
                .map(|(party, identity)| (Role::new(party), identity.clone()))
                .collect(),
        );
        let network = LocalNetworkingStore::from_host_ids(&identities);
        identities
            .into_iter()
            .enumerate()
            .map(|(party, identity)| NetworkSession {
                session_id: SessionId(0),
                role_assignments: Arc::clone(&assignments),
                networking: Box::new(network.get_local_network(identity)),
                own_role: Role::new(party),
            })
            .collect()
    }

    #[tokio::test]
    async fn five_party_matching_hashes_succeed_across_repeated_exchanges() -> eyre::Result<()> {
        let mut jobs = JoinSet::new();
        for mut session in five_party_sessions() {
            jobs.spawn(async move {
                for hash in [HASH, [0xCD; JOB_HASH_LEN]] {
                    sync_on_job_hash_5pc(&mut session, &hash, AGREEMENT_TIMEOUT).await?;
                }
                Ok::<_, eyre::Report>(())
            });
        }
        for result in timeout(TEST_TIMEOUT, jobs.join_all()).await? {
            result?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn five_party_disagreement_from_any_party_fails_everywhere() -> eyre::Result<()> {
        for different_party in 0..ORBIT5_PARTY_COUNT {
            let mut jobs = JoinSet::new();
            for mut session in five_party_sessions() {
                let hash = if session.own_role.index() == different_party {
                    [0xCD; JOB_HASH_LEN]
                } else {
                    HASH
                };
                jobs.spawn(async move {
                    sync_on_job_hash_5pc(&mut session, &hash, AGREEMENT_TIMEOUT).await
                });
            }
            for result in timeout(TEST_TIMEOUT, jobs.join_all()).await? {
                let error = result.unwrap_err();
                assert!(error
                    .to_string()
                    .contains("Job hash disagreement with party"));
                assert!(!error.to_string().contains(&hex::encode(HASH)));
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn five_party_malformed_messages_are_rejected() -> eyre::Result<()> {
        for malformed in [
            NetworkValue::Bytes(Vec::new().into()),
            NetworkValue::Bytes(vec![0; JOB_HASH_LEN - 1].into()),
            NetworkValue::Bytes(vec![0; JOB_HASH_LEN + 1].into()),
            NetworkValue::PrfKey([0; 16]),
        ] {
            for faulty_party in 0..ORBIT5_PARTY_COUNT {
                let mut sessions = five_party_sessions();
                let mut faulty = sessions.remove(faulty_party);
                // Queue malformed input for each honest party through the real codec.
                for peer in 0..ORBIT5_PARTY_COUNT {
                    if peer != faulty_party {
                        faulty.send_to(malformed.clone(), &Role::new(peer)).await?;
                    }
                }
                let mut jobs = JoinSet::new();
                for mut session in sessions {
                    jobs.spawn(async move {
                        sync_on_job_hash_5pc(&mut session, &HASH, AGREEMENT_TIMEOUT).await
                    });
                }
                for result in timeout(TEST_TIMEOUT, jobs.join_all()).await? {
                    assert!(result
                        .unwrap_err()
                        .to_string()
                        .contains(&format!("Malformed job hash from party {faulty_party}")));
                }
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn five_party_missing_peer_times_out_for_every_other_party() -> eyre::Result<()> {
        for missing_party in 0..ORBIT5_PARTY_COUNT {
            let mut sessions = five_party_sessions();
            // The local transport retains channels, so dropping a peer models silence
            // rather than a disconnect. The helper must enforce its own deadline.
            sessions.remove(missing_party);
            let mut jobs = JoinSet::new();
            for mut session in sessions {
                jobs.spawn(async move {
                    sync_on_job_hash_5pc(&mut session, &HASH, Duration::from_millis(100)).await
                });
            }
            for result in timeout(TEST_TIMEOUT, jobs.join_all()).await? {
                assert!(result
                    .unwrap_err()
                    .to_string()
                    .contains(&format!("timed out receiving from party {missing_party}")));
            }
        }
        Ok(())
    }

    enum NetworkFault {
        SendError,
        ReceiveError,
        StalledSend,
        SlowSends,
        SlowReceives,
    }

    #[async_trait::async_trait]
    impl Networking for NetworkFault {
        async fn send(&mut self, _value: NetworkValue, _receiver: &Identity) -> eyre::Result<()> {
            match self {
                Self::SendError => bail!("injected send failure"),
                Self::StalledSend => std::future::pending().await,
                Self::SlowSends => {
                    tokio::time::sleep(Duration::from_millis(40)).await;
                    Ok(())
                }
                _ => Ok(()),
            }
        }

        async fn receive(&mut self, _sender: &Identity) -> eyre::Result<NetworkValue> {
            match self {
                Self::ReceiveError => bail!("injected receive failure"),
                Self::SlowReceives => tokio::time::sleep(Duration::from_millis(40)).await,
                _ => {}
            }
            Ok(NetworkValue::Bytes(HASH.to_vec().into()))
        }
    }

    #[tokio::test]
    async fn five_party_transport_errors_include_operation_and_party() -> eyre::Result<()> {
        for (fault, context, cause) in [
            (
                NetworkFault::SendError,
                "Failed to send job hash to party 1",
                "injected send failure",
            ),
            (
                NetworkFault::ReceiveError,
                "Failed to receive job hash from party 1",
                "injected receive failure",
            ),
        ] {
            let mut session = five_party_sessions().remove(0);
            session.networking = Box::new(fault);
            let error = timeout(
                TEST_TIMEOUT,
                sync_on_job_hash_5pc(&mut session, &HASH, AGREEMENT_TIMEOUT),
            )
            .await?
            .unwrap_err();
            assert_eq!(error.to_string(), context);
            assert!(format!("{error:#}").contains(cause));
        }
        Ok(())
    }

    #[tokio::test]
    async fn five_party_deadline_covers_blocked_sends_and_the_whole_exchange() -> eyre::Result<()> {
        for (fault, operation) in [
            (NetworkFault::StalledSend, "sending"),
            (NetworkFault::SlowSends, "sending"),
            (NetworkFault::SlowReceives, "receiving"),
        ] {
            let mut session = five_party_sessions().remove(0);
            session.networking = Box::new(fault);
            let error = timeout(
                TEST_TIMEOUT,
                sync_on_job_hash_5pc(&mut session, &HASH, Duration::from_millis(100)),
            )
            .await?
            .unwrap_err();
            assert!(error
                .to_string()
                .contains(&format!("timed out {operation}")));
        }
        Ok(())
    }

    #[tokio::test]
    async fn five_party_invalid_membership_is_rejected_before_network_io() -> eyre::Result<()> {
        for case in 0..5 {
            let mut session = five_party_sessions().remove(0);
            session.networking = Box::new(NetworkFault::SendError);
            let assignments = Arc::make_mut(&mut session.role_assignments);
            let expected = match case {
                0 => {
                    assignments.remove(&Role::new(4));
                    "requires exactly 5 parties"
                }
                1 => {
                    assignments.insert(Role::new(5), Identity::from("extra"));
                    "requires exactly 5 parties"
                }
                2 => {
                    session.own_role = Role::new(5);
                    "Invalid own party ID"
                }
                3 => {
                    let identity = assignments.remove(&Role::new(4)).unwrap();
                    assignments.insert(Role::new(5), identity);
                    "Missing role for party 4"
                }
                4 => {
                    let identity = assignments[&Role::new(0)].clone();
                    assignments.insert(Role::new(4), identity);
                    "Duplicate identity for party 4"
                }
                _ => unreachable!(),
            };
            let error = sync_on_job_hash_5pc(&mut session, &HASH, AGREEMENT_TIMEOUT)
                .await
                .unwrap_err();
            assert!(error.to_string().contains(expected), "{error:#}");
        }
        let mut session = five_party_sessions().remove(0);
        session.networking = Box::new(NetworkFault::SendError);
        let error = sync_on_job_hash_5pc(&mut session, &HASH, Duration::ZERO)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("timeout must be positive"));
        Ok(())
    }

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
}
