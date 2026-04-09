//! Task abstraction for the async runtime.
//!
//! A [`Task`] wraps a pinned, boxed future along with a shared state that stores
//! the result once the future completes. Tasks are the fundamental unit of work
//! scheduled onto the executor.
//!
//! Since this is a single-threaded runtime, tasks use `Rc<RefCell>` for shared
//! state instead of `Arc<Mutex>`, and futures are not required to be `Send`.

use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

/// The inner future type erased to a trait object.
/// No `Send` bound — this is a single-threaded runtime.
pub(crate) type BoxFuture = Pin<Box<dyn Future<Output = ()> + 'static>>;

/// Shared state between a [`Task`] and its corresponding [`JoinHandle`](crate::join_handle::JoinHandle).
///
/// When the task's future resolves, the result is stored here so the
/// `JoinHandle` can retrieve it.
pub(crate) struct TaskState<T> {
    /// The result produced by the task, set to `Some` upon completion.
    pub result: Option<T>,
    /// Waker registered by the `JoinHandle` so it can be notified on completion.
    pub waker: Option<std::task::Waker>,
}

/// A spawned task that the executor can poll.
///
/// Each task holds a future and an optional shared state for communicating
/// the result back to a `JoinHandle`.
pub(crate) struct Task {
    /// The future to be driven to completion.
    /// `RefCell` provides interior mutability for polling (single-threaded, no Mutex needed).
    pub future: RefCell<BoxFuture>,
    /// Unique identifier for this task (used for scheduling).
    pub id: usize,
    /// Pre-built waker that re-enqueues this task when woken.
    /// Created once at spawn time and reused across polls.
    waker: Waker,
}

impl Task {
    /// Create a new task wrapping a future that produces `()`.
    pub fn new(id: usize, future: BoxFuture, waker: Waker) -> Self {
        Task {
            future: RefCell::new(future),
            id,
            waker,
        }
    }

    /// Return a reference to the pre-built waker.
    pub fn waker(&self) -> &Waker {
        &self.waker
    }

    /// Poll the inner future. Returns `Poll::Ready(())` when done.
    pub fn poll(&self, cx: &mut Context<'_>) -> Poll<()> {
        let mut future = self.future.borrow_mut();
        future.as_mut().poll(cx)
    }
}

/// Helper to wrap a future with shared state so we can extract the result
/// through a `JoinHandle`.
pub(crate) fn wrap_future_with_state<F, T>(future: F, state: Rc<RefCell<TaskState<T>>>) -> BoxFuture
where
    F: Future<Output = T> + 'static,
    T: 'static,
{
    Box::pin(async move {
        let result = future.await;
        let mut state = state.borrow_mut();
        state.result = Some(result);
        // Wake the JoinHandle if someone is waiting on it.
        if let Some(waker) = state.waker.take() {
            waker.wake();
        }
    })
}
