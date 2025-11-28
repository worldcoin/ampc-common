use eyre::{eyre, Report};
use serde::{Deserialize, Serialize};
use std::convert::TryFrom;
use std::fmt;
use std::fmt::{Display, Formatter};
use std::hash::{Hash, Hasher};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, Hash)]
#[repr(i16)]
pub enum AnonStatsOperation {
    #[default]
    Uniqueness = 0,
    Reauth = 1,
}

impl From<AnonStatsOperation> for i16 {
    fn from(value: AnonStatsOperation) -> Self {
        value as i16
    }
}

impl TryFrom<i16> for AnonStatsOperation {
    type Error = Report;

    fn try_from(value: i16) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(AnonStatsOperation::Uniqueness),
            1 => Ok(AnonStatsOperation::Reauth),
            other => Err(eyre!("Unknown anon stats operation value {}", other)),
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Serialize, Deserialize, Default, Hash)]
pub enum Eye {
    #[default]
    Left,
    Right,
}

impl Eye {
    pub fn other(&self) -> Self {
        match self {
            Self::Left => Self::Right,
            Self::Right => Self::Left,
        }
    }
}

impl Display for Eye {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Left => write!(f, "left"),
            Self::Right => write!(f, "right"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AnonStatsResultSource {
    #[default]
    Legacy,
    Aggregator,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AnonStatsOrigin {
    pub side: Option<Eye>,
    pub orientation: AnonStatsOrientation,
    pub context: AnonStatsContext,
}

impl From<AnonStatsOrigin> for i16 {
    fn from(origin: AnonStatsOrigin) -> Self {
        let side_val: i16 = match origin.side {
            Some(Eye::Left) => 0,
            Some(Eye::Right) => 1,
            None => 2,
        };
        let orientation_val: i16 = match origin.orientation {
            AnonStatsOrientation::Normal => 0,
            AnonStatsOrientation::Mirror => 1,
        };
        // 2 bits side + 1 bit orientation + 8 bits context
        (side_val << 9) | (orientation_val << 8) | (origin.context as i16)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AnonStatsOrientation {
    Normal,
    Mirror,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum AnonStatsContext {
    GPU = 0,
    HNSW = 1,
    FACE = 2,
}

pub struct AnonStatsMapping<T> {
    stats: Vec<(i64, T)>,
}

impl<T> AnonStatsMapping<T> {
    pub fn new(mut stats: Vec<(i64, T)>) -> Self {
        // ensure it is sorted by id
        stats.sort_by_key(|(id, _)| *id);
        AnonStatsMapping { stats }
    }

    pub fn len(&self) -> usize {
        self.stats.len()
    }
    pub fn is_empty(&self) -> bool {
        self.stats.is_empty()
    }
    pub fn get_ranges(&self) -> (i64, i64) {
        if self.stats.is_empty() {
            (0, 0)
        } else {
            (self.stats.first().unwrap().0, self.stats.last().unwrap().0)
        }
    }
    pub fn get_id_hash(&self) -> u64 {
        let mut hasher = siphasher::sip::SipHasher13::new();
        for (id, _) in &self.stats {
            id.hash(&mut hasher);
        }
        hasher.finish()
    }

    pub fn into_bundles(self) -> Vec<T> {
        self.stats.into_iter().map(|(_, bundle)| bundle).collect()
    }

    pub fn truncate(&mut self, new_size: usize) {
        self.stats.truncate(new_size);
    }
}
