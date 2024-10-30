use std::cell::UnsafeCell;
use std::net::TcpStream;
use std::ops::{Add, AddAssign, Deref};

use serde::{Deserialize, Serialize};

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
        *self = *self + rhs
    }
}

pub struct NeuronData {
    pub info: NeuronInfoLite,

    // Allow (non-shared) mutation in computation thread, not RefCell to avoid runtime borrow overhead
    pub score: UnsafeCell<Score>,
    pub connection: UnsafeCell<Option<TcpStream>>,
}

pub trait UnsafeCellImmutableBorrow<T> {
    fn as_ref(&self) -> &T;
}

pub trait UnsafeCellImmutableCopy<T> {
    fn inner(&self) -> T;
}

impl<T> UnsafeCellImmutableBorrow<T> for UnsafeCell<T> {
    fn as_ref(&self) -> &T {
        unsafe { &*self.get() }
    }
}

impl<T> UnsafeCellImmutableCopy<T> for UnsafeCell<T> where T : Copy {
    fn inner(&self) -> T {
        *self.as_ref()
    }
}

pub struct SendPtr<T>(*mut T);

unsafe impl<T> Send for SendPtr<T> {}

impl<T> From<*mut T> for SendPtr<T> {
    fn from(value: *mut T) -> Self {
        SendPtr(value)
    }
}

impl<T> Deref for SendPtr<T> {
    type Target = *mut T;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
