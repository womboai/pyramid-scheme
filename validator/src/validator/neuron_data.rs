use rusttensor::rpc::types::NeuronInfoLite;
use serde::{Deserialize, Serialize};
use std::cell::SyncUnsafeCell;
use std::net::TcpStream;
use std::num::NonZeroU8;
use std::ops::{Add, AddAssign};

#[derive(Debug, Copy, Clone, Serialize, Deserialize)]
pub enum Score {
    Legitimate(u128),
    Cheater,
}

impl Default for Score {
    fn default() -> Self {
        Score::Legitimate(0)
    }
}

impl From<Score> for u128 {
    fn from(value: Score) -> Self {
        match value {
            Score::Legitimate(value) => value,
            Score::Cheater => 0,
        }
    }
}

impl Add<u128> for Score {
    type Output = Score;

    fn add(self, rhs: u128) -> Self::Output {
        match self {
            Score::Legitimate(value) => Score::Legitimate(value + rhs),
            Score::Cheater => Score::Cheater,
        }
    }
}

impl AddAssign<u128> for Score {
    fn add_assign(&mut self, rhs: u128) {
        *self = self.add(rhs);
    }
}

pub struct NeuronData {
    pub info: NeuronInfoLite,

    pub weight: NonZeroU8,
    pub score: SyncUnsafeCell<Score>,

    pub connection: SyncUnsafeCell<Option<TcpStream>>,
}
