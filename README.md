# Mini Async Runtime

A minimal async runtime for Rust, built from scratch for learning purposes.
Implements the core components found in production runtimes like
[Tokio](https://tokio.rs), in roughly 2000 lines of code with only `libc` as a
dependency.

## Features

- **Single-threaded runtime** (`Runtime`) — simple, zero-atomics, `Rc<RefCell>` based.
- **Multi-threaded runtime** (`MultiThreadRuntime`) — thread-pool with work queue, Tokio-style driver election.
- **Epoll reactor** — non-blocking I/O with internal locking (`epoll_wait` never holds a lock).
- **Timer wheel** — `BinaryHeap`-based timers with lock-free per-timer state.
- **Async TCP** — `TcpListener` / `TcpStream` for both runtimes.
- **MPSC channel** — unbounded async channel for inter-task communication.
- **JoinHandle** — await the result of spawned tasks.

## Quick Start

```rust
use mini_async_runtime::Runtime;

let mut rt = Runtime::new().unwrap();
rt.block_on(async {
    println!("Hello from async!");
});
```

Multi-threaded:

```rust
use mini_async_runtime::multi_thread::MultiThreadRuntime;

let rt = MultiThreadRuntime::new(4).unwrap();
let h1 = rt.spawn(async { 1 });
let h2 = rt.spawn(async { 2 });
let sum = rt.block_on(async { h1.await + h2.await });
assert_eq!(sum, 3);
```

## Architecture

```
┌──────────────────────────────────────────────────────────┐
│                       Runtime                            │
│                                                          │
│  ┌────────────┐   ┌────────────┐   ┌─────────────────┐   │
│  │  Executor  │   │  Reactor   │   │   TimerWheel    │   │
│  │            │   │  (epoll)   │   │   (BinaryHeap)  │   │
│  │ ReadyQueue │   │            │   │                 │   │
│  │ TaskMap    │   │ eventfd    │   │                 │   │
│  └─────┬──────┘   └─────┬──────┘   └────────┬────────┘   │
│        │                │                   │            │
│        └────────────────┴───────────────────┘            │
│                     Waker: re-enqueue task               │
└──────────────────────────────────────────────────────────┘
```

### Single-Threaded Runtime

| Component    | Implementation                                          |
| ------------ | ------------------------------------------------------- |
| Task storage | `Rc<Task>` with `RefCell<BoxFuture>`                    |
| Sharing      | `Rc<RefCell>` — no atomic overhead                      |
| Reactor      | `Rc<Reactor>` (internal `Mutex` on registrations)       |
| Event loop   | `block_on` drives executor → reactor → timers in a loop |

### Multi-Threaded Runtime

| Component          | Implementation                                                                   |
| ------------------ | -------------------------------------------------------------------------------- |
| Task storage       | `Arc<Task>` with `UnsafeCell<BoxFuture>` + `AtomicU8` state                      |
| Task state machine | `IDLE → SCHEDULED → RUNNING → IDLE/NOTIFIED` (CAS, no Mutex)                     |
| Worker pool        | N threads pulling from shared `ReadyQueue`                                       |
| Driver election    | `AtomicBool` CAS — idle worker becomes I/O driver                                |
| Reactor            | `Arc<Reactor>` — `epoll_wait` outside lock, `eventfd` for instant wake           |
| Timer wheel        | Inbox (`Mutex<Vec>`) + heap (`UnsafeCell`) — register and process never contend  |
| Per-timer state    | `AtomicBool` + `AtomicWaker` — fully lock-free                                   |
| Wake strategy      | `condvar.notify` for sleeping workers, `reactor.wake()` only if driver is active |

### Worker Loop (Multi-Threaded)

```
loop {
    if let Some(task) = ready_queue.dequeue() {
        CAS(SCHEDULED → RUNNING);
        poll(task);
        CAS(RUNNING → IDLE) or handle NOTIFIED;
        continue;
    }

    if driver_token.CAS(false → true) {
        // I am the I/O driver
        timers.process();
        reactor.poll(timeout);   // epoll_wait, no lock held
        timers.process();
        driver_token.store(false);
        continue;
    }

    // Someone else is driving — sleep
    condvar.wait_timeout(100ms);
}
```

## Module Structure

```
src/
├── lib.rs              # Public API, re-exports, tests
├── main.rs             # Usage examples (single + multi-threaded)
├── runtime.rs          # Single-threaded Runtime (block_on, spawn, sleep)
├── executor.rs         # Single-threaded executor + RawWaker
├── task.rs             # Single-threaded Task (Rc<RefCell>)
├── reactor.rs          # Shared epoll reactor (internal Mutex, eventfd wake)
├── timer.rs            # Single-threaded TimerWheel
├── channel.rs          # Unbounded MPSC channel
├── join_handle.rs      # JoinHandle<T>
├── net.rs              # Async TcpListener / TcpStream
└── multi_thread/
    ├── mod.rs          # Module exports
    ├── runtime.rs      # MultiThreadRuntime (worker pool, block_on)
    ├── executor.rs     # Worker loop, driver election, atomic wakers
    ├── task.rs         # Task (UnsafeCell + AtomicU8 state machine)
    ├── timer.rs        # TimerWheel (inbox/heap split, AtomicWaker)
    └── net.rs          # Thread-safe TcpListener / TcpStream
```

## Key Design Decisions

**Why `UnsafeCell` instead of `Mutex` for task futures?**
`Future::poll` requires `&mut` access. In a multi-threaded runtime the task is
behind `Arc`. Rather than using `Mutex` (a syscall per poll), we use an atomic
state machine to guarantee exclusive access, then access the future through
`UnsafeCell` with zero overhead.

**Why separate inbox/heap in TimerWheel?**
`register()` (any thread) and `process()` (driver only) would contend on the
same lock. By pushing new timers to an inbox `Mutex<Vec>` and only draining into
the heap during `process()`, the two paths never touch the same data structure
simultaneously.

**Why eventfd?**
Without it, the driver must use short timeout polling (5ms) to stay responsive,
adding latency. The eventfd lets any thread instantly interrupt `epoll_wait`,
so the driver can block indefinitely and still respond in microseconds.

## Running

```bash
cargo run          # Run all examples
cargo test         # Run all 20 tests
cargo clippy       # Lint check
```
