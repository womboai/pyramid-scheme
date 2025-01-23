use rusttensor::rpc::types::NeuronInfoLite;
use serde::{Deserialize, Serialize};
use std::cell::SyncUnsafeCell;
use std::cmp::Ordering;
use std::net::TcpStream;
use std::ops::AddAssign;

const ADJUSTMENT_ALPHA: f64 = 0.1;

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq)]
pub enum Rate {
    Legitimate(f64),
    Newcomer,
    Cheater,
}

impl Default for Rate {
    fn default() -> Self {
        Rate::Newcomer
    }
}

impl PartialOrd for Rate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        match (*self, other) {
            (Rate::Legitimate(a), Rate::Legitimate(b)) => a.partial_cmp(&b),
            (Rate::Legitimate(_), _) => Some(Ordering::Greater),
            (Rate::Newcomer, Rate::Newcomer) => Some(Ordering::Equal),
            (Rate::Newcomer, Rate::Cheater) => Some(Ordering::Greater),
            (Rate::Cheater, Rate::Cheater) => Some(Ordering::Equal),
            (a, b) => b.partial_cmp(&a).map(|o| o.reverse()),
        }
    }
}

impl AddAssign<f64> for Rate {
    fn add_assign(&mut self, rhs: f64) {
        match self {
            Rate::Legitimate(existing) => {
                *existing = (*existing * (1.0 - ADJUSTMENT_ALPHA)) + (rhs * ADJUSTMENT_ALPHA)
            }
            Rate::Newcomer => {
                *self = Rate::Legitimate(rhs);
            }
            Rate::Cheater => {}
        }
    }
}

pub struct NeuronData {
    pub info: NeuronInfoLite,

    pub rate: Rate,

    pub connection: SyncUnsafeCell<Option<TcpStream>>,
}
