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
    let local = NetworkValue::Bytes(hash.to_vec());

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
            NetworkValue::Bytes(b) if b.len() == JOB_HASH_LEN => Ok(b.as_slice()),
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

pub async fn exchange_checkpoint_id(
    session: &mut Session,
    value: usize,
) -> eyre::Result<[usize; 3]> {
    let local = NetworkValue::Bytes(value.to_le_bytes().to_vec());

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

    let mut res = [0; 3];
    for (idx, nv) in all.into_iter().enumerate() {
        match nv {
            NetworkValue::Bytes(b) if b.len() == size_of::<usize>() => {
                let mut arr = [0u8; 8];
                arr.copy_from_slice(&b[..8]);
                res[idx] = usize::from_le_bytes(arr);
            }
            _ => bail!("Unexpected network value in exchange_checkpoint_id"),
        }
    }
    Ok(res)
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
}
