//! The async runtime that combines the executor, reactor, and timer wheel.
//!
//! [`Runtime`] is the main entry point. Create one, then use
//! [`Runtime::block_on`] to drive a top-level future, or [`Runtime::spawn`]
//! to submit background tasks.

use std::cell::RefCell;
use std::future::Future;
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::executor::{next_task_id, Executor};
use crate::join_handle::JoinHandle;
use crate::net;
use crate::reactor::{self, SharedReactor};
use crate::task::{self, Task, TaskState};
use crate::timer::{self, SharedTimerWheel, Sleep};

/// The mini async runtime.
///
/// # Examples
///
/// ```
/// use mini_async_runtime::Runtime;
///
/// let mut rt = Runtime::new().unwrap();
/// let result = rt.block_on(async {
///     1 + 1
/// });
/// assert_eq!(result, 2);
/// ```
pub struct Runtime {
    executor: Executor,
    pub(crate) reactor: SharedReactor,
    pub(crate) timers: SharedTimerWheel,
}

impl Runtime {
    /// Create a new runtime instance.
    pub fn new() -> std::io::Result<Self> {
        Ok(Runtime {
            executor: Executor::new(),
            reactor: reactor::new_shared_reactor()?,
            timers: timer::new_shared_timer_wheel(),
        })
    }

    /// Spawn a future onto the runtime, returning a [`JoinHandle`] to await its result.
    ///
    /// No `Send` bound is required — this is a single-threaded runtime.
    pub fn spawn<F, T>(&mut self, future: F) -> JoinHandle<T>
    where
        F: Future<Output = T> + 'static,
        T: 'static,
    {
        let id = next_task_id();
        let state = Rc::new(RefCell::new(TaskState {
            result: None,
            waker: None,
        }));
        let boxed = task::wrap_future_with_state(future, Rc::clone(&state));
        let task = Rc::new(Task::new(id, boxed));
        self.executor.spawn(task);
        JoinHandle { state }
    }

    /// Create a [`Sleep`] future that resolves after `duration`.
    pub fn sleep(&self, duration: Duration) -> Sleep {
        let deadline = Instant::now() + duration;
        self.timers.borrow_mut().register(deadline)
    }

    /// Bind a TCP listener to the given address.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let listener = rt.tcp_listen("127.0.0.1:8080")?;
    /// ```
    pub fn tcp_listen<A: std::net::ToSocketAddrs>(
        &self,
        addr: A,
    ) -> std::io::Result<net::TcpListener> {
        net::TcpListener::bind_with_reactor(addr, self.reactor.clone())
    }

    /// Connect to a remote TCP address asynchronously.
    ///
    /// Returns a future that resolves to a [`TcpStream`](net::TcpStream).
    /// The future is `'static` and can be moved into spawned tasks.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let connect = rt.tcp_connect("127.0.0.1:8080");
    /// let stream = rt.block_on(async { connect.await.unwrap() });
    /// ```
    pub fn tcp_connect<A: std::net::ToSocketAddrs + 'static>(
        &self,
        addr: A,
    ) -> impl Future<Output = std::io::Result<net::TcpStream>> {
        let reactor = self.reactor.clone();
        async move { net::TcpStream::connect_with_reactor(addr, reactor).await }
    }

    /// Drive the given future to completion, running all spawned tasks
    /// along the way.
    ///
    /// This is the primary way to enter the async context. It blocks the
    /// current thread until `future` resolves.
    pub fn block_on<F, T>(&mut self, future: F) -> T
    where
        F: Future<Output = T> + 'static,
        T: 'static,
    {
        let handle = self.spawn(future);

        // Pin the handle so we can poll it.
        let mut handle = handle;
        let mut handle = std::pin::Pin::new(&mut handle);

        loop {
            // 1. Run all ready tasks.
            while self.executor.tick() {}

            // 2. Check if our top-level future is done.
            //    We create a no-op waker just to check the JoinHandle state.
            let waker = noop_waker();
            let mut cx = std::task::Context::from_waker(&waker);
            if let std::task::Poll::Ready(val) = handle.as_mut().poll(&mut cx) {
                return val;
            }

            // 3. If no tasks remain (other than possibly the handle), bail out
            //    to avoid spinning forever.
            if self.executor.is_empty() {
                let waker = noop_waker();
                let mut cx = std::task::Context::from_waker(&waker);
                if let std::task::Poll::Ready(val) = handle.as_mut().poll(&mut cx) {
                    return val;
                }
            }

            // 4. Process timers and compute how long we can sleep.
            let timer_timeout = self.timers.borrow_mut().process();

            // 5. If there are ready tasks after timer processing, skip blocking.
            if !self.executor.ready_queue.borrow().is_empty() {
                // ReadyQueue::is_empty() is O(1)
                continue;
            }

            // 6. Block on the reactor for I/O events (with a timeout).
            let timeout = if self.timers.borrow().has_pending() {
                // Don't block longer than the next timer deadline.
                Some(timer_timeout.unwrap_or(Duration::from_millis(1)))
            } else if self.executor.is_empty() {
                // Nothing left to do.
                break;
            } else {
                // No timers, but tasks are alive — use a short timeout so we
                // don't miss wakeups from non-I/O sources (e.g. channels).
                Some(Duration::from_millis(10))
            };

            let _ = self.reactor.poll(timeout);

            // 7. Process timers again after sleeping.
            self.timers.borrow_mut().process();
        }

        // Final attempt to extract the result.
        let waker = noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        match handle.as_mut().poll(&mut cx) {
            std::task::Poll::Ready(val) => val,
            std::task::Poll::Pending => {
                panic!("block_on: future did not complete and no tasks remain")
            }
        }
    }
}

/// Build a no-op waker using the RawWaker API.
///
/// Used only inside `block_on` to probe the JoinHandle without
/// registering any real wake-up interest.
fn noop_waker() -> std::task::Waker {
    use std::task::{RawWaker, RawWakerVTable};

    const NOOP_VTABLE: RawWakerVTable = RawWakerVTable::new(
        |_| RawWaker::new(std::ptr::null(), &NOOP_VTABLE), // clone
        |_| {},                                            // wake
        |_| {},                                            // wake_by_ref
        |_| {},                                            // drop
    );

    // SAFETY: the vtable does nothing, so no invariants can be violated.
    unsafe { std::task::Waker::from_raw(RawWaker::new(std::ptr::null(), &NOOP_VTABLE)) }
}
