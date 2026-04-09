//! Multi-threaded executor with a shared work queue and worker pool.
//!
//! Worker threads pull tasks from a shared ready queue and poll them.
//! When no tasks are available, one worker becomes the **I/O driver**
//! (running epoll + timers) while the others sleep on a [`Condvar`].
//!
//! ## Task state machine
//!
//! Each task carries an atomic state that replaces the `Mutex` around the
//! future, removing lock overhead from every poll:
//!
//! ```text
//!              spawn / wake
//! ┌──────┐   ──────────────▶  ┌───────────┐  worker dequeues  ┌─────────┐
//! │ IDLE │                    │ SCHEDULED │ ────────────────▶ │ RUNNING │
//! └──────┘  ◀────────────────  └───────────┘                  └────┬────┘
//!        poll returns Pending                                      │
//!        & no wake during poll                              wake during poll
//!                                                                  │
//!                                                            ┌─────▼─────┐
//!                                                            │ NOTIFIED  │
//!                                                            └───────────┘
//!                                                   worker re-schedules after poll
//! ```

use std::collections::{HashMap, HashSet, VecDeque};
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};
use std::time::Duration;

use super::net::SharedReactor;
use super::task::{Task, IDLE, NOTIFIED, RUNNING, SCHEDULED};
use super::timer::TimerWheel;

pub(crate) use crate::next_task_id;

// ---------------------------------------------------------------------------
// Ready queue with deduplication
// ---------------------------------------------------------------------------

/// A FIFO queue of task IDs with O(1) deduplication.
pub(crate) struct ReadyQueue {
    queue: VecDeque<usize>,
    pending: HashSet<usize>,
}

impl ReadyQueue {
    pub fn new() -> Self {
        ReadyQueue {
            queue: VecDeque::new(),
            pending: HashSet::new(),
        }
    }

    pub fn enqueue(&mut self, task_id: usize) {
        if self.pending.insert(task_id) {
            self.queue.push_back(task_id);
        }
    }

