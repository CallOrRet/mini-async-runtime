//! An async multi-producer, single-consumer (mpsc) channel.
//!
//! This is a simple unbounded channel for sending values between async tasks.
//!
//! Uses `Rc<RefCell>` internally — no locks needed in a single-threaded runtime.
//!
//! # Examples
//!
//! ```ignore
//! let (tx, mut rx) = mini_async_runtime::channel::unbounded::<i32>();
//! tx.send(42).unwrap();
//! let value = rx.recv().await;
//! assert_eq!(value, Some(42));
//! ```

use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

/// Shared internal state of the channel.
struct Inner<T> {
    /// Buffered messages waiting to be received.
    queue: VecDeque<T>,
    /// Waker for the receiver, set when it is waiting for data.
    rx_waker: Option<Waker>,
    /// Number of live senders. When this drops to 0 the channel is closed.
    sender_count: usize,
    /// Whether the receiver has been dropped.
    rx_closed: bool,
}

/// The sending half of the channel. Can be cloned to create multiple producers.
pub struct Sender<T> {
    inner: Rc<RefCell<Inner<T>>>,
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        self.inner.borrow_mut().sender_count += 1;
        Sender {
            inner: Rc::clone(&self.inner),
        }
    }
}

impl<T> Sender<T> {
    /// Send a value into the channel.
    ///
    /// Returns `Err(value)` if the receiver has been dropped.
    pub fn send(&self, value: T) -> Result<(), T> {
        let mut inner = self.inner.borrow_mut();
        if inner.rx_closed {
            return Err(value);
        }
        inner.queue.push_back(value);
        if let Some(waker) = inner.rx_waker.take() {
            waker.wake();
        }
        Ok(())
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        let mut inner = self.inner.borrow_mut();
        inner.sender_count -= 1;
        if inner.sender_count == 0 {
            // All senders gone — wake the receiver so it sees `None`.
            if let Some(waker) = inner.rx_waker.take() {
                waker.wake();
            }
        }
    }
}

/// The receiving half of the channel.
pub struct Receiver<T> {
    inner: Rc<RefCell<Inner<T>>>,
}

impl<T> Receiver<T> {
    /// Receive the next value from the channel.
    ///
    /// Returns `None` when all senders have been dropped and the buffer is empty.
    pub fn recv(&mut self) -> Recv<'_, T> {
        Recv { receiver: self }
    }

    /// Try to receive a value without blocking.
    pub fn try_recv(&mut self) -> Option<T> {
        self.inner.borrow_mut().queue.pop_front()
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        self.inner.borrow_mut().rx_closed = true;
    }
}

/// Future returned by [`Receiver::recv`].
pub struct Recv<'a, T> {
    receiver: &'a mut Receiver<T>,
}

impl<'a, T> Future for Recv<'a, T> {
    type Output = Option<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut inner = self.receiver.inner.borrow_mut();
        let front = inner.queue.pop_front();
        match front {
            Some(value) => Poll::Ready(Some(value)),
            _ => if inner.sender_count == 0 {
                // Channel closed and buffer drained.
                Poll::Ready(None)
            } else {
                inner.rx_waker = Some(cx.waker().clone());
                Poll::Pending
            },
        }
    }
}

/// Create an unbounded mpsc channel.
pub fn unbounded<T>() -> (Sender<T>, Receiver<T>) {
    let inner = Rc::new(RefCell::new(Inner {
        queue: VecDeque::new(),
        rx_waker: None,
        sender_count: 1,
        rx_closed: false,
    }));
    (
        Sender {
            inner: Rc::clone(&inner),
        },
        Receiver { inner },
    )
}
