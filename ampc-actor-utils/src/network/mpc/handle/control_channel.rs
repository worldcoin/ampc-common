use async_trait::async_trait;
use eyre::{bail, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use crate::network::mpc::NetworkValue;
use crate::network::tcp::NetworkConnection;

const MAX_MESSAGE_SIZE: usize = 1024 * 1024; // 1MB
const SYNC_TOKEN_BYTES: &[u8] = &[b'o', b'k'];

/// A synchronization-safe, non-multiplexed channel for MPC control-plane messaging.
///
/// Unlike [`crate::execution::session::NetworkSession`], sends on a `ControlChannel`
/// block until the data has been written to the underlying stream and flushed — there
/// is no background dispatch. Use this when you need to know a message was delivered
/// before proceeding, for example when coordinating phase transitions between parties.
///
/// If the underlying TCP/TLS connection drops or errors during an operation, the
/// operation returns an error immediately. There is no automatic retry — call
/// [`crate::network::mpc::NetworkHandle::control_channel`] again to reconnect.
#[async_trait]
pub trait ControlChannel: Send {
    /// Send a value to the next party in the MPC ring. Blocks until flushed.
    async fn send_next(&mut self, value: NetworkValue) -> Result<()>;

    /// Send a value to the previous party in the MPC ring. Blocks until flushed.
    async fn send_prev(&mut self, value: NetworkValue) -> Result<()>;

    /// Receive a value from the next party. Blocks until a full message arrives.
    async fn recv_next(&mut self) -> Result<NetworkValue>;

    /// Receive a value from the previous party. Blocks until a full message arrives.
    async fn recv_prev(&mut self) -> Result<NetworkValue>;

    /// Ring barrier: send a sync token to both peers, then receive from both.
    ///
    /// All three parties must call `sync()` concurrently. Returns once this party
    /// has received a token from each neighbour. Returns an error (without retry)
    /// if either send or receive fails.
    ///
    /// Sends are issued before receives to avoid deadlock: because TCP buffers
    /// small messages, all parties can complete their sends before blocking on
    /// receives.
    async fn sync(&mut self) -> Result<()>;
}

/// [`ControlChannel`] implementation over a generic [`NetworkConnection`] stream.
///
/// Holds one dedicated stream per ring neighbour (next / prev). Constructed by
/// [`crate::network::mpc::NetworkHandle::control_channel`]. Works with both
/// [`crate::network::tcp::TcpStreamConn`] and
/// [`crate::network::tcp::TlsStreamConn`] via the generic parameter.
pub(crate) struct TcpControlChannel<T: NetworkConnection> {
    next_stream: T,
    prev_stream: T,
    shutdown_ct: CancellationToken,
}

impl<T: NetworkConnection> TcpControlChannel<T> {
    pub(super) fn new(next_stream: T, prev_stream: T, shutdown_ct: CancellationToken) -> Self {
        Self {
            next_stream,
            prev_stream,
            shutdown_ct,
        }
    }
}

/// Write a length-prefixed [`NetworkValue`] to `stream` and flush.
/// Returns an error if the payload exceeds `MAX_MESSAGE_SIZE` or if the shutdown token is triggered.
async fn write_value<T: NetworkConnection>(
    stream: &mut T,
    value: NetworkValue,
    shutdown_ct: &CancellationToken,
) -> Result<()> {
    let bytes = value.to_network();

    // Check if payload exceeds max message size
    if bytes.len() > MAX_MESSAGE_SIZE {
        bail!(
            "message size {} exceeds maximum allowed size {}",
            bytes.len(),
            MAX_MESSAGE_SIZE
        );
    }

    let len = (bytes.len() as u32).to_le_bytes();

    tokio::select! {
        result = async {
            stream.write_all(&len).await?;
            stream.write_all(&bytes).await?;
            stream.flush().await?;
            Ok::<(), eyre::Report>(())
        } => result,
        _ = shutdown_ct.cancelled() => {
            bail!("shutdown cancellation token triggered during write_value")
        }
    }
}

/// Read a length-prefixed [`NetworkValue`] from `stream`.
/// Returns an error if the payload exceeds `MAX_MESSAGE_SIZE` or if the shutdown token is triggered.
async fn read_value<T: NetworkConnection>(
    stream: &mut T,
    shutdown_ct: &CancellationToken,
) -> Result<NetworkValue> {
    tokio::select! {
        result = async {
            let mut len_buf = [0u8; 4];
            stream.read_exact(&mut len_buf).await?;
            let len = u32::from_le_bytes(len_buf) as usize;

            // Check if payload exceeds max message size
            if len > MAX_MESSAGE_SIZE {
                bail!(
                    "message size {} exceeds maximum allowed size {}",
                    len,
                    MAX_MESSAGE_SIZE
                );
            }

            let mut buf = vec![0u8; len];
            stream.read_exact(&mut buf).await?;
            NetworkValue::deserialize(&buf)
        } => result,
        _ = shutdown_ct.cancelled() => {
            bail!("shutdown cancellation token triggered during read_value")
        }
    }
}

#[async_trait]
impl<T: NetworkConnection> ControlChannel for TcpControlChannel<T> {
    async fn send_next(&mut self, value: NetworkValue) -> Result<()> {
        write_value(&mut self.next_stream, value, &self.shutdown_ct).await
    }

    async fn send_prev(&mut self, value: NetworkValue) -> Result<()> {
        write_value(&mut self.prev_stream, value, &self.shutdown_ct).await
    }

    async fn recv_next(&mut self) -> Result<NetworkValue> {
        read_value(&mut self.next_stream, &self.shutdown_ct).await
    }

    async fn recv_prev(&mut self) -> Result<NetworkValue> {
        read_value(&mut self.prev_stream, &self.shutdown_ct).await
    }

    async fn sync(&mut self) -> Result<()> {
        let token = NetworkValue::Bytes(SYNC_TOKEN_BYTES.to_vec());
        self.send_next(token.clone()).await?;
        self.send_prev(token).await?;

        let next_token = self.recv_next().await?;
        match next_token {
            NetworkValue::Bytes(ref bytes) if bytes == SYNC_TOKEN_BYTES => {}
            _ => bail!("invalid sync token received from next party"),
        }

        let prev_token = self.recv_prev().await?;
        match prev_token {
            NetworkValue::Bytes(ref bytes) if bytes == SYNC_TOKEN_BYTES => {}
            _ => bail!("invalid sync token received from prev party"),
        }

        Ok(())
    }
}
