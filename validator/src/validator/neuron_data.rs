use serde::{Deserialize, Serialize};
use std::marker::PhantomData;
use std::net::TcpStream;
use std::ops::{Add, AddAssign, Deref, DerefMut};
use std::sync::{Mutex, MutexGuard, RwLock};

use neuron::NeuronInfoLite;

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

pub enum ConnectionState {
    Connected { stream: TcpStream, free: bool },
    Disconnected,
    Unusable,
}

impl ConnectionState {
    pub fn connected(stream: TcpStream) -> Self {
        Self::Connected { stream, free: true }
    }
}

pub struct ConnectionGuard<'a> {
    guard: MutexGuard<'a, ConnectionState>,
    phantom: PhantomData<&'a mut TcpStream>,
}

impl<'a> ConnectionGuard<'a> {
    pub fn new(mut guard: MutexGuard<'a, ConnectionState>) -> Self {
        if let ConnectionState::Connected { free, .. } = &mut *guard {
            *free = false;
        } else {
            panic!("Tried to initialize connection guard out of a non-connected state")
        }

        Self {
            guard,
            phantom: PhantomData,
        }
    }
}

impl Deref for ConnectionGuard<'_> {
    type Target = TcpStream;

    fn deref(&self) -> &Self::Target {
        let ConnectionState::Connected { stream, .. } = &*self.guard else {
            unreachable!();
        };

        stream
    }
}

impl DerefMut for ConnectionGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        let ConnectionState::Connected { stream, .. } = &mut *self.guard else {
            unreachable!();
        };

        stream
    }
}

pub struct NeuronData {
    pub info: NeuronInfoLite,

    pub score: RwLock<Score>,
    pub connection: Mutex<ConnectionState>,
}

unsafe impl Send for NeuronData {}
unsafe impl Sync for NeuronData {}
