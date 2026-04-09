//! The async runtime that combines the executor, reactor, and timer wheel.
//!
//! [`Runtime`] is the main entry point. Create one, then use
//! [`Runtime::block_on`] to drive a top-level future, or [`Runtime::spawn`]
//! to submit background tasks.

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::executor::{self, Executor};
use crate::join_handle::JoinHandle;
use crate::net;
use crate::next_task_id;
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
        let waker = executor::make_waker(id, Rc::clone(&self.executor.ready_queue));
        let task = Rc::new(Task::new(id, boxed, waker));
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
    ) -> impl Future<Output = std::io::Result<net::TcpStream>> + use<A> {
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

        // Use a flag-setting waker so we only re-poll the JoinHandle when
        // the underlying task has actually completed and woken us.
        let handle_woken = Rc::new(Cell::new(true)); // start as true to poll once
        let waker = flag_waker(Rc::clone(&handle_woken));
        let mut cx = std::task::Context::from_waker(&waker);

        loop {
            // 1. Run all ready tasks.
            while self.executor.tick() {}

            // 2. Check if our top-level future is done (only if woken).
            if handle_woken.get() {
                handle_woken.set(false);
                let poll = handle.as_mut().poll(&mut cx);
                if let std::task::Poll::Ready(val) = poll {
                    return val;
                }
            }

            // 3. If no tasks remain (other than possibly the handle), bail out
            //    to avoid spinning forever.
            if self.executor.is_empty() {
                // The root task finished — result must be stored.
                let poll = handle.as_mut().poll(&mut cx);
                if let std::task::Poll::Ready(val) = poll {
                    return val;
                }
            }

            // 4. Process timers and compute how long we can sleep.
            let timer_timeout = {
                let mut timers = self.timers.borrow_mut();
                timers.process()
            };

            // 5. If there are ready tasks after timer processing, skip blocking.
            let ready_empty = self.executor.ready_queue.borrow().is_empty();
            if !ready_empty {
                continue;
            }

            // 6. Block on the reactor for I/O events (with a timeout).
            let has_pending = self.timers.borrow().has_pending();
            let timeout = if has_pending {
                // Don't block longer than the next timer deadline.
                Some(timer_timeout.unwrap_or(Duration::from_millis(1)))
            } else if self.executor.is_empty() {
                // Nothing left to do.
                break;
            } else {
                // No timers, but tasks are alive — use a short timeout so we
                // don't miss wakeups from non-I/O sources (e.g. channels).
                Some(Duration::from_millis(5))
            };

            let _ = self.reactor.poll(timeout);

            // 7. Process timers again after sleeping.
            let mut timers = self.timers.borrow_mut();
            timers.process();
            drop(timers);
        }

        // Final attempt to extract the result.
        let poll = handle.as_mut().poll(&mut cx);
        match poll {
            std::task::Poll::Ready(val) => val,
            std::task::Poll::Pending => {
                panic!("block_on: future did not complete and no tasks remain")
            }
        }
    }
}

/// Build a waker that sets a flag when woken.
///
/// Used by `block_on` to know when the JoinHandle's underlying task has
/// completed, avoiding redundant polls on every loop iteration.
fn flag_waker(flag: Rc<Cell<bool>>) -> std::task::Waker {
    use std::task::{RawWaker, RawWakerVTable};

    const VTABLE: RawWakerVTable = RawWakerVTable::new(
        // clone
        |ptr| {
            let flag = unsafe { Rc::from_raw(ptr as *const Cell<bool>) };
            let cloned = flag.clone();
            std::mem::forget(flag); // don't drop the original
            RawWaker::new(Rc::into_raw(cloned) as *const (), &VTABLE)
        },
        // wake (by value)
        |ptr| {
            let flag = unsafe { Rc::from_raw(ptr as *const Cell<bool>) };
            flag.set(true);
            // flag is dropped here (decrements refcount)
        },
        // wake_by_ref
        |ptr| {
            let flag = unsafe { &*(ptr as *const Cell<bool>) };
            flag.set(true);
        },
        // drop
        |ptr| {
            unsafe { Rc::from_raw(ptr as *const Cell<bool>) };
            // dropped, decrements refcount
        },
    );

    let ptr = Rc::into_raw(flag) as *const ();
    // SAFETY: the vtable correctly manages the Rc<Cell<bool>> lifetime.
    unsafe { std::task::Waker::from_raw(RawWaker::new(ptr, &VTABLE)) }
}
