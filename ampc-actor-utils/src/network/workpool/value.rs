use super::{JobId, Payload, WorkerId};
use bytes::BytesMut;
use eyre::{bail, Result};
use num_enum::{IntoPrimitive, TryFromPrimitive};

/// Network message envelope for worker pool communication
#[derive(Debug, Clone, PartialEq)]
pub enum NetworkValue {
    /// Request from core to worker
    Request {
        job_id: JobId,
        worker_id: WorkerId,
        payload: Payload,
    },
    /// Response from worker to core
    Response {
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
    Request = 0x01,
    Response = 0x02,
    QueryJobState = 0x03,
    JobStateResponse = 0x04,
    Cancel = 0x05,
}

impl NetworkValue {
    pub fn new_request(job_id: JobId, worker_id: WorkerId, payload: Payload) -> Self {
        Self::Request {
            job_id,
            worker_id,
            payload,
        }
    }

    pub fn new_response(job_id: JobId, worker_id: WorkerId, payload: Payload) -> Self {
        Self::Response {
            job_id,
            worker_id,
            payload,
        }
    }

    fn get_descriptor_byte(&self) -> DescriptorByte {
        match self {
            NetworkValue::Request { .. } => DescriptorByte::Request,
            NetworkValue::Response { .. } => DescriptorByte::Response,
            NetworkValue::QueryJobState { .. } => DescriptorByte::QueryJobState,
            NetworkValue::JobStateResponse { .. } => DescriptorByte::JobStateResponse,
            NetworkValue::Cancel { .. } => DescriptorByte::Cancel,
        }
    }

    /// Calculate total byte length when serialized
    pub fn byte_len(&self) -> usize {
        match self {
            NetworkValue::Request { payload, .. } | NetworkValue::Response { payload, .. } => {
                // descriptor (1) + job_id (4) + worker_id (2) + payload_len (4) + payload
                11 + payload.len()
            }
            NetworkValue::QueryJobState { .. } => {
                // descriptor (1) + worker_id (2)
                3
            }
            NetworkValue::JobStateResponse { .. } => {
                // descriptor (1) + worker_id (2) + last_received (4) + last_responded (4)
                11
            }
            NetworkValue::Cancel { .. } => {
                // descriptor (1) + job_id (4) + worker_id (2)
                7
            }
        }
    }

