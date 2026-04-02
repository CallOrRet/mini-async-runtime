//! JoinHandle for awaiting the result of a spawned task.
//!
//! When a future is spawned onto the runtime, the caller receives a
//! `JoinHandle<T>` that can be `.await`ed to obtain the task's return value.

use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use crate::task::TaskState;

/// A handle to a spawned task that can be awaited for its result.
///
/// # Examples
///
/// ```ignore
/// let handle = runtime.spawn(async { 42 });
/// let result = handle.await;
/// assert_eq!(result, 42);
/// ```
pub struct JoinHandle<T> {
    /// Shared state with the task; the result appears here once the task completes.
    pub(crate) state: Rc<RefCell<TaskState<T>>>,
}

impl<T> Future for JoinHandle<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = self.state.borrow_mut();
        if let Some(result) = state.result.take() {
            Poll::Ready(result)
        } else {
            // Register the waker so we get notified when the task finishes.
            state.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}
