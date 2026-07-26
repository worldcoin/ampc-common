//! Raw-bytes gRPC [`Codec`] for the MPC data plane.
//!
//! The generated tonic client/server default to [`tonic::codec::ProstCodec`],
//! which re-encodes every message through protobuf. But our payloads are already
//! serialized `NetworkValue` bytes (see `session.rs`), so protobuf just adds a
//! second encode/decode pass and varint tag overhead on the hot path.
//!
//! This codec is wired in via `tonic-build`'s `codec_path` (see `build.rs`) so the
//! generated `PartyNodeClient`/`PartyNodeServer` use it in place of `ProstCodec`.
//! It serializes [`SendRequests`] straight into the gRPC frame with a flat
//! length-prefixed layout — no protobuf.
//!
//! ## Wire format for [`SendRequests`]
//!
//! A message is the concatenation of one record per [`SendRequest`]:
//!
//! ```text
//! ┌───────────────┬───────────────┬───────────────┐
//! │ session_id    │ data_len      │ data          │
//! │ u32 (4 bytes) │ u32 (4 bytes) │ data_len bytes│
//! └───────────────┴───────────────┴───────────────┘
//! ```
//!
//! gRPC's own 5-byte length prefix delimits the whole message, so the decoder
//! reads records until the frame is exhausted. Integers are big-endian (network
//! order); the choice is arbitrary as long as encode and decode agree.
//!
//! [`SendResponse`] carries no fields, so it encodes to zero bytes.

use std::marker::PhantomData;

use bytes::{Buf, BufMut};
use tonic::{
    codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder},
    Status,
};

use crate::proto_generated::party_node::{SendRequest, SendRequests, SendResponse};

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
            dst.put_u32(req.session_id);
            dst.put_u32(req.data.len() as u32);
            dst.put_slice(&req.data);
        }
    }

    fn raw_decode(src: &mut DecodeBuf<'_>) -> Result<Self, Status> {
        let mut requests = Vec::new();
        while src.remaining() > 0 {
            // Each record starts with an 8-byte header (session_id + data_len).
            if src.remaining() < 8 {
                return Err(Status::internal("raw codec: truncated SendRequest header"));
            }
            let session_id = src.get_u32();
            let data_len = src.get_u32() as usize;
            if src.remaining() < data_len {
                return Err(Status::internal("raw codec: truncated SendRequest payload"));
            }
            let mut data = vec![0u8; data_len];
            src.copy_to_slice(&mut data);
            requests.push(SendRequest { session_id, data });
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
