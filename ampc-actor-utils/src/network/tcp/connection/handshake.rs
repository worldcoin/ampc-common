use crate::{
    execution::player::Identity,
    network::tcp::{ConnectError, ConnectionId, NetworkConnection},
};
use eyre::bail;
use eyre::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const HANDSHAKE_OK: &[u8] = b"2ok";
const MAX_PEER_ID_LENGTH: u32 = 128;

pub async fn outbound<T: NetworkConnection>(
    stream: &mut T,
    own_id: &Identity,
    connection_id: &ConnectionId,
) -> Result<(), ConnectError> {
    // Perform handshake: send our StreamId, expect to read it back
    stream
        .write_u32(connection_id.0)
        .await
        .map_err(|e| ConnectError::HandshakeError(format!("Failed to write stream_id: {:?}", e)))?;

    let echoed_id = stream
        .read_u32()
        .await
        .map_err(|e| ConnectError::HandshakeError(format!("Failed to read stream_id: {:?}", e)))?;

    if echoed_id != connection_id.0 {
        return Err(ConnectError::HandshakeError(format!(
            "expected stream_id {}, got {}",
            connection_id.0, echoed_id
        )));
    }

    let own_id_bytes = own_id.0.as_bytes();
    let own_id_len = own_id_bytes.len() as u32;

    stream.write_u32(own_id_len).await.map_err(|e| {
        ConnectError::HandshakeError(format!("Failed to write own_id length: {:?}", e))
    })?;

    stream.write_all(own_id_bytes).await.map_err(|e| {
        ConnectError::HandshakeError(format!("Failed to write own_id bytes: {:?}", e))
    })?;

    Ok(())
}

pub async fn inbound<T: NetworkConnection>(stream: &mut T) -> Result<(Identity, ConnectionId)> {
    let connection_id = match stream.read_u32().await {
        Ok(id) => id,
        Err(e) => {
            bail!("Failed to read stream_id: {:?}", e);
        }
    };
    if let Err(e) = stream.write_u32(connection_id).await {
        bail!("Failed to write stream_id back: {:?}", e);
    }
    let peer_id_length = match stream.read_u32().await {
        Ok(id) => id,
        Err(e) => {
            bail!("Failed to read peer_id length: {:?}", e);
        }
    };
    if peer_id_length > MAX_PEER_ID_LENGTH {
        bail!("invalid peer_id_length. too long: {}", peer_id_length);
    }
    let mut peer_id_bytes = vec![0u8; peer_id_length as usize];
    if let Err(e) = stream.read_exact(&mut peer_id_bytes).await {
        bail!("Failed to read peer_id bytes: {:?}", e);
    }
    let peer_id = match String::from_utf8(peer_id_bytes) {
        Ok(s) => Identity(s),
        Err(e) => {
            bail!("Failed to parse peer_id bytes as UTF-8: {:?}", e);
        }
    };
    Ok((peer_id, connection_id.into()))
}

pub async fn outbound_ok<T: NetworkConnection>(stream: &mut T) -> Result<(), ConnectError> {
    let mut rsp = [0; 3];
    let n = stream.read(&mut rsp[..]).await.map_err(|e| {
        ConnectError::HandshakeError(format!("Failed to read handshake response: {:?}", e))
    })?;

    if n != rsp.len() || &rsp[..n] != HANDSHAKE_OK {
        Err(ConnectError::HandshakeError(format!(
            "not accepted: rsp={:?}",
            &rsp[..n]
        )))
    } else {
        Ok(())
    }
}

pub async fn inbound_ok<T: NetworkConnection>(stream: &mut T) -> Result<()> {
    stream.write_all(HANDSHAKE_OK).await?;
    Ok(())
}
