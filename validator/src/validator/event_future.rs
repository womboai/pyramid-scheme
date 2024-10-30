use std::future::Future;
use std::mem::MaybeUninit;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};

pub struct EventFuture<T> {
    set: AtomicBool,
    data: MaybeUninit<T>,
}

impl<T> Default for EventFuture<T> {
    fn default() -> Self {
        Self {
            set: AtomicBool::default(),
            data: MaybeUninit::uninit(),
        }
    }
}

unsafe impl<T> Sync for EventFuture<T> {}

impl<T> EventFuture<T> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn complete(&mut self, value: T) {
        self.set.store(true, Ordering::Release);

        self.data.write(value);
    }
}

impl<T> Future for EventFuture<T> where T : Copy {
    type Output = T;

    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        let complete = self.set.load(Ordering::Acquire);
        if complete {
            let data = unsafe {
                self.data.assume_init()
            };

            Poll::Ready(data)
        } else {
            Poll::Pending
        }
    }
}
