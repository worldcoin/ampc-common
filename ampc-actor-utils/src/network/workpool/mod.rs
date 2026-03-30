use crate::network::{tcp::NetworkConnection, workpool::value::NetworkValue};
use bytes::BytesMut;
use std::array::TryFromSliceError;
use thiserror::Error;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf},
    sync::mpsc::{UnboundedReceiver, UnboundedSender},
};

pub mod leader;
mod payload;
pub mod value;
pub mod worker;

pub use payload::{Payload, ToBytes};

pub type WorkerId = u16;
pub type JobId = u32;

pub(crate) const MAX_PAYLOAD_SIZE: usize = 50 * 1024 * 1024; // 50MB

#[derive(Error, Debug, Clone, PartialEq)]
pub enum WorkpoolError {
    #[error("worker {worker_id} lost job {job_id}")]
    JobsLost { worker_id: WorkerId, job_id: JobId },
    #[error("worker {worker_id} response for job {job_id} was lost in transit")]
    ResponseLost { worker_id: WorkerId, job_id: JobId },
    #[error("lost connection to worker tasks")]
    SendFailed,
    #[error("input failed validation {0}")]
    InvalidInput(String),
    #[error("cancellation token triggered shutdown")]
    Cancelled,
    #[error("operation timed out")]
    Timeout,
}

// used when constructing a worker or leader handle
#[derive(Error, Debug)]
pub enum SetupError {
    #[error("connection error: {0}")]
    BadConfig(String),
    #[error("parse error: {0}")]
    InvalidAddress(String),
    #[error("error in TCP stack: {0}")]
    ListenFailed(String),
}

#[derive(Error, Debug)]
pub enum DeserializeError {
    #[error("io err")]
    IO(#[from] tokio::io::Error),
    #[error("invalid descriptor byte {0}")]
    InvalidDescriptorByte(u8),
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("parsing failed: {0}")]
    ParseFailed(#[from] TryFromSliceError),
}

#[derive(Error, Debug)]
pub(crate) enum Error {
    #[error("io err")]
    IO(#[from] tokio::io::Error),
    #[error("handshake failed. got {0}")]
    HandshakeFailed(String),
    #[error("deserialize failed: {0}")]
    Deserialize(#[from] DeserializeError),
    #[error("send failed")]
    SendFailed,
    #[error("invalid input: {0}")]
    InvalidInput(String),
}

// note that in the event of a disconnect, pre_send_hook could
// result in a message as being classified as "sent" when it wasn't,
// and then the job reconciliation (PendingJobsRequest) would mark the
// message as dropped.
//
// It is possible to re-enqueue messages which failed to send but this
// makes the code much more complex. Given that disconnects are rare, it
// seems acceptable to keep the code simple and let job reconciliation
// handle the lost messages.
pub(crate) async fn serialize_and_write_outbound<T: NetworkConnection, F>(
    mut stream: WriteHalf<T>,
    cmd_rx: &mut UnboundedReceiver<NetworkValue>,
    mut pre_send_hook: F,
) -> Result<(), Error>
where
    F: FnMut(&NetworkValue),
{
    const MAX_TO_BATCH: usize = 64 * 1024 * 1024;

    let mut buf = BytesMut::with_capacity(1024 * 64);
    while let Some(msg) = cmd_rx.recv().await {
        pre_send_hook(&msg);
        buf.clear();
        msg.serialize(&mut buf);

        // Try to batch more messages
        while let Ok(msg) = cmd_rx.try_recv() {
            pre_send_hook(&msg);
            msg.serialize(&mut buf);
            if buf.len() >= MAX_TO_BATCH {
                break;
            }
        }
        stream.write_all(&buf).await?;
        stream.flush().await?;
    }
    Ok(())
}

pub(crate) async fn read_and_parse_inbound<T, R, F>(
    mut reader: BufReader<ReadHalf<T>>,
    tx: &UnboundedSender<R>,
    mut handle_inbound_msg: F,
) -> Result<(), Error>
where
    T: NetworkConnection,
    F: FnMut(NetworkValue, &UnboundedSender<R>) -> Result<(), Error>,
{
    loop {
        let network_value = read_single_message(&mut reader).await?;
        handle_inbound_msg(network_value, tx)?;
    }
}

/// Read and parse a single network message from a reader.
/// Used by handshake functions and the main read loop.
pub(crate) async fn read_single_message<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<NetworkValue, DeserializeError> {
    use bytes::Bytes;
    use value::DescriptorByte;

    let mut buf = vec![0u8; 1];
    reader.read_exact(&mut buf).await?;
    let descriptor: DescriptorByte = buf[0]
        .try_into()
        .map_err(|_| DeserializeError::InvalidDescriptorByte(buf[0]))?;

    let header_len = descriptor.header_len();
    buf.resize(header_len, 0);
    reader.read_exact(&mut buf[1..header_len]).await?;

    let payload_len = match descriptor {
        DescriptorByte::Job => u32::from_le_bytes(buf[7..11].try_into().unwrap()) as usize,
        DescriptorByte::PendingJobsReply => {
            u32::from_le_bytes(buf[3..7].try_into().unwrap()) as usize
        }
        DescriptorByte::PendingJobsRequest | DescriptorByte::Cancel => 0,
    };

    if payload_len > MAX_PAYLOAD_SIZE {
        return Err(DeserializeError::InvalidInput(format!(
            "Payload size {} exceeds maximum {}",
            payload_len, MAX_PAYLOAD_SIZE
        )));
    }

    if payload_len > 0 {
        buf.resize(header_len + payload_len, 0);
        reader.read_exact(&mut buf[header_len..]).await?;
    }

    NetworkValue::deserialize(Bytes::from(buf))
}
