//! Raw-bytes gRPC [`Codec`] for the MPC data plane.
//!
//! The generated tonic client/server default to [`tonic::codec::ProstCodec`],
//! which re-encodes every message through protobuf. This codec is wired in via
//! `tonic-build`'s `codec_path` (see `build.rs`) so the generated
//! `PartyNodeClient`/`PartyNodeServer` use it instead.
//!
//! It goes further than skipping protobuf: [`SendRequest`] carries the
//! [`NetworkValue`] *unserialized*, so the encoder serializes each value **once,
//! directly into tonic's frame buffer** (no intermediate `Vec` and no second
//! copy), and the decoder deserializes **straight out of** the frame buffer. This
//! matches the single-pass profile of the MPC multiplexer.
//!
//! ## Wire format for [`SendRequests`]
//!
//! A message is the concatenation of one record per [`SendRequest`]:
//!
//! ```text
//! ┌───────────────┬───────────────┬───────────────────────────┐
//! │ session_id    │ value_len     │ value                     │
//! │ u32 (4 bytes) │ u32 (4 bytes) │ NetworkValue, value_len B │
//! └───────────────┴───────────────┴───────────────────────────┘
//! ```
//!
//! gRPC's own 5-byte length prefix delimits the whole message, so the decoder
//! reads records until the frame is exhausted. `value_len` bounds the slice handed
//! to [`NetworkValue::deserialize`]. Integers are big-endian (network order); the
//! choice is arbitrary as long as encode and decode agree.
//!
//! [`SendResponse`] carries no fields, so it encodes to zero bytes.

use std::marker::PhantomData;

use bytes::{Buf, BufMut};
use tonic::{
    codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder},
    Status,
};

use crate::network::mpc::NetworkValue;

use super::messages::{SendRequest, SendRequests, SendResponse};

/// A message that can be encoded to / decoded from the raw gRPC frame body,
/// bypassing protobuf.
pub(crate) trait RawMessage: Sized {
    /// Append this message's raw bytes to the gRPC frame buffer.
    fn raw_encode(&self, dst: &mut EncodeBuf<'_>);

    /// Decode a message from a buffer containing exactly one gRPC frame body.
    fn raw_decode(src: &mut DecodeBuf<'_>) -> Result<Self, Status>;
}

impl RawMessage for SendRequests {
    fn raw_encode(&self, dst: &mut EncodeBuf<'_>) {
        for req in &self.requests {
            let value_len = req.value.byte_len();
            // Reserve the whole record so `serialize` writes into one contiguous
            // region rather than growing the buffer in small increments.
            dst.reserve(8 + value_len);
            dst.put_u32(req.session_id);
            dst.put_u32(value_len as u32);
            // Serialize the NetworkValue straight into tonic's frame buffer.
            req.value.serialize(dst);
        }
    }

    fn raw_decode(src: &mut DecodeBuf<'_>) -> Result<Self, Status> {
        let mut requests = Vec::new();
        while src.remaining() > 0 {
            // Each record starts with an 8-byte header (session_id + value_len).
            if src.remaining() < 8 {
                return Err(Status::internal("raw codec: truncated SendRequest header"));
            }
            let session_id = src.get_u32();
            let value_len = src.get_u32() as usize;
            if src.remaining() < value_len {
                return Err(Status::internal("raw codec: truncated SendRequest payload"));
            }
            // A single gRPC frame is buffered contiguously, so `chunk()` exposes the
            // whole remaining message — deserialize directly from it, no staging copy.
            let value = {
                let chunk = src.chunk();
                if chunk.len() < value_len {
                    return Err(Status::internal("raw codec: non-contiguous frame buffer"));
                }
                NetworkValue::deserialize(&chunk[..value_len])
                    .map_err(|e| Status::internal(format!("raw codec: {e}")))?
            };
            src.advance(value_len);
            requests.push(SendRequest { session_id, value });
        }
        Ok(SendRequests { requests })
    }
}

impl RawMessage for SendResponse {
    fn raw_encode(&self, _dst: &mut EncodeBuf<'_>) {
        // No fields — nothing to write.
    }

    fn raw_decode(_src: &mut DecodeBuf<'_>) -> Result<Self, Status> {
        Ok(SendResponse {})
    }
}

/// A [`Codec`] that serializes messages as raw bytes, skipping protobuf.
///
/// Generic over the encode (`T`) and decode (`U`) message types so a single type
/// covers both directions of the RPC (the client encodes [`SendRequests`] and
/// decodes [`SendResponse`]; the server does the reverse). `tonic-build` emits
/// `RawCodec::default()` and infers `T`/`U` at each call site, exactly as it does
/// for `ProstCodec`.
pub(crate) struct RawCodec<T, U> {
    _pd: PhantomData<(T, U)>,
}

impl<T, U> Default for RawCodec<T, U> {
    fn default() -> Self {
        Self { _pd: PhantomData }
    }
}

impl<T, U> Codec for RawCodec<T, U>
where
    T: RawMessage + Send + 'static,
    U: RawMessage + Send + 'static,
{
    type Encode = T;
    type Decode = U;

    type Encoder = RawEncoder<T>;
    type Decoder = RawDecoder<U>;

    fn encoder(&mut self) -> Self::Encoder {
        RawEncoder(PhantomData)
    }

    fn decoder(&mut self) -> Self::Decoder {
        RawDecoder(PhantomData)
    }
}

/// [`Encoder`] half of [`RawCodec`].
pub(crate) struct RawEncoder<T>(PhantomData<T>);

impl<T: RawMessage> Encoder for RawEncoder<T> {
    type Item = T;
    type Error = Status;

    fn encode(&mut self, item: Self::Item, dst: &mut EncodeBuf<'_>) -> Result<(), Self::Error> {
        item.raw_encode(dst);
        Ok(())
    }
}

/// [`Decoder`] half of [`RawCodec`].
pub(crate) struct RawDecoder<U>(PhantomData<U>);

impl<U: RawMessage> Decoder for RawDecoder<U> {
    type Item = U;
    type Error = Status;

    fn decode(&mut self, src: &mut DecodeBuf<'_>) -> Result<Option<Self::Item>, Self::Error> {
        // tonic guarantees `src` holds exactly one full message frame.
        Ok(Some(U::raw_decode(src)?))
    }
}
