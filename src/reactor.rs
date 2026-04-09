//! Epoll-based I/O reactor.
//!
//! The reactor monitors file descriptors for readiness events using Linux's
//! `epoll` interface. When a file descriptor becomes ready, the reactor wakes
//! the corresponding task so the executor can poll it again.
//!
//! All public methods take `&self` and use an internal [`Mutex`] to protect
//! the registrations map.  [`Reactor::poll`] only holds the lock **after**
//! `epoll_wait` returns — never during the blocking wait.
//!
//! An **eventfd** allows any thread to interrupt a blocking `epoll_wait`
//! via [`Reactor::wake`], so the driver never has to use short timeout
//! polling.

use std::collections::HashMap;
use std::io;
use std::os::unix::io::RawFd;
use std::rc::Rc;
use std::sync::Mutex;
use std::task::Waker;
use std::time::Duration;

// ---- Raw epoll syscall wrappers ----

/// Events we care about from epoll.
pub(crate) const READABLE: u32 = libc::EPOLLIN as u32;
pub(crate) const WRITABLE: u32 = libc::EPOLLOUT as u32;

/// Thin wrapper around an epoll file descriptor.
struct Epoll {
    fd: RawFd,
}

impl Epoll {
    fn new() -> io::Result<Self> {
        let fd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Epoll { fd })
    }

    fn ctl(&self, op: i32, fd: RawFd, events: u32, data: u64) -> io::Result<()> {
        let mut event = libc::epoll_event { events, u64: data };
        let ret = unsafe { libc::epoll_ctl(self.fd, op, fd, &mut event) };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn wait(&self, events: &mut [libc::epoll_event], timeout_ms: i32) -> io::Result<usize> {
        let ret = unsafe {
            libc::epoll_wait(
                self.fd,
                events.as_mut_ptr(),
                events.len() as i32,
                timeout_ms,
            )
        };
        if ret < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                return Ok(0);
            }
            return Err(err);
        }
        Ok(ret as usize)
    }
}

impl Drop for Epoll {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.fd);
        }
    }
}

/// Per-fd registration: which events are monitored and who to wake.
struct Registration {
    read_waker: Option<Waker>,
    write_waker: Option<Waker>,
}

/// The I/O reactor that drives non-blocking file descriptors.
///
/// All public methods take `&self`.  The registrations map is protected by
/// an internal [`Mutex`]; the epoll fd is thread-safe at the syscall level.
///
/// An **eventfd** is registered with epoll so that [`Reactor::wake`] can
/// break a blocking `epoll_wait` instantly.
///
/// Shared via `Rc<Reactor>` (single-threaded) or `Arc<Reactor>`
/// (multi-threaded) — no external `RefCell` or `Mutex` needed.
pub(crate) struct Reactor {
    epoll: Epoll,
    registrations: Mutex<HashMap<RawFd, Registration>>,
    /// eventfd used by [`wake`] to interrupt `epoll_wait`.
    wake_fd: RawFd,
}

// SAFETY: Epoll fd, eventfd, and Mutex<HashMap> are all safe to
// send/share.  epoll syscalls and eventfd write are thread-safe.
unsafe impl Send for Reactor {}
unsafe impl Sync for Reactor {}

impl Reactor {
    /// Create a new reactor backed by epoll, with an eventfd for wake-up.
    pub fn new() -> io::Result<Self> {
        let epoll = Epoll::new()?;

        // Create an eventfd for cross-thread wake-up.
        let wake_fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if wake_fd < 0 {
            return Err(io::Error::last_os_error());
        }

        // Register the eventfd with epoll using **level-triggered** mode.
        // Edge-triggered would risk missing a wake() that fires between the
        // driver checking the ready queue and entering epoll_wait.
        // Level-triggered ensures epoll_wait always returns while the eventfd
        // counter is non-zero.
        epoll.ctl(
            libc::EPOLL_CTL_ADD,
            wake_fd,
            libc::EPOLLIN as u32,
            wake_fd as u64,
        )?;

        Ok(Reactor {
            epoll,
            registrations: Mutex::new(HashMap::new()),
            wake_fd,
        })
    }

    /// Interrupt a blocking [`poll`](Reactor::poll) from any thread.
    ///
    /// Writing to the eventfd causes `epoll_wait` to return immediately.
    pub fn wake(&self) {
        let val: u64 = 1;
        unsafe {
            libc::write(self.wake_fd, &val as *const u64 as *const libc::c_void, 8);
        }
    }

