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
    #[error("worker {worker_id} lost jobs: expected last received job_id {expected}, but worker reports {actual:?}")]
    JobsLost {
        worker_id: WorkerId,
        expected: JobId,
        actual: Option<JobId>,
    },
    #[error("lost connection to worker tasks")]
    SendFailed,
    #[error("Input failed validation {0}")]
    InvalidInput(String),
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
    #[error("bad response: {0}")]
    BadResponse(String),
    #[error("io err")]
    IO(#[from] tokio::io::Error),
}

pub(crate) async fn handle_outbound_traffic<T: NetworkConnection, F>(
    mut stream: WriteHalf<T>,
    cmd_rx: &mut UnboundedReceiver<NetworkValue>,
    mut on_send: F,
) -> io::Result<()>
where
    F: FnMut(&NetworkValue),
{
    const BUFFER_CAPACITY: usize = 64 * 1024 * 1024;

    let mut buf = BytesMut::with_capacity(1024 * 64);
    while let Some(msg) = cmd_rx.recv().await {
        on_send(&msg);
        buf.clear();
        msg.serialize(&mut buf);

        // Try to batch more messages
        while let Ok(msg) = cmd_rx.try_recv() {
            on_send(&msg);
            msg.serialize(&mut buf);
            if buf.len() >= BUFFER_CAPACITY {
                break;
            }
        }
        stream.write_all(&buf).await?;
        stream.flush().await?;
    }
    Ok(())
}

pub(crate) async fn handle_inbound_traffic<T, R, F>(
    mut reader: BufReader<ReadHalf<T>>,
    tx: &UnboundedSender<R>,
    mut convert_and_send: F,
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

        // Read remaining bytes based on message type
        let total_len = match descriptor {
            DescriptorByte::Job => {
                // Read: job_id (4) + worker_id (2) + payload_len (4)
                buf.resize(11, 0);
                reader.read_exact(&mut buf[1..11]).await?;
                let payload_len = u32::from_le_bytes(buf[7..11].try_into().unwrap()) as usize;

                // Read payload
                buf.resize(11 + payload_len, 0);
                reader.read_exact(&mut buf[11..11 + payload_len]).await?;
                11 + payload_len
            }
            DescriptorByte::QueryJobState => {
                // Read: worker_id (2)
                buf.resize(3, 0);
                reader.read_exact(&mut buf[1..3]).await?;
                3
            }
            DescriptorByte::JobStateResponse => {
                // Fixed length: worker_id (2) + bitfield (1) + last_received (4) + last_responded (4)
                buf.resize(12, 0);
                reader.read_exact(&mut buf[1..12]).await?;
                12
            }
            DescriptorByte::Cancel => {
                // Read: job_id (4) + worker_id (2)
                buf.resize(7, 0);
                reader.read_exact(&mut buf[1..7]).await?;
                7
            }
        };

        // Deserialize the NetworkValue (zero-copy via split/freeze)
        let bytes = buf.split_to(total_len).freeze();
        match NetworkValue::deserialize(bytes) {
            Ok(network_value) => {
                convert_and_send(network_value, tx)?;
            }
            Err(e) => {
                return Err(io::Error::other(format!("Failed to deserialize: {}", e)));
            }
        }
    }
}