    pub fn dequeue(&mut self) -> Option<usize> {
        if let Some(id) = self.queue.pop_front() {
            self.pending.remove(&id);
            Some(id)
        } else {
            None
        }
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Shared state across all workers
// ---------------------------------------------------------------------------

/// State shared between the runtime and all worker threads.
pub(crate) struct SharedState {
    /// All live tasks, keyed by ID.
    pub tasks: Mutex<HashMap<usize, Arc<Task>>>,
    /// Queue of task IDs ready to be polled.
    pub ready_queue: Mutex<ReadyQueue>,
    /// Signaled when new tasks become ready or on shutdown.
    pub condvar: Condvar,
    /// Number of currently active (live) tasks.
    pub active_count: AtomicUsize,
    /// Set to `true` to signal all threads to exit.
    pub shutdown: AtomicBool,
    /// I/O driver token — see [`super::executor`] module docs.
    pub driver_token: AtomicBool,
    /// The epoll-based I/O reactor.
    pub reactor: SharedReactor,
    /// Timer wheel.
    pub timers: Arc<TimerWheel>,
}

impl SharedState {
    pub fn new(reactor: SharedReactor, timers: Arc<TimerWheel>) -> Self {
        SharedState {
            tasks: Mutex::new(HashMap::new()),
            ready_queue: Mutex::new(ReadyQueue::new()),
            condvar: Condvar::new(),
            active_count: AtomicUsize::new(0),
            shutdown: AtomicBool::new(false),
            driver_token: AtomicBool::new(false),
            reactor,
            timers,
        }
    }

    /// Enqueue a task ID for polling and wake one worker.
    pub fn enqueue_task(&self, task_id: usize) {
        self.ready_queue.lock().unwrap().enqueue(task_id);
        if self.driver_token.load(Ordering::Relaxed) {
            // A worker is driving I/O — poke the eventfd so `epoll_wait`
            // returns immediately.
            self.reactor.wake();
        }
        // Wake a worker sleeping on the condvar (no-op if none are waiting).
        self.condvar.notify_one();
    }

    /// Insert a new task and enqueue it for its first poll.
    pub fn spawn(&self, task: Arc<Task>) {
        let id = task.id;
        self.tasks.lock().unwrap().insert(id, task);
        self.active_count.fetch_add(1, Ordering::Relaxed);
        self.enqueue_task(id);
    }
}

// ---------------------------------------------------------------------------
// Arc-based waker (carries task state for lock-free wake)
// ---------------------------------------------------------------------------

struct WakerData {
    task_id: usize,
    /// Handle to the task's atomic state — allows the waker to set
    /// `NOTIFIED` without touching the tasks map or any mutex.
    task_state: Arc<AtomicU8>,
    shared: Arc<SharedState>,
}

const VTABLE: RawWakerVTable =
    RawWakerVTable::new(waker_clone, waker_wake, waker_wake_by_ref, waker_drop);

unsafe fn waker_clone(ptr: *const ()) -> RawWaker { unsafe {
    let data = &*(ptr as *const WakerData);
    let cloned = Box::new(WakerData {
        task_id: data.task_id,
        task_state: data.task_state.clone(),
        shared: data.shared.clone(),
    });
    RawWaker::new(Box::into_raw(cloned) as *const (), &VTABLE)
}}

unsafe fn waker_wake(ptr: *const ()) { unsafe {
    let data = Box::from_raw(ptr as *mut WakerData);
    wake_by_ref_impl(&data);
}}

unsafe fn waker_wake_by_ref(ptr: *const ()) { unsafe {
    let data = &*(ptr as *const WakerData);
    wake_by_ref_impl(data);
}}

/// Core wake logic: transition the task state and enqueue if needed.
fn wake_by_ref_impl(data: &WakerData) {
    loop {
        match data.task_state.load(Ordering::Acquire) {
            IDLE => {
                // IDLE → SCHEDULED: put the task back in the ready queue.
                if data
                    .task_state
                    .compare_exchange_weak(IDLE, SCHEDULED, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    data.shared.enqueue_task(data.task_id);
                    return;
                }
                // CAS failed — state changed under us, retry.
            }
            RUNNING => {
                // RUNNING → NOTIFIED: the worker will re-schedule after poll.
                if data
                    .task_state
                    .compare_exchange_weak(RUNNING, NOTIFIED, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    return;
                }
                // CAS failed — state changed (e.g. poll just finished), retry.
            }
            // Already SCHEDULED or NOTIFIED — nothing to do.
            _ => return,
        }
    }
}

unsafe fn waker_drop(ptr: *const ()) { unsafe {
    let _ = Box::from_raw(ptr as *mut WakerData);
}}

pub(crate) fn make_waker(
    task_id: usize,
    task_state: Arc<AtomicU8>,
    shared: Arc<SharedState>,
) -> Waker {
    let data = Box::new(WakerData {
        task_id,
        task_state,
        shared,
    });
    let raw = RawWaker::new(Box::into_raw(data) as *const (), &VTABLE);
    // SAFETY: the vtable correctly manages the WakerData heap allocation.
    unsafe { Waker::from_raw(raw) }
}

// ---------------------------------------------------------------------------
// Worker thread
// ---------------------------------------------------------------------------

/// Run a worker loop: dequeue tasks, poll them, remove completed ones.
///
/// When no tasks are ready the worker tries to become the I/O driver.
pub(crate) fn run_worker(shared: Arc<SharedState>) {
    loop {
        if shared.shutdown.load(Ordering::Relaxed) {
            return;
        }

        // 1. Try to dequeue a ready task (non-blocking).
        let task_id = shared.ready_queue.lock().unwrap().dequeue();

        if let Some(task_id) = task_id {
            poll_task(task_id, &shared);
            continue;
        }

        // 2. No tasks — try to become the I/O driver.
        if shared
            .driver_token
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            drive_io(&shared);
            shared.driver_token.store(false, Ordering::Release);
            continue;
        }

        // 3. Another worker is driving — sleep on the condvar.
        let queue = shared.ready_queue.lock().unwrap();
        if !queue.is_empty() {
            continue;
        }
        if shared.shutdown.load(Ordering::Relaxed) {
            return;
        }
        // Longer timeout is fine — condvar.notify_one() and reactor.wake()
        // provide instant wake-up when new work arrives.
        let (_guard, _timeout) = shared
            .condvar
            .wait_timeout(queue, Duration::from_millis(100))
            .unwrap();
    }
}

/// Poll a single task by ID.
fn poll_task(task_id: usize, shared: &Arc<SharedState>) {
    let task = shared.tasks.lock().unwrap().get(&task_id).cloned();

    let Some(task) = task else { return };

    // Transition SCHEDULED → RUNNING.
    if task
        .state
        .compare_exchange(SCHEDULED, RUNNING, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        // Not in SCHEDULED state — another waker or worker got here first.
        return;
    }

    // Build waker with a handle to the task's atomic state.
    let waker = make_waker(task.id, task.state.clone(), shared.clone());
    let mut cx = Context::from_waker(&waker);

    // SAFETY: we just CAS-ed into RUNNING — we are the only thread that
    // will touch the future until we leave RUNNING.
    //
    // We wrap the poll in `catch_unwind` so that a panicking task does not
    // tear down the entire worker thread (and potentially hang `block_on`).
    let poll_result = panic::catch_unwind(AssertUnwindSafe(|| unsafe { task.poll(&mut cx) }));

    match poll_result {
        Ok(Poll::Ready(())) => {
            // Task is done — remove it.
            shared.tasks.lock().unwrap().remove(&task_id);
            shared.active_count.fetch_sub(1, Ordering::Relaxed);
            shared.condvar.notify_all();
        }
        Err(_panic) => {
            // Task panicked — treat it as completed so we don't poll it
            // again, and avoid crashing the worker thread.
            eprintln!("mini-async-runtime: task {task_id} panicked");
            shared.tasks.lock().unwrap().remove(&task_id);
            shared.active_count.fetch_sub(1, Ordering::Relaxed);
            shared.condvar.notify_all();
        }
        Ok(Poll::Pending) => {
            // Try RUNNING → IDLE.
            if task
                .state
                .compare_exchange(RUNNING, IDLE, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                // Must be NOTIFIED — a waker fired during poll.
                // Re-schedule the task.
                task.state.store(SCHEDULED, Ordering::Release);
                shared.enqueue_task(task_id);
            }
        }
    }
}

/// Act as the I/O driver: process timers, poll epoll, process timers again.
///
/// The epoll timeout is set to the next timer deadline (or blocks
/// indefinitely if no timers are pending).  `reactor.wake()` via eventfd
/// ensures we return immediately when new tasks are enqueued.
/// Act as the I/O driver: process timers, poll epoll, process timers again.
///
/// # Safety
///
/// Called only while holding the `driver_token` — this is the single-thread
/// guarantee required by `TimerWheel::process`.
fn drive_io(shared: &SharedState) {
    // SAFETY: we hold the driver_token.
    let timeout = unsafe {
        let next_deadline = shared.timers.process();
        if shared.timers.has_pending() {
            Some(next_deadline.unwrap_or(Duration::from_millis(1)))
        } else {
            Some(next_deadline.unwrap_or(Duration::from_millis(5)))
        }
    };

    let _ = shared.reactor.poll(timeout);

    // SAFETY: we still hold the driver_token.
    unsafe {
        shared.timers.process();
    }
}
