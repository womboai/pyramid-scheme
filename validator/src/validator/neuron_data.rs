use rusttensor::rpc::types::NeuronInfoLite;
use serde::{Deserialize, Serialize};
use std::cell::UnsafeCell;
use std::marker::PhantomData;
use std::mem::transmute;
use std::net::TcpStream;
use std::num::NonZeroU8;
use std::ops::{Add, AddAssign, Deref, DerefMut};
use std::sync::{Mutex, MutexGuard};

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
    Connected(TcpStream),
    Disconnected,
    Unusable,
}

impl ConnectionState {
    pub fn connected(stream: TcpStream) -> Self {
        Self::Connected(stream)
    }
}

pub struct ConnectionGuard {
    // Incorrect lifetimes, but must live within threads
    pub guard: MutexGuard<'static, ConnectionState>,
    phantom: PhantomData<&'static mut TcpStream>,
}

unsafe impl Send for ConnectionGuard {}

impl ConnectionGuard {
    pub fn new<'a>(guard: MutexGuard<'a, ConnectionState>) -> Self {
        let guard = unsafe {
            // UNSAFE: Extend lifetime to allow connection guard to be used between threads
            transmute::<MutexGuard<'a, ConnectionState>, MutexGuard<'static, ConnectionState>>(
                guard,
            )
        };

        Self {
            guard,
            phantom: PhantomData,
        }
    }
}

impl Deref for ConnectionGuard {
    type Target = TcpStream;

    fn deref(&self) -> &Self::Target {
        let ConnectionState::Connected(stream) = &*self.guard else {
            unreachable!();
        };

        stream
    }
}

impl DerefMut for ConnectionGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        let ConnectionState::Connected(stream) = &mut *self.guard else {
            unreachable!();
        };

        stream
    }
}

pub struct NeuronData {
    pub info: NeuronInfoLite,

    pub weight: NonZeroU8,
    pub score: UnsafeCell<Score>,

    pub connection: Mutex<ConnectionState>,
}

unsafe impl Send for NeuronData {}
unsafe impl Sync for NeuronData {}