    /// Serialize to bytes
    /// Format:
    /// - Request/Response: descriptor (1) + job_id (4) + worker_id (2) + payload_len (4) + payload
    /// - QueryJobState: descriptor (1) + worker_id (2)
    /// - JobStateResponse: descriptor (1) + worker_id (2) + bitfield (1) + optional values (0-8 bytes)
    /// - Cancel: descriptor (1) + job_id (4) + worker_id (2)
    pub fn serialize(&self, buf: &mut BytesMut) {
        buf.extend_from_slice(&[self.get_descriptor_byte() as u8]);

        match self {
            NetworkValue::Request {
                job_id,
                worker_id,
                payload,
            }
            | NetworkValue::Response {
                job_id,
                worker_id,
                payload,
            } => {
                buf.extend_from_slice(&job_id.to_le_bytes());
                buf.extend_from_slice(&worker_id.to_le_bytes());
                buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
                buf.extend_from_slice(payload);
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
                // Fixed length: 0 means "no job"
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
    pub fn deserialize(bytes: &[u8]) -> Result<Self> {
        if bytes.is_empty() {
            bail!("Empty buffer");
        }

        let descriptor: DescriptorByte = bytes[0]
            .try_into()
            .map_err(|_| eyre::eyre!("Invalid descriptor byte: {}", bytes[0]))?;

        match descriptor {
            DescriptorByte::Request | DescriptorByte::Response => {
                if bytes.len() < 11 {
                    bail!(
                        "Buffer too short for Request/Response: {} bytes",
                        bytes.len()
                    );
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

                let payload = bytes[11..11 + payload_len].to_vec();

                match descriptor {
                    DescriptorByte::Request => Ok(NetworkValue::Request {
                        job_id,
                        worker_id,
                        payload,
                    }),
                    DescriptorByte::Response => Ok(NetworkValue::Response {
                        job_id,
                        worker_id,
                        payload,
                    }),
                    _ => unreachable!(),
                }
            }
            DescriptorByte::QueryJobState => {
                if bytes.len() < 3 {
                    bail!("Buffer too short for QueryJobState: {} bytes", bytes.len());
                }
                let worker_id = u16::from_le_bytes(bytes[1..3].try_into()?);
                Ok(NetworkValue::QueryJobState { worker_id })
            }
            DescriptorByte::JobStateResponse => {
                if bytes.len() < 11 {
                    bail!(
                        "Buffer too short for JobStateResponse: {} bytes",
                        bytes.len()
                    );
                }
                let worker_id = u16::from_le_bytes(bytes[1..3].try_into()?);
                // Fixed length: 0 means "no job"
                let last_received_raw = u32::from_le_bytes(bytes[3..7].try_into()?);
                let last_responded_raw = u32::from_le_bytes(bytes[7..11].try_into()?);

                let last_received_job_id = if last_received_raw == 0 {
                    None
                } else {
                    Some(last_received_raw)
                };
                let last_responded_job_id = if last_responded_raw == 0 {
                    None
                } else {
                    Some(last_responded_raw)
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
    pub fn to_network(&self) -> Vec<u8> {
        let mut buf = BytesMut::with_capacity(self.byte_len());
        self.serialize(&mut buf);
        buf.freeze().into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_request_roundtrip() {
        let msg = NetworkValue::Request {
            job_id: 12345,
            worker_id: 42,
            payload: vec![1, 2, 3, 4, 5],
        };

        let serialized = msg.to_network();
        let deserialized = NetworkValue::deserialize(&serialized).unwrap();

        assert_eq!(msg, deserialized);
    }

    #[test]
    fn test_response_roundtrip() {
        let msg = NetworkValue::Response {
            job_id: 67890,
            worker_id: 7,
            payload: vec![10, 20, 30, 40],
        };

        let serialized = msg.to_network();
        let deserialized = NetworkValue::deserialize(&serialized).unwrap();

        assert_eq!(msg, deserialized);
    }

    #[test]
    fn test_empty_payload() {
        let msg = NetworkValue::Request {
            job_id: 999,
            worker_id: 1,
            payload: vec![],
        };

        let serialized = msg.to_network();
        assert_eq!(serialized.len(), 11); // Just header, no payload

        let deserialized = NetworkValue::deserialize(&serialized).unwrap();
        assert_eq!(msg, deserialized);
    }

    #[test]
    fn test_query_job_state_roundtrip() {
        let msg = NetworkValue::QueryJobState { worker_id: 5 };

        let serialized = msg.to_network();
        assert_eq!(serialized.len(), 3);

        let deserialized = NetworkValue::deserialize(&serialized).unwrap();
        assert_eq!(msg, deserialized);
    }

    #[test]
    fn test_job_state_response_roundtrip() {
        let msg = NetworkValue::JobStateResponse {
            worker_id: 7,
            last_received_job_id: Some(100),
            last_responded_job_id: Some(95),
        };

        let serialized = msg.to_network();
        assert_eq!(serialized.len(), 11); // fixed: descriptor + worker_id + 2 u32s

        let deserialized = NetworkValue::deserialize(&serialized).unwrap();
        assert_eq!(msg, deserialized);
    }

    #[test]
    fn test_job_state_response_none_values() {
        let msg = NetworkValue::JobStateResponse {
            worker_id: 3,
            last_received_job_id: None,
            last_responded_job_id: None,
        };

        let serialized = msg.to_network();
        assert_eq!(serialized.len(), 11); // fixed length (zeros for None)

        let deserialized = NetworkValue::deserialize(&serialized).unwrap();
        assert_eq!(msg, deserialized);
    }

    #[test]
    fn test_job_state_response_partial_values() {
        // Test with only last_received
        let msg1 = NetworkValue::JobStateResponse {
            worker_id: 5,
            last_received_job_id: Some(42),
            last_responded_job_id: None,
        };
        let serialized1 = msg1.to_network();
        assert_eq!(serialized1.len(), 11); // fixed length
        let deserialized1 = NetworkValue::deserialize(&serialized1).unwrap();
        assert_eq!(msg1, deserialized1);

        // Test with only last_responded
        let msg2 = NetworkValue::JobStateResponse {
            worker_id: 6,
            last_received_job_id: None,
            last_responded_job_id: Some(99),
        };
        let serialized2 = msg2.to_network();
        assert_eq!(serialized2.len(), 11); // fixed length
        let deserialized2 = NetworkValue::deserialize(&serialized2).unwrap();
        assert_eq!(msg2, deserialized2);
    }

    #[test]
    fn test_cancel_roundtrip() {
        let msg = NetworkValue::Cancel {
            job_id: 12345,
            worker_id: 7,
        };

        let serialized = msg.to_network();
        assert_eq!(serialized.len(), 7); // descriptor + job_id + worker_id

        let deserialized = NetworkValue::deserialize(&serialized).unwrap();
        assert_eq!(msg, deserialized);
    }
}
