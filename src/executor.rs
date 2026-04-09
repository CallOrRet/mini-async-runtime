//! Task executor.
//!
//! The executor maintains a queue of runnable tasks. Each iteration it drains
//! the queue, polling every task once. Tasks that return `Poll::Pending` are
//! kept alive; tasks that return `Poll::Ready` are removed.
//!
//! The executor cooperates with the reactor: when the run queue is empty it
//! asks the reactor to wait for I/O or timer events, which will wake tasks
//! and re-populate the queue.
//!
//! ## Why `Rc<RefCell>` instead of `Arc<Mutex>`?
//!
//! This is a single-threaded runtime — all tasks run on one thread. Using
//! atomic reference counts and OS mutexes would be unnecessary overhead.
//! Instead we use `Rc<RefCell>` for shared state and build wakers via the
//! low-level [`RawWaker`] API (since `Rc` is not `Send`/`Sync`, we cannot
//! use the higher-level `Wake` trait which requires `Arc`).

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

use crate::task::Task;

// ---------------------------------------------------------------------------
// ReadyQueue — O(1) enqueue with deduplication
// ---------------------------------------------------------------------------

/// A FIFO queue of task ids with O(1) duplicate detection.
///
/// Maintains a `VecDeque` for ordering and a `HashSet` for fast membership
/// checks, so `enqueue` is O(1) amortized instead of O(n).
pub(crate) struct ReadyQueue {
    queue: VecDeque<usize>,
    set: HashSet<usize>,
}

impl ReadyQueue {
    fn new() -> Self {
        ReadyQueue {
            queue: VecDeque::new(),
            set: HashSet::new(),
        }
    }

    /// Push a task id if it is not already in the queue.
    fn push(&mut self, task_id: usize) {
        if self.set.insert(task_id) {
            self.queue.push_back(task_id);
        }
    }

    /// Drain all task ids out, returning them as a `Vec`.
    fn drain(&mut self) -> Vec<usize> {
        self.set.clear();
        self.queue.drain(..).collect()
    }

    /// Check whether the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }
}

/// Shared handle to the ready queue, used by both the executor and wakers.
pub(crate) type SharedReadyQueue = Rc<RefCell<ReadyQueue>>;

// ---------------------------------------------------------------------------
// RawWaker implementation for single-threaded wakers
// ---------------------------------------------------------------------------
//
// A `Waker` must be `Send + Sync`, but our data (`Rc<RefCell>`) is not.
// This is sound because the waker is only ever used on the same thread
// that created it — the runtime is single-threaded. We encode the task id
// and a pointer to the ready queue into a heap-allocated `WakerData` struct,
// and implement the RawWaker vtable manually.

/// Data carried inside each waker.
struct WakerData {
    task_id: usize,
    queue: SharedReadyQueue,
}

/// Build a `Waker` for the given task id that pushes to `queue` when woken.
fn make_waker(task_id: usize, queue: SharedReadyQueue) -> Waker {
    let data = Box::new(WakerData { task_id, queue });
    let ptr = Box::into_raw(data) as *const ();
    // SAFETY: the vtable functions below correctly manage the WakerData lifetime.
    unsafe { Waker::from_raw(RawWaker::new(ptr, &VTABLE)) }
}

const VTABLE: RawWakerVTable =
    RawWakerVTable::new(waker_clone, waker_wake, waker_wake_by_ref, waker_drop);

/// Clone: allocate a new `WakerData` with the same contents.
unsafe fn waker_clone(ptr: *const ()) -> RawWaker { unsafe {
    let data = &*(ptr as *const WakerData);
    let cloned = Box::new(WakerData {
        task_id: data.task_id,
        queue: Rc::clone(&data.queue),
    });
    RawWaker::new(Box::into_raw(cloned) as *const (), &VTABLE)
}}

/// Wake (by value): enqueue the task and free the WakerData.
unsafe fn waker_wake(ptr: *const ()) { unsafe {
    let data = Box::from_raw(ptr as *mut WakerData);
    data.queue.borrow_mut().push(data.task_id);
}}

/// Wake by reference: enqueue the task but do NOT free the WakerData.
unsafe fn waker_wake_by_ref(ptr: *const ()) { unsafe {
    let data = &*(ptr as *const WakerData);
    data.queue.borrow_mut().push(data.task_id);
}}

/// Drop: free the WakerData.
unsafe fn waker_drop(ptr: *const ()) { unsafe {
    drop(Box::from_raw(ptr as *mut WakerData));
}}

// ---------------------------------------------------------------------------
// Executor
// ---------------------------------------------------------------------------

/// The executor that drives tasks to completion.
pub(crate) struct Executor {
    /// All live tasks, keyed by id.
    pub tasks: HashMap<usize, Rc<Task>>,
    /// Queue of task ids that are ready to be polled.
    pub ready_queue: SharedReadyQueue,
}

impl Executor {
    /// Create a new, empty executor.
    pub fn new() -> Self {
        Executor {
            tasks: HashMap::new(),
            ready_queue: Rc::new(RefCell::new(ReadyQueue::new())),
        }
    }

    /// Submit a task to the executor. It will be polled on the next tick.
    pub fn spawn(&mut self, task: Rc<Task>) {
        let id = task.id;
        self.tasks.insert(id, task);
        self.ready_queue.borrow_mut().push(id);
    }

    /// Run one tick: poll every ready task once.
    ///
    /// Returns `true` if at least one task was polled.
    pub fn tick(&mut self) -> bool {
        let ready = self.ready_queue.borrow_mut().drain();

        if ready.is_empty() {
            return false;
        }

        for task_id in ready {
            let task = match self.tasks.get(&task_id) {
                Some(t) => Rc::clone(t),
                None => continue, // task was already completed
            };

            let waker = make_waker(task_id, Rc::clone(&self.ready_queue));
            let mut cx = Context::from_waker(&waker);

            match task.poll(&mut cx) {
                Poll::Ready(()) => {
                    // Task finished — remove it.
                    self.tasks.remove(&task_id);
                }
                Poll::Pending => {
                    // Task is waiting; it will be re-enqueued when its waker fires.
                }
            }
        }

        true
    }

    /// Returns `true` when there are no more tasks to run.
    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }
}
