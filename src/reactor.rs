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
            Some(d) if d.is_zero() => 0,
            // Round any non-zero sub-millisecond duration up to 1ms so that
            // e.g. `Duration::from_micros(500)` doesn't degenerate into a
            // zero-timeout call that busy-spins.
            Some(d) => {
                let ns = d.as_nanos();
                let ms = ns.div_ceil(1_000_000);
                ms.min(i32::MAX as u128) as i32
            }
        };

        // 1. Block on epoll — NO lock held.
        let mut events = [libc::epoll_event { events: 0, u64: 0 }; 64];
        let n = self.epoll.wait(&mut events, timeout_ms)?;

        // 2. Collect wakers under the lock, then call them after releasing
        //    it.  This avoids calling arbitrary waker code while holding the
        //    registrations mutex, preventing potential deadlocks if a waker
        //    implementation ever needs to re-enter the reactor.
        //
        //    Because we use EPOLLONESHOT, an fd is disabled in the kernel
        //    after one event.  If the fd had *both* read and write interest
        //    registered but only one direction fired, we must re-arm with
        //    the surviving direction or that side's task is stranded.
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

                let Some(reg) = regs.get_mut(&fd) else {
                    continue;
                };

                // EPOLLERR / EPOLLHUP are reported regardless of the mask;
                // wake all interested parties so they observe the error
                // through their next read/write syscall.
                let err_or_hup =
                    event.events & (libc::EPOLLERR | libc::EPOLLHUP) as u32 != 0;

                if (event.events & READABLE != 0 || err_or_hup)
                    && let Some(waker) = reg.read_waker.take()
                {
                    wakers.push(waker);
                }
                if (event.events & WRITABLE != 0 || err_or_hup)
                    && let Some(waker) = reg.write_waker.take()
                {
                    wakers.push(waker);
                }

                // Re-arm any direction whose waker is still registered.
                // We keep the map entry either way: the fd remains in the
                // kernel's interest list (just disabled by ONESHOT), so a
                // future register call uses EPOLL_CTL_MOD as expected.
                let mut remaining = 0u32;
                if reg.read_waker.is_some() {
                    remaining |= READABLE;
                }
                if reg.write_waker.is_some() {
                    remaining |= WRITABLE;
                }
                if remaining != 0 {
                    remaining |= libc::EPOLLET as u32 | libc::EPOLLONESHOT as u32;
                    // Best-effort: if rearm fails (e.g. fd was closed) the
                    // surviving task will likely observe the error on its
                    // next read/write syscall, or the user will deregister.
                    let _ = self.epoll.ctl(
                        libc::EPOLL_CTL_MOD,
                        fd,
                        remaining,
                        fd as u64,
                    );
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::task::Wake;

    struct CountWaker(AtomicUsize);

    impl CountWaker {
        fn count(&self) -> usize {
            self.0.load(AtomicOrdering::SeqCst)
        }
    }

    impl Wake for CountWaker {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, AtomicOrdering::SeqCst);
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, AtomicOrdering::SeqCst);
        }
    }

    /// Make a non-blocking AF_UNIX socketpair, returning `(fd_a, fd_b)`.
    fn nonblocking_socketpair() -> (RawFd, RawFd) {
        let mut fds = [0i32; 2];
        let ret = unsafe {
            libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr())
        };
        assert_eq!(ret, 0, "socketpair failed");
        for fd in fds {
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            assert!(flags >= 0);
            let r = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
            assert_eq!(r, 0);
        }
        (fds[0], fds[1])
    }

    fn close(fd: RawFd) {
        unsafe {
            libc::close(fd);
        }
    }

    /// Regression test for the ET+ONESHOT rearm bug: when an fd has both
    /// read and write interest registered and only one direction fires,
    /// the other direction must remain armed.
    ///
    /// Pre-fix behaviour: after the first poll consumes the write_waker,
    /// the fd stays disabled in the kernel (ONESHOT) and the read_waker
    /// is never woken even after data arrives. This test would hang or
    /// fail on the second `poll()`.
    #[test]
    fn rearm_other_direction_when_only_one_event_fires() {
        let (fd_a, fd_b) = nonblocking_socketpair();
        let reactor = Reactor::new().unwrap();

        let read_w = Arc::new(CountWaker(AtomicUsize::new(0)));
        let write_w = Arc::new(CountWaker(AtomicUsize::new(0)));

        reactor
            .register_readable(fd_a, Waker::from(read_w.clone()))
            .unwrap();
        reactor
            .register_writable(fd_a, Waker::from(write_w.clone()))
            .unwrap();

        // Fresh AF_UNIX stream socket: writable, not readable. The first
        // epoll_wait reports EPOLLOUT only.
        reactor.poll(Some(Duration::from_millis(100))).unwrap();
        assert_eq!(write_w.count(), 1, "writable side should fire");
        assert_eq!(read_w.count(), 0, "no data yet — read shouldn't fire");

        // Make fd_a readable.
        let n = unsafe {
            libc::write(fd_b, b"x".as_ptr() as *const libc::c_void, 1)
        };
        assert_eq!(n, 1);

        // The read_waker must still be armed. Without the rearm fix, the
        // kernel still has fd_a in EPOLL_CTL_DISABLED-by-ONESHOT state and
        // this poll would time out without firing the read waker.
        reactor.poll(Some(Duration::from_millis(200))).unwrap();
        assert_eq!(
            read_w.count(),
            1,
            "rearm regression: read waker stranded by oneshot"
        );

        let _ = reactor.deregister(fd_a);
        close(fd_a);
        close(fd_b);
    }

    /// Sanity: `wake()` from another thread breaks a blocking `poll()`.
    #[test]
    fn wake_breaks_blocking_poll() {
        let reactor = Arc::new(Reactor::new().unwrap());
        let r2 = reactor.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            r2.wake();
        });

        let start = std::time::Instant::now();
        // Block "forever"; should be interrupted by wake() ~20ms in.
        reactor.poll(None).unwrap();
        let elapsed = start.elapsed();
        handle.join().unwrap();

        assert!(
            elapsed < Duration::from_millis(500),
            "wake() did not interrupt poll: {elapsed:?}"
        );
    }

    /// Sub-millisecond timeouts should not degenerate into a busy spin.
    #[test]
    fn submillisecond_timeout_does_not_busy_spin() {
        let reactor = Reactor::new().unwrap();
        // 10 polls of 500µs each. Pre-fix: each rounds down to 0ms and
        // returns immediately, so the total is near-zero CPU time but
        // performs 10 syscalls with no wait. Post-fix: each rounds up
        // to 1ms, so the total is at least ~10ms of actual sleeping.
        let start = std::time::Instant::now();
        for _ in 0..10 {
            reactor.poll(Some(Duration::from_micros(500))).unwrap();
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(5),
            "sub-ms timeout regressed to busy-spin: {elapsed:?}"
        );
    }
}
