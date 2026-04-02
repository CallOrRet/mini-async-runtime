//! The multi-threaded async runtime.
//!
//! [`MultiThreadRuntime`] provides the same capabilities as the single-threaded
//! [`Runtime`](crate::Runtime) — task spawning, timers, TCP — but distributes
//! work across a pool of OS threads.
//!
//! ## Architecture
//!
//! - **N worker threads** pull tasks from a shared ready queue and poll them.
//! - When idle, one worker **becomes the I/O driver** (epoll + timers) while
//!   the others sleep on a condvar — no dedicated reactor thread is needed.
//! - **`block_on`** parks the calling thread until the root future completes.

use std::future::Future;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};
use std::{io, thread};

use crate::reactor::Reactor;

use super::executor::{self, next_task_id, SharedState};
use super::net::{SharedReactor, TcpListener, TcpStream};
use super::task::{self, Task, TaskState};
use super::timer::{Sleep, TimerWheel};

/// Multi-threaded async runtime.
///
/// # Examples
///
/// ```
/// use mini_async_runtime::multi_thread::MultiThreadRuntime;
///
/// let rt = MultiThreadRuntime::new(4).unwrap();
/// let result = rt.block_on(async { 2 + 2 });
/// assert_eq!(result, 4);
/// ```
pub struct MultiThreadRuntime {
    shared: Arc<SharedState>,
    workers: Mutex<Vec<thread::JoinHandle<()>>>,
}

impl MultiThreadRuntime {
    /// Create a new multi-threaded runtime with `num_threads` worker threads.
    ///
    /// Each worker can both poll tasks and act as the I/O driver when idle.
    pub fn new(num_threads: usize) -> io::Result<Self> {
        assert!(num_threads > 0, "need at least one worker thread");

        let reactor: SharedReactor = Arc::new(Reactor::new()?);
        let timers = Arc::new(TimerWheel::new());
        let shared = Arc::new(SharedState::new(reactor, timers));

        // Spawn worker threads.
        let mut workers = Vec::with_capacity(num_threads);
        for i in 0..num_threads {
            let shared = shared.clone();
            let handle = thread::Builder::new()
                .name(format!("rt-worker-{i}"))
                .spawn(move || executor::run_worker(shared))
                .unwrap();
            workers.push(handle);
        }

        Ok(MultiThreadRuntime {
            shared,
            workers: Mutex::new(workers),
        })
    }

    /// Spawn a future onto the runtime, returning a handle to await its result.
    ///
    /// The future must be `Send` because it may be polled on any worker thread.
    pub fn spawn<F, T>(&self, future: F) -> super::task::JoinHandle<T>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let id = next_task_id();
        let state = Arc::new(Mutex::new(TaskState {
            result: None,
            waker: None,
        }));
        let boxed = task::wrap_future_with_state(future, state.clone());
        let task = Arc::new(Task::new(id, boxed));
        self.shared.spawn(task);
        super::task::JoinHandle { state }
    }

    /// Create a [`Sleep`] future that resolves after `duration`.
    pub fn sleep(&self, duration: Duration) -> Sleep {
        let deadline = Instant::now() + duration;
        self.shared.timers.register(deadline)
    }

    /// Bind a TCP listener to the given address.
    pub fn tcp_listen<A: std::net::ToSocketAddrs>(&self, addr: A) -> io::Result<TcpListener> {
        TcpListener::bind_with_reactor(addr, self.shared.reactor.clone())
    }

    /// Connect to a remote TCP address.
    ///
    /// Uses a blocking connect internally (same as the single-threaded runtime).
    pub fn tcp_connect<A: std::net::ToSocketAddrs>(&self, addr: A) -> io::Result<TcpStream> {
        TcpStream::connect_with_reactor(addr, self.shared.reactor.clone())
    }

    /// Drive the given future to completion, blocking the calling thread.
    ///
    /// The future is submitted to the worker pool; the calling thread parks
    /// until the result is ready.
    pub fn block_on<F, T>(&self, future: F) -> T
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        // Condvar pair to park the calling thread.
        let signal = Arc::new((Mutex::new(None::<T>), Condvar::new()));
        let signal2 = signal.clone();

        // Wrap the user future to deposit its result and signal us.
        let id = next_task_id();
        let boxed: task::BoxFuture = Box::pin(async move {
            let result = future.await;
            let (lock, cvar) = &*signal2;
            *lock.lock().unwrap() = Some(result);
            cvar.notify_one();
        });
        let task = Arc::new(Task::new(id, boxed));
        self.shared.spawn(task);

        // Park until the result appears.
        let (lock, cvar) = &*signal;
        let mut result = lock.lock().unwrap();
        while result.is_none() {
            result = cvar.wait(result).unwrap();
        }
        result.take().unwrap()
    }

    /// Shut down the runtime, joining all background threads.
    ///
    /// Called automatically on [`Drop`].
    pub fn shutdown(&self) {
        self.shared.shutdown.store(true, Ordering::SeqCst);
        // Wake workers sleeping on condvar.
        self.shared.condvar.notify_all();
        // Wake the driver if it is blocked in epoll_wait.
        self.shared.reactor.wake();

        let mut workers = self.workers.lock().unwrap();
        for handle in workers.drain(..) {
            let _ = handle.join();
        }
    }
}

impl Drop for MultiThreadRuntime {
    fn drop(&mut self) {
        self.shutdown();
    }
}
