//! Thread-safe timer wheel and [`Sleep`] future.
//!
//! Any thread can call [`TimerWheel::register`] to add a timer (pushes to a
//! lock-free-ish inbox).  Only the driver calls [`TimerWheel::process`] to
//! drain the inbox into the heap and fire expired entries.  The two paths
//! never contend on the same lock.
//!
//! Per-timer state uses atomics instead of a `Mutex`: an `AtomicBool` for
//! the completed flag and a simple atomic-waker cell for the waker.

use std::cell::UnsafeCell;
use std::collections::BinaryHeap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Atomic waker cell (replaces Mutex<Option<Waker>> for per-timer state)
// ---------------------------------------------------------------------------

const WAKER_EMPTY: u8 = 0;
const WAKER_STORED: u8 = 1;
const WAKER_WAKING: u8 = 2;

/// A lock-free cell that stores at most one [`Waker`].
///
/// - `register(waker)`: store or replace the waker.
/// - `wake()`: take and wake the stored waker, if any.
struct AtomicWaker {
    state: AtomicU8,
    waker: UnsafeCell<Option<Waker>>,
}

unsafe impl Send for AtomicWaker {}
unsafe impl Sync for AtomicWaker {}

impl AtomicWaker {
    fn new() -> Self {
        AtomicWaker {
            state: AtomicU8::new(WAKER_EMPTY),
            waker: UnsafeCell::new(None),
        }
    }

    /// Store a waker (called by `Sleep::poll`).
    fn register(&self, waker: &Waker) {
        // Spin until we can claim the cell.
        loop {
            match self.state.compare_exchange_weak(
                WAKER_EMPTY,
                WAKER_WAKING,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    // SAFETY: we hold WAKING — exclusive access.
                    unsafe { *self.waker.get() = Some(waker.clone()) };
                    self.state.store(WAKER_STORED, Ordering::Release);
                    return;
                }
                Err(WAKER_STORED) => {
                    // Replace existing waker.
                    if self
                        .state
                        .compare_exchange_weak(
                            WAKER_STORED,
                            WAKER_WAKING,
                            Ordering::Acquire,
                            Ordering::Relaxed,
                        )
                        .is_ok()
                    {
                        unsafe { *self.waker.get() = Some(waker.clone()) };
                        self.state.store(WAKER_STORED, Ordering::Release);
                        return;
                    }
                    // CAS failed, retry.
                }
                Err(_) => {
                    // WAKING — another thread is in register/wake; spin.
                    std::hint::spin_loop();
                }
            }
        }
    }

    /// Take and wake the stored waker (called by `TimerWheel::process`).
    fn wake(&self) {
        loop {
            match self.state.compare_exchange_weak(
                WAKER_STORED,
                WAKER_WAKING,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    // SAFETY: we hold WAKING — exclusive access.
                    let waker = unsafe { (*self.waker.get()).take() };
                    self.state.store(WAKER_EMPTY, Ordering::Release);
                    if let Some(w) = waker {
                        w.wake();
                    }
                    return;
                }
                Err(WAKER_EMPTY) => return,       // nothing to wake
                Err(_) => std::hint::spin_loop(), // WAKING — spin
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Per-timer state (lock-free)
// ---------------------------------------------------------------------------

struct TimerState {
    completed: AtomicBool,
    waker: AtomicWaker,
}

// ---------------------------------------------------------------------------
// Heap entry
// ---------------------------------------------------------------------------

struct TimerEntry {
    deadline: Instant,
    state: Arc<TimerState>,
}

/// Ordering: earliest deadline first (min-heap via reversed `Ord`).
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
        other.deadline.cmp(&self.deadline) // reversed for min-heap
    }
}

// ---------------------------------------------------------------------------
// Timer wheel
// ---------------------------------------------------------------------------

/// Thread-safe timer wheel.
///
/// `register()` pushes to an inbox (`Mutex<Vec>`).  `process()` drains
/// the inbox into the heap and fires expired timers.  The heap is behind
/// an `UnsafeCell` (driver-only access).  Per-timer state is fully
/// lock-free (atomics).
pub(crate) struct TimerWheel {
    /// Inbox — any thread pushes new timers here.
    inbox: Mutex<Vec<TimerEntry>>,
    /// The actual min-heap — only the driver accesses this.
    heap: UnsafeCell<BinaryHeap<TimerEntry>>,
}

// SAFETY: `inbox` is a Mutex (Send+Sync).  `heap` is only accessed by the
// driver thread (enforced by the `driver_token` CAS in the executor).
unsafe impl Send for TimerWheel {}
unsafe impl Sync for TimerWheel {}

impl TimerWheel {
    pub fn new() -> Self {
        TimerWheel {
            inbox: Mutex::new(Vec::new()),
            heap: UnsafeCell::new(BinaryHeap::new()),
        }
    }

    /// Register a new timer.  Can be called from **any** thread.
    ///
    /// Only briefly locks the inbox — never touches the heap.
    pub fn register(&self, deadline: Instant) -> Sleep {
        let state = Arc::new(TimerState {
            completed: AtomicBool::new(false),
            waker: AtomicWaker::new(),
        });
        self.inbox.lock().unwrap().push(TimerEntry {
            deadline,
            state: state.clone(),
        });
        Sleep { state }
    }

    /// Drain the inbox, fire expired timers, and return the duration until
    /// the next one (or `None` if no timers remain).
    ///
    /// # Safety
    ///
    /// Must only be called by the driver thread (i.e. while holding the
    /// `driver_token`).
    pub unsafe fn process(&self) -> Option<Duration> { unsafe {
        let heap = &mut *self.heap.get();

        // Drain inbox into heap (brief lock).
        {
            let mut inbox = self.inbox.lock().unwrap();
            heap.extend(inbox.drain(..));
        }

        // Fire expired entries (no lock).
        let now = Instant::now();
        while let Some(entry) = heap.peek() {
            if entry.deadline <= now {
                let entry = heap.pop().unwrap();
                entry.state.completed.store(true, Ordering::Release);
                entry.state.waker.wake();
            } else {
                return Some(entry.deadline - now);
            }
        }
        None
    }}

    /// Check if there are pending timers (inbox or heap).
    ///
    /// # Safety
    ///
    /// The heap check must only be called by the driver thread.
    pub unsafe fn has_pending(&self) -> bool { unsafe {
        !(*self.heap.get()).is_empty() || !self.inbox.lock().unwrap().is_empty()
    }}
}

// ---------------------------------------------------------------------------
// Sleep future
// ---------------------------------------------------------------------------

/// A future that completes after a deadline, for the multi-threaded runtime.
///
/// Created by [`MultiThreadRuntime::sleep`](super::MultiThreadRuntime::sleep).
pub struct Sleep {
    state: Arc<TimerState>,
}

impl Future for Sleep {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.state.completed.load(Ordering::Acquire) {
            Poll::Ready(())
        } else {
            self.state.waker.register(cx.waker());
            // Double-check after registering to avoid missed wake.
            if self.state.completed.load(Ordering::Acquire) {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }
    }
}
