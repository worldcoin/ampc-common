use super::{JobId, Payload, WorkerId};
use bytes::{Bytes, BytesMut};
use eyre::{bail, Result};
use num_enum::{IntoPrimitive, TryFromPrimitive};

/// Network message envelope for worker pool communication
#[derive(Debug)]
pub enum NetworkValue {
    /// Job message between core and worker
    Job {
        job_id: JobId,
        worker_id: WorkerId,
        payload: Payload,
    },
    /// Query worker for last job state (reconciliation after reconnect)
    QueryJobState { worker_id: WorkerId },
    /// Worker's response to job state query
    JobStateResponse {
        worker_id: WorkerId,
        last_received_job_id: Option<JobId>,
        last_responded_job_id: Option<JobId>,
    },
    /// Currently unused. Potentially useful in the future.
    Cancel { job_id: JobId, worker_id: WorkerId },
}

#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, IntoPrimitive, TryFromPrimitive, Debug)]
pub enum DescriptorByte {
    Job = 0x01,
    QueryJobState = 0x03,
    JobStateResponse = 0x04,
    Cancel = 0x05,
}

impl NetworkValue {
    pub fn new_job(job_id: JobId, worker_id: WorkerId, payload: Payload) -> Self {
        Self::Job {
            job_id,
            worker_id,
            payload,
        }
    }

    fn get_descriptor_byte(&self) -> DescriptorByte {
        match self {
            NetworkValue::Job { .. } => DescriptorByte::Job,
            NetworkValue::QueryJobState { .. } => DescriptorByte::QueryJobState,
            NetworkValue::JobStateResponse { .. } => DescriptorByte::JobStateResponse,
            NetworkValue::Cancel { .. } => DescriptorByte::Cancel,
        }
    }

    /// Calculate total byte length when serialized
    pub fn byte_len(&self) -> usize {
        match self {
            NetworkValue::Job { payload, .. } => {
                // descriptor (1) + job_id (4) + worker_id (2) + payload_len (4) + payload
                11 + payload.len()
            }
            NetworkValue::QueryJobState { .. } => {
                // descriptor (1) + worker_id (2)
                3
            }
            NetworkValue::JobStateResponse { .. } => {
                // descriptor (1) + worker_id (2) + bitfield (1) + last_received (4) + last_responded (4)
                12
            }
            NetworkValue::Cancel { .. } => {
                // descriptor (1) + job_id (4) + worker_id (2)
                7
            }
        }
    }

    /// Serialize to bytes
    /// Format:
    /// - Job: descriptor (1) + job_id (4) + worker_id (2) + payload_len (4) + payload
    /// - QueryJobState: descriptor (1) + worker_id (2)
    /// - JobStateResponse: descriptor (1) + worker_id (2) + bitfield (1) + optional values (0-8 bytes)
    /// - Cancel: descriptor (1) + job_id (4) + worker_id (2)
    pub fn serialize(self, buf: &mut BytesMut) {
        buf.extend_from_slice(&[self.get_descriptor_byte() as u8]);

        match self {
            NetworkValue::Job {
                job_id,
                worker_id,
                payload,
            } => {
                buf.extend_from_slice(&job_id.to_le_bytes());
                buf.extend_from_slice(&worker_id.to_le_bytes());
                buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
                match payload {
                    Payload::Bytes(b) => buf.extend_from_slice(&b),
                    Payload::Dyn(p) => p.to_bytes(buf),
                }
            }
            NetworkValue::QueryJobState { worker_id } => {
                buf.extend_from_slice(&worker_id.to_le_bytes());
            }
            NetworkValue::JobStateResponse {
                worker_id,
                last_received_job_id,
                last_responded_job_id,
            } => {
                buf.extend_from_slice(&worker_id.to_le_bytes());
                let bitfield = last_received_job_id.map_or(0u8, |_| 0x01)
                    | last_responded_job_id.map_or(0u8, |_| 0x02);
                buf.extend_from_slice(&[bitfield]);
                buf.extend_from_slice(&last_received_job_id.unwrap_or(0).to_le_bytes());
                buf.extend_from_slice(&last_responded_job_id.unwrap_or(0).to_le_bytes());
            }
            NetworkValue::Cancel { job_id, worker_id } => {
                buf.extend_from_slice(&job_id.to_le_bytes());
                buf.extend_from_slice(&worker_id.to_le_bytes());
            }
        }
    }

    /// Deserialize from bytes
    pub fn deserialize(bytes: Bytes) -> Result<Self> {
        if bytes.is_empty() {
            bail!("Empty buffer");
        }

        let descriptor: DescriptorByte = bytes[0]
            .try_into()
            .map_err(|_| eyre::eyre!("Invalid descriptor byte: {}", bytes[0]))?;

        match descriptor {
            DescriptorByte::Job => {
                if bytes.len() < 11 {
                    bail!("Buffer too short for Job: {} bytes", bytes.len());
                }

                let job_id = u32::from_le_bytes(bytes[1..5].try_into()?);
                let worker_id = u16::from_le_bytes(bytes[5..7].try_into()?);
                let payload_len = u32::from_le_bytes(bytes[7..11].try_into()?) as usize;

                if bytes.len() < 11 + payload_len {
                    bail!(
                        "Incomplete payload: expected {} bytes, got {}",
                        11 + payload_len,
                        bytes.len()
                    );
                }

                let payload = Payload::Bytes(bytes.slice(11..11 + payload_len));

                Ok(NetworkValue::Job {
                    job_id,
                    worker_id,
                    payload,
                })
            }
            DescriptorByte::QueryJobState => {
                if bytes.len() < 3 {
                    bail!("Buffer too short for QueryJobState: {} bytes", bytes.len());
                }
                let worker_id = u16::from_le_bytes(bytes[1..3].try_into()?);
                Ok(NetworkValue::QueryJobState { worker_id })
            }
            DescriptorByte::JobStateResponse => {
                if bytes.len() < 12 {
                    bail!(
                        "Buffer too short for JobStateResponse: {} bytes",
                        bytes.len()
                    );
                }
                let worker_id = u16::from_le_bytes(bytes[1..3].try_into()?);
                let bitfield = bytes[3];
                let last_received_raw = u32::from_le_bytes(bytes[4..8].try_into()?);
                let last_responded_raw = u32::from_le_bytes(bytes[8..12].try_into()?);

                let last_received_job_id = if bitfield & 0x01 != 0 {
                    Some(last_received_raw)
                } else {
                    None
                };
                let last_responded_job_id = if bitfield & 0x02 != 0 {
                    Some(last_responded_raw)
                } else {
                    None
                };

                Ok(NetworkValue::JobStateResponse {
                    worker_id,
                    last_received_job_id,
                    last_responded_job_id,
                })
            }
            DescriptorByte::Cancel => {
                if bytes.len() < 7 {
                    bail!("Buffer too short for Cancel: {} bytes", bytes.len());
                }
                let job_id = u32::from_le_bytes(bytes[1..5].try_into()?);
                let worker_id = u16::from_le_bytes(bytes[5..7].try_into()?);
                Ok(NetworkValue::Cancel { job_id, worker_id })
            }
        }
    }

    /// Convert to network bytes
    pub fn to_network(self) -> Vec<u8> {
        let mut buf = BytesMut::with_capacity(self.byte_len());
        self.serialize(&mut buf);
        buf.freeze().into()
    }
}
