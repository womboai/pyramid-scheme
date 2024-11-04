use std::cell::Cell;
use std::future::Future;
use std::mem::MaybeUninit;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Waker};

pub struct EventFuture<T>(Arc<Mutex<Data<T>>>);

struct Data<T> {
    set: AtomicBool,
    data: MaybeUninit<T>,
    waker: Option<Waker>,
}

unsafe impl<T> Send for EventFuture<T> {}

impl<T> Default for EventFuture<T> {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(Data {
            set: AtomicBool::default(),
            data: MaybeUninit::uninit(),
            waker: None,
        })))
    }
}

impl<T> EventFuture<T> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn complete(&mut self, value: T) {
        let data = &mut self.0.lock().unwrap();
        data.data.write(value);
        data.set.store(true, Ordering::Release);

        if let Some(waker) = data.waker.take() {
            waker.wake();
        }
    }
}

impl<T> Future for EventFuture<T>
where
    T: Copy,
{
    type Output = T;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let data = &mut self.0.lock().unwrap();

        let complete = data.set.load(Ordering::Acquire);
        if complete {
            let data = unsafe {
                data.data.assume_init()
            };

            Poll::Ready(data)
        } else {
            data.waker = Some(context.waker().clone());

            Poll::Pending
        }
    }
}
