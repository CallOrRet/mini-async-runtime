//! Thread-safe task types for the multi-threaded runtime.
//!
//! Each task wraps its future in an [`UnsafeCell`] instead of a `Mutex`.
//! Exclusive access is guaranteed by an atomic state machine
//! (`IDLE` → `SCHEDULED` → `RUNNING` → …) rather than a lock, removing
//! mutex overhead from every poll.

use std::cell::UnsafeCell;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::AtomicU8;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

/// Type-erased, `Send`-able future for multi-threaded execution.
pub(crate) type BoxFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

// ---- Task states ----

/// Not running, not in the ready queue.
pub(crate) const IDLE: u8 = 0;
/// Sitting in the ready queue, waiting for a worker.
pub(crate) const SCHEDULED: u8 = 1;
/// Currently being polled by a worker.
pub(crate) const RUNNING: u8 = 2;
/// Being polled, AND a waker fired during the poll (needs re-schedule).
pub(crate) const NOTIFIED: u8 = 3;

// ---- Task ----

/// Shared state between a spawned task and its [`JoinHandle`].
pub(crate) struct TaskState<T> {
    pub result: Option<T>,
    pub waker: Option<Waker>,
}

/// A schedulable unit of work in the multi-threaded runtime.
///
/// The future is behind an [`UnsafeCell`]; exclusive access is enforced by
/// the atomic `state` field rather than a mutex.
pub(crate) struct Task {
    pub id: usize,
    future: UnsafeCell<BoxFuture>,
    /// Atomic state shared with wakers via `Arc<AtomicU8>`.
    pub state: Arc<AtomicU8>,
    /// Pre-built waker that re-schedules this task when woken.
    /// Created once at spawn time and reused across polls.
    waker: Waker,
}

// SAFETY: The `UnsafeCell<BoxFuture>` is only dereferenced when the task
// is in the `RUNNING` state, which is held by exactly one thread (enforced
// by the CAS in the worker loop).  The `BoxFuture` itself is `Send`.
unsafe impl Send for Task {}
unsafe impl Sync for Task {}

impl Task {
    pub fn new(id: usize, future: BoxFuture, state: Arc<AtomicU8>, waker: Waker) -> Self {
        Task {
            id,
            future: UnsafeCell::new(future),
            state,
            waker,
        }
    }

    /// Return a reference to the pre-built waker.
    pub fn waker(&self) -> &Waker {
        &self.waker
    }

    /// Poll the future.
    ///
    /// # Safety
    ///
    /// The caller **must** have transitioned `state` to [`RUNNING`] via a
    /// successful CAS.  No other thread may access the future concurrently.
    pub unsafe fn poll(&self, cx: &mut Context<'_>) -> Poll<()> {
        unsafe {
            let fut = &mut *self.future.get();
            fut.as_mut().poll(cx)
        }
    }
}

// ---- Helpers ----

/// Wrap a typed future into a [`BoxFuture`] that stores its result in
/// shared state and wakes the join handle.
pub(crate) fn wrap_future_with_state<F, T>(future: F, state: Arc<Mutex<TaskState<T>>>) -> BoxFuture
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    Box::pin(async move {
        let result = future.await;
        let mut st = state.lock().unwrap();
        st.result = Some(result);
        if let Some(waker) = st.waker.take() {
            waker.wake();
        }
    })
}

// ---- JoinHandle ----

/// Handle to await the result of a task spawned on the multi-threaded runtime.
///
/// Analogous to [`crate::JoinHandle`] but thread-safe.
pub struct JoinHandle<T> {
    pub(crate) state: Arc<Mutex<TaskState<T>>>,
}

impl<T> Future for JoinHandle<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        let mut state = self.state.lock().unwrap();
        let result = state.result.take();
        match result {
            Some(val) => Poll::Ready(val),
            _ => {
                state.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}
