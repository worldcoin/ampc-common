use crate::network::{tcp::NetworkConnection, workpool::value::NetworkValue};
use bytes::BytesMut;
use std::io;
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
    #[error("cancellation token triggred shutdown")]
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
pub(crate) enum LeaderError {
    #[error("io err")]
    IO(#[from] tokio::io::Error),
}

pub(crate) async fn serialize_and_write_outbound<T: NetworkConnection, F>(
    mut stream: WriteHalf<T>,
    cmd_rx: &mut UnboundedReceiver<NetworkValue>,
    mut pre_send_hook: F,
) -> io::Result<()>
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
) -> io::Result<()>
where
    T: NetworkConnection,
    F: FnMut(NetworkValue, &UnboundedSender<R>) -> io::Result<()>,
{
    use value::DescriptorByte;
    let mut buf = BytesMut::with_capacity(64 * 1024);

    loop {
        buf.clear();
        buf.resize(1, 0);

        // Read descriptor byte first
        reader.read_exact(&mut buf[..1]).await?;
        let descriptor: DescriptorByte = buf[0]
            .try_into()
            .map_err(|_| io::Error::other(format!("Invalid descriptor byte: {}", buf[0])))?;

        // Read remaining header bytes based on message type
        let header_len = descriptor.header_len();
        buf.resize(header_len, 0);
        reader.read_exact(&mut buf[1..header_len]).await?;

        const MAX_PAYLOAD_SIZE: usize = 500 * 1024 * 1024; // 500MB
        let payload_len = match descriptor {
            DescriptorByte::Job => u32::from_le_bytes(buf[7..11].try_into().unwrap()) as usize,
            DescriptorByte::PendingJobsReply => {
                u32::from_le_bytes(buf[3..7].try_into().unwrap()) as usize
            }
            _ => 0,
        };

        if payload_len > MAX_PAYLOAD_SIZE {
            return Err(io::Error::other(format!(
                "Payload size {} exceeds maximum {}",
                payload_len, MAX_PAYLOAD_SIZE
            )));
        }

        let total_len = header_len + payload_len;
        if payload_len > 0 {
            buf.resize(total_len, 0);
            reader.read_exact(&mut buf[header_len..total_len]).await?;
        }

        // Deserialize the NetworkValue (zero-copy via split/freeze)
        let bytes = buf.split_to(total_len).freeze();
        match NetworkValue::deserialize(bytes) {
            Ok(network_value) => {
                handle_inbound_msg(network_value, tx)?;
            }
            Err(e) => {
                return Err(io::Error::other(format!("Failed to deserialize: {}", e)));
            }
        }
    }
}