    /// Register interest in readability for `fd`, storing the given waker.
    pub fn register_readable(&self, fd: RawFd, waker: Waker) -> io::Result<()> {
        self.register(fd, waker, true)
    }

    /// Register interest in writability for `fd`, storing the given waker.
    pub fn register_writable(&self, fd: RawFd, waker: Waker) -> io::Result<()> {
        self.register(fd, waker, false)
    }

    fn register(&self, fd: RawFd, waker: Waker, is_read: bool) -> io::Result<()> {
        let mut regs = self.registrations.lock().unwrap();

        let is_new = !regs.contains_key(&fd);
        let reg = regs.entry(fd).or_insert(Registration {
            read_waker: None,
            write_waker: None,
        });

        // Save the old waker so we can rollback on epoll_ctl failure.
        let old_waker = if is_read {
            reg.read_waker.replace(waker)
        } else {
            reg.write_waker.replace(waker)
        };

        let mut mask = 0;
        if reg.read_waker.is_some() {
            mask |= READABLE;
        }
        if reg.write_waker.is_some() {
            mask |= WRITABLE;
        }
        mask |= libc::EPOLLET as u32 | libc::EPOLLONESHOT as u32;

        let op = if is_new {
            libc::EPOLL_CTL_ADD
        } else {
            libc::EPOLL_CTL_MOD
        };

        let result = self.epoll.ctl(op, fd, mask, fd as u64);
        if result.is_err() {
            // Rollback: restore the old waker so the map stays consistent.
            let reg = regs.get_mut(&fd).unwrap();
            if is_read {
                reg.read_waker = old_waker;
            } else {
                reg.write_waker = old_waker;
            }
            // If this was a new entry and epoll_ctl(ADD) failed, remove it.
            if is_new {
                regs.remove(&fd);
            }
        }
        result
    }

    /// Remove all interest in `fd`.
    pub fn deregister(&self, fd: RawFd) -> io::Result<()> {
        if self.registrations.lock().unwrap().remove(&fd).is_some() {
            let _ = self.epoll.ctl(libc::EPOLL_CTL_DEL, fd, 0, 0);
        }
        Ok(())
    }

    /// Poll for I/O events, waking any tasks whose fds are ready.
    ///
    /// The lock is NOT held during `epoll_wait`.  Use [`wake`](Reactor::wake)
    /// to interrupt a blocking wait from another thread.
    pub fn poll(&self, timeout: Option<Duration>) -> io::Result<()> {
        let timeout_ms = match timeout {
            None => -1,
            Some(d) => d.as_millis().min(i32::MAX as u128) as i32,
        };

        // 1. Block on epoll — NO lock held.
        let mut events = [libc::epoll_event { events: 0, u64: 0 }; 64];
        let n = self.epoll.wait(&mut events, timeout_ms)?;

        //    it.  This avoids calling arbitrary waker code while holding the
        //    registrations mutex, preventing potential deadlocks if a waker
        //    implementation ever needs to re-enter the reactor.
        let wakers_to_wake = {
            let mut regs = self.registrations.lock().unwrap();
            let mut wakers = Vec::with_capacity(events.len());
            for event in &events[..n] {
                let fd = event.u64 as RawFd;

                // Skip the wake eventfd — just drain it.
                if fd == self.wake_fd {
                    Self::drain_wake_fd(self.wake_fd);
                    continue;
                }
                if let Some(reg) = regs.get_mut(&fd) {
                    if event.events & READABLE != 0
                        && let Some(waker) = reg.read_waker.take()
                    {
                        wakers.push(waker);
                    }
                    if event.events & WRITABLE != 0
                        && let Some(waker) = reg.write_waker.take()
                    {
                        wakers.push(waker);
                    }
                }
            }
            wakers
            // lock released here
        };

        for waker in wakers_to_wake {
            waker.wake();
        }

        Ok(())
    }

    /// Drain the eventfd counter so it doesn't keep firing.
    fn drain_wake_fd(fd: RawFd) {
        let mut buf = 0u64;
        unsafe {
            libc::read(fd, &mut buf as *mut u64 as *mut libc::c_void, 8);
        }
    }
}

impl Drop for Reactor {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.wake_fd);
        }
    }
}

/// Single-threaded handle to the reactor.
pub(crate) type SharedReactor = Rc<Reactor>;

/// Create a new shared reactor.
pub(crate) fn new_shared_reactor() -> io::Result<SharedReactor> {
    Ok(Rc::new(Reactor::new()?))
}
