//! Timer and sleep support.
//!
//! Provides an async [`sleep`] function similar to `tokio::time::sleep`.
//! Timers are tracked by the runtime and checked each tick; when a timer
//! expires the corresponding task is woken.
//!
//! Uses `Rc<RefCell>` for shared state — no atomics or mutexes needed
//! in a single-threaded runtime.

use std::cell::RefCell;
use std::collections::BinaryHeap;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

/// A pending timer entry in the timer wheel.
struct TimerEntry {
    /// When this timer fires.
    deadline: Instant,
    /// Shared state with the `Sleep` future.
    state: Rc<RefCell<TimerState>>,
}

/// Ordering for the min-heap: earliest deadline first.
impl PartialEq for TimerEntry {
    fn eq(&self, other: &Self) -> bool {
        self.deadline == other.deadline
    }
}
impl Eq for TimerEntry {}
impl PartialOrd for TimerEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for TimerEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Reverse because BinaryHeap is a max-heap and we want min-heap.
        other.deadline.cmp(&self.deadline)
    }
}

/// Shared mutable state for a single timer.
struct TimerState {
    completed: bool,
    waker: Option<Waker>,
}

/// The timer wheel that tracks all pending timers.
///
/// The runtime calls [`TimerWheel::process`] each tick to fire expired timers.
pub(crate) struct TimerWheel {
    heap: BinaryHeap<TimerEntry>,
}

impl TimerWheel {
    pub fn new() -> Self {
        TimerWheel {
            heap: BinaryHeap::new(),
        }
    }

    /// Register a new timer and return a `Sleep` future.
    pub fn register(&mut self, deadline: Instant) -> Sleep {
        let state = Rc::new(RefCell::new(TimerState {
            completed: false,
            waker: None,
        }));
        self.heap.push(TimerEntry {
            deadline,
            state: Rc::clone(&state),
        });
        Sleep { state }
    }

    /// Process all timers whose deadline has passed, waking their tasks.
    ///
    /// Returns the duration until the next timer fires, or `None` if there
    /// are no pending timers.
    pub fn process(&mut self) -> Option<Duration> {
        let now = Instant::now();
        while let Some(entry) = self.heap.peek() {
            if entry.deadline <= now {
                let entry = self.heap.pop().unwrap();
                let mut state = entry.state.borrow_mut();
                state.completed = true;
                if let Some(waker) = state.waker.take() {
                    waker.wake();
                }
            } else {
                return Some(entry.deadline - now);
            }
        }
        None
    }

    /// Check if there are pending timers.
    pub fn has_pending(&self) -> bool {
        !self.heap.is_empty()
    }
}

/// Single-threaded handle to the timer wheel.
pub(crate) type SharedTimerWheel = Rc<RefCell<TimerWheel>>;

/// Create a new shared timer wheel.
pub(crate) fn new_shared_timer_wheel() -> SharedTimerWheel {
    Rc::new(RefCell::new(TimerWheel::new()))
}

/// A future that completes after a specified duration.
///
/// Created by the [`sleep`] function.
pub struct Sleep {
    state: Rc<RefCell<TimerState>>,
}

impl Future for Sleep {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let mut state = self.state.borrow_mut();
        if state.completed {
            Poll::Ready(())
        } else {
            state.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

// The public `sleep` function is provided via the runtime context in lib.rs.
