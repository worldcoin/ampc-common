use crate::execution::player::Identity;

#[derive(Clone, Copy, PartialOrd, Ord, PartialEq, Eq, Debug, Hash)]
pub struct ConnectionId(pub u32);

impl ConnectionId {
    pub fn new(val: u32) -> Self {
        Self(val)
    }
}

impl From<u32> for ConnectionId {
    fn from(val: u32) -> Self {
        ConnectionId::new(val)
    }
}

pub struct Peer {
    id: Identity,
    url: String,
}

impl Peer {
    pub fn new(id: Identity, url: String) -> Self {
        Peer { id, url }
    }

    pub fn id(&self) -> &Identity {
        &self.id
    }

    pub fn url(&self) -> &str {
        &self.url
    }
}

impl From<(Identity, String)> for Peer {
    fn from((id, url): (Identity, String)) -> Self {
        Peer::new(id, url)
    }
}
