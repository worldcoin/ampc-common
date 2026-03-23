use bytes::{Bytes, BytesMut};
use std::sync::Arc;

#[allow(clippy::len_without_is_empty)]
pub trait ToBytes: std::fmt::Debug + Send + Sync + 'static {
    fn to_bytes(&self, buf: &mut BytesMut);
    fn len(&self) -> usize;
}

/// the application layer may want to send very large messages and
/// it would be advantageous to avoid having to either copy them twice
/// or call socket.write() twice (to ensure the header is sent first).
/// in this case, use Payload::Dyn
///
/// for small or infrequent messages, it isn't worth the trouble and
/// Payload::Bytes can be used
#[derive(Clone, Debug)]
pub enum Payload {
    Bytes(Bytes),
    Dyn(Arc<dyn ToBytes>),
}

impl Default for Payload {
    fn default() -> Self {
        Payload::Bytes(Bytes::new())
    }
}

impl Payload {
    pub fn len(&self) -> usize {
        match self {
            Payload::Bytes(b) => b.len(),
            Payload::Dyn(p) => p.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Get the payload as Bytes.
    /// Payload::Dyn is only sent, not received. when received, it is transformed into Bytes.
    /// the match arm  for Dyn was only left in to avoid an unnecessary panic
    pub fn to_bytes(self) -> Bytes {
        match self {
            Payload::Bytes(b) => b,
            Payload::Dyn(a) => {
                let mut bytes = BytesMut::new();
                bytes.reserve(a.len());
                a.to_bytes(&mut bytes);
                bytes.freeze()
            }
        }
    }
}

impl From<Bytes> for Payload {
    fn from(b: Bytes) -> Self {
        Payload::Bytes(b)
    }
}

impl From<Vec<u8>> for Payload {
    fn from(v: Vec<u8>) -> Self {
        Payload::Bytes(Bytes::from(v))
    }
}

impl<T: ToBytes> From<Arc<T>> for Payload {
    fn from(p: Arc<T>) -> Self {
        Payload::Dyn(p)
    }
}
