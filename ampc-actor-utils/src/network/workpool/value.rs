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
    /// ask the worker for all jobs that are in progress
    PendingJobsRequest { worker_id: WorkerId },
    /// worker response to PendingJobRequest
    PendingJobsReply {
        worker_id: WorkerId,
        job_ids: Vec<JobId>,
    },
    /// Currently unused. Potentially useful in the future.
    Cancel { job_id: JobId, worker_id: WorkerId },
}

#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, IntoPrimitive, TryFromPrimitive, Debug)]
pub enum DescriptorByte {
    Job = 0x01,
    PendingJobsRequest = 0x03,
    PendingJobsReply = 0x04,
    Cancel = 0x05,
}

impl DescriptorByte {
    /// Returns the fixed header length (including descriptor byte) for this message type.
    /// For Job messages, this excludes the variable-length payload.
    pub const fn header_len(&self) -> usize {
        match self {
            // descriptor (1) + job_id (4) + worker_id (2) + payload_len (4)
            DescriptorByte::Job => 11,
            // descriptor (1) + worker_id (2)
            DescriptorByte::PendingJobsRequest => 3,
            // descriptor (1) + worker_id (2) + job_ids_len (4)
            DescriptorByte::PendingJobsReply => 7,
            // descriptor (1) + job_id (4) + worker_id (2)
            DescriptorByte::Cancel => 7,
        }
    }
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
            NetworkValue::PendingJobsRequest { .. } => DescriptorByte::PendingJobsRequest,
            NetworkValue::PendingJobsReply { .. } => DescriptorByte::PendingJobsReply,
            NetworkValue::Cancel { .. } => DescriptorByte::Cancel,
        }
    }

    /// Calculate total byte length when serialized
    pub fn byte_len(&self) -> usize {
        match self {
            NetworkValue::Job { payload, .. } => DescriptorByte::Job.header_len() + payload.len(),
            NetworkValue::PendingJobsReply { job_ids, .. } => {
                DescriptorByte::PendingJobsReply.header_len() + (job_ids.len() * size_of::<JobId>())
            }
            NetworkValue::PendingJobsRequest { .. } | NetworkValue::Cancel { .. } => {
                self.get_descriptor_byte().header_len()
            }
        }
    }

    /// Serialize to bytes
    /// Format:
    /// - Job: descriptor (1) + job_id (4) + worker_id (2) + payload_len (4) + payload
    /// - PendingJobsRequest: descriptor (1) + worker_id (2)
    /// - PendingJobsReply: descriptor (1) + worker_id (2) + job_ids_len (4) + job_ids (4 each)
    /// - Cancel: descriptor (1) + job_id (4) + worker_id (2)
    pub fn serialize(&self, buf: &mut BytesMut) {
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
                    Payload::Bytes(b) => buf.extend_from_slice(b),
                    Payload::Dyn(p) => p.to_bytes(buf),
                }
            }
            NetworkValue::PendingJobsRequest { worker_id } => {
                buf.extend_from_slice(&worker_id.to_le_bytes());
            }
            NetworkValue::PendingJobsReply { worker_id, job_ids } => {
                buf.extend_from_slice(&worker_id.to_le_bytes());
                buf.extend_from_slice(&((job_ids.len() * size_of::<JobId>()) as u32).to_le_bytes());
                for job_id in job_ids {
                    buf.extend_from_slice(&job_id.to_le_bytes());
                }
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
        let header_len = descriptor.header_len();

        if bytes.len() < header_len {
            bail!(
                "Buffer too short for {:?}: {} bytes",
                descriptor,
                bytes.len()
            );
        }

        match descriptor {
            DescriptorByte::Job => {
                let job_id = u32::from_le_bytes(bytes[1..5].try_into()?);
                let worker_id = u16::from_le_bytes(bytes[5..7].try_into()?);
                let payload_len = u32::from_le_bytes(bytes[7..11].try_into()?) as usize;

                if bytes.len() < header_len + payload_len {
                    bail!(
                        "Incomplete payload: expected {} bytes, got {}",
                        header_len + payload_len,
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
            DescriptorByte::PendingJobsRequest => {
                let worker_id = u16::from_le_bytes(bytes[1..3].try_into()?);
                Ok(NetworkValue::PendingJobsRequest { worker_id })
            }
            DescriptorByte::PendingJobsReply => {
                let worker_id = u16::from_le_bytes(bytes[1..3].try_into()?);
                let payload_len = u32::from_le_bytes(bytes[3..header_len].try_into()?) as usize;

                if bytes.len() < header_len + payload_len {
                    bail!(
                        "Incomplete PendingJobsReply payload: expected {} bytes, got {}",
                        header_len + payload_len,
                        bytes.len()
                    );
                }

                if !payload_len.is_multiple_of(size_of::<JobId>()) {
                    bail!(
                        "Invalid PendingJobsReply payload_len {}: not divisible by JobId size ({})",
                        payload_len,
                        size_of::<JobId>()
                    );
                }

                let num_jobs = payload_len / size_of::<JobId>();
                let mut job_ids = Vec::with_capacity(num_jobs);
                for idx in (header_len..header_len + payload_len).step_by(size_of::<JobId>()) {
                    let job_id =
                        u32::from_le_bytes(bytes[idx..idx + size_of::<JobId>()].try_into()?);
                    job_ids.push(job_id);
                }

                Ok(NetworkValue::PendingJobsReply { worker_id, job_ids })
            }
            DescriptorByte::Cancel => {
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
