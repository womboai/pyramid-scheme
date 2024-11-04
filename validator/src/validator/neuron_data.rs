use std::cell::{Cell, UnsafeCell};
use std::marker::PhantomData;
use std::net::TcpStream;
use std::ops::{Add, AddAssign, Deref};

use serde::{Deserialize, Serialize};

use neuron::NeuronInfoLite;

#[derive(Debug, Copy, Clone, Serialize, Deserialize)]
pub enum Score {
    Legitimate(u128),
    Cheater,
}

unsafe impl Sync for Score {}
unsafe impl Send for Score {}

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

pub struct UnsafeSendRef<'a, T>(&'a T, PhantomData<T>) where T : ?Sized;

impl<'a, T> Clone for UnsafeSendRef<'a, T> where T : ?Sized {
    fn clone(&self) -> Self {
        Self(self.0, PhantomData::default())
    }
}

unsafe impl<'a, T> Send for UnsafeSendRef<'a, T> where T : ?Sized {}

impl<'a, T> From<&'a T> for UnsafeSendRef<'a, T> where T : ?Sized {
    fn from(value: &'a T) -> Self {
        Self(value, PhantomData::default())
    }
}

impl<'a, T> Deref for UnsafeSendRef<'a, T> where T : ?Sized {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.0
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
    pub score: Cell<Score>,
    pub connection: UnsafeCell<Option<TcpStream>>,
}

pub trait UnsafeCellImmutableBorrow<T> {
    fn as_ref(&self) -> &T;
}

impl<T> UnsafeCellImmutableBorrow<T> for UnsafeCell<T> {
    fn as_ref(&self) -> &T {
        unsafe { &*self.get() }
    }
}
