# Mini Async Runtime

一个从零实现的 Rust 异步运行时，用于学习 async runtime 内部原理。
约 2000 行代码，仅依赖 `libc`，实现了 [Tokio](https://tokio.rs) 等生产级
运行时的核心组件。

## 功能

- **单线程运行时** (`Runtime`) — 零原子操作，基于 `Rc<RefCell>`
- **多线程运行时** (`MultiThreadRuntime`) — 线程池 + 共享工作队列，Tokio 风格的 driver 竞选
- **Epoll reactor** — 非阻塞 I/O，内部锁，`epoll_wait` 期间不持有任何锁
- **定时器** — `BinaryHeap` 最小堆，per-timer 无锁状态（`AtomicBool` + `AtomicWaker`）
- **异步 TCP** — `TcpListener` / `TcpStream`，两套运行时各有对应版本
- **MPSC 通道** — 无界异步通道，用于任务间通信
- **JoinHandle** — 等待 spawn 出去的任务返回结果

## 快速上手

```rust
use mini_async_runtime::Runtime;

let mut rt = Runtime::new().unwrap();
rt.block_on(async {
    println!("Hello from async!");
});
```

多线程：

```rust
use mini_async_runtime::multi_thread::MultiThreadRuntime;

let rt = MultiThreadRuntime::new(4).unwrap();
let h1 = rt.spawn(async { 1 });
let h2 = rt.spawn(async { 2 });
let sum = rt.block_on(async { h1.await + h2.await });
assert_eq!(sum, 3);
```

## 架构总览

```
┌──────────────────────────────────────────────────────────┐
│                       Runtime                            │
│                                                          │
│  ┌────────────┐   ┌────────────┐   ┌─────────────────┐   │
│  │  Executor  │   │  Reactor   │   │   TimerWheel    │   │
│  │  任务调度  │   │  (epoll)   │   │  (BinaryHeap)   │   │
│  │            │   │            │   │                 │   │
│  │ ReadyQueue │   │ eventfd    │   │                 │   │
│  └─────┬──────┘   └─────┬──────┘   └────────┬────────┘   │
│        │                │                   │            │
│        └────────────────┴───────────────────┘            │
│                   Waker: 将任务重新入队                  │
└──────────────────────────────────────────────────────────┘
```

### 单线程运行时

| 组件     | 实现                                             |
| -------- | ------------------------------------------------ |
| 任务存储 | `Rc<Task>`，future 在 `RefCell<BoxFuture>` 中    |
| 共享机制 | `Rc<RefCell>` — 无原子操作开销                   |
| Reactor  | `Rc<Reactor>`（registrations 用内部 `Mutex`）    |
| 事件循环 | `block_on` 驱动 executor → reactor → timers 循环 |

### 多线程运行时

| 组件           | 实现                                                                    |
| -------------- | ----------------------------------------------------------------------- |
| 任务存储       | `Arc<Task>`，future 在 `UnsafeCell` 中 + `AtomicU8` 状态机              |
| 任务状态机     | `IDLE → SCHEDULED → RUNNING → IDLE/NOTIFIED`（CAS，无 Mutex）           |
| 工作线程池     | N 个线程从共享 `ReadyQueue` 拉取任务                                    |
| Driver 竞选    | `AtomicBool` CAS — 空闲 worker 竞争成为 I/O driver                      |
| Reactor        | `Arc<Reactor>` — `epoll_wait` 在锁外，`eventfd` 即时唤醒                |
| 定时器         | inbox（`Mutex<Vec>`）+ heap（`UnsafeCell`）— 注册和处理不争锁           |
| Per-timer 状态 | `AtomicBool` + `AtomicWaker` — 完全无锁                                 |
| 唤醒策略       | `condvar.notify` 唤醒休眠 worker，仅在 driver 活跃时调 `reactor.wake()` |

### Worker 循环（多线程）

```
loop {
    if let Some(task) = ready_queue.dequeue() {
        CAS(SCHEDULED → RUNNING);
        poll(task);
        CAS(RUNNING → IDLE) 或处理 NOTIFIED;
        continue;
    }

    if driver_token.CAS(false → true) {
        // 我是 I/O driver
        timers.process();
        reactor.poll(timeout);   // epoll_wait，不持锁
        timers.process();
        driver_token.store(false);
        continue;
    }

    // 别人在 drive — 休眠
    condvar.wait_timeout(100ms);
}
```

## 模块结构

```
src/
├── lib.rs              # 公共 API、re-exports、测试
├── main.rs             # 使用示例（单线程 + 多线程）
├── runtime.rs          # 单线程 Runtime（block_on, spawn, sleep）
├── executor.rs         # 单线程 executor + RawWaker
├── task.rs             # 单线程 Task（Rc<RefCell>）
├── reactor.rs          # 共享 epoll reactor（内部 Mutex，eventfd 唤醒）
├── timer.rs            # 单线程 TimerWheel
├── channel.rs          # 无界 MPSC 通道
├── join_handle.rs      # JoinHandle<T>
├── net.rs              # 异步 TcpListener / TcpStream
└── multi_thread/
    ├── mod.rs          # 模块导出
    ├── runtime.rs      # MultiThreadRuntime（线程池、block_on）
    ├── executor.rs     # Worker 循环、driver 竞选、原子 waker
    ├── task.rs         # Task（UnsafeCell + AtomicU8 状态机）
    ├── timer.rs        # TimerWheel（inbox/heap 分离、AtomicWaker）
    └── net.rs          # 线程安全 TcpListener / TcpStream
```

## 核心设计决策

**为什么用 `UnsafeCell` 而不是 `Mutex` 保护 future？**

`Future::poll` 需要 `&mut` 访问。多线程中 Task 在 `Arc` 后面。
用 `Mutex` 每次 poll 都要走系统调用，改为原子状态机保证独占访问，
然后通过 `UnsafeCell` 零开销读写 future。

**为什么 TimerWheel 要拆成 inbox + heap？**

`register()`（任意线程）和 `process()`（driver）会争同一把锁。
把新 timer 推入 inbox（`Mutex<Vec>`），driver 处理时才 drain 到 heap，
两条路径永远不碰同一个数据结构。

**为什么需要 eventfd？**

没有 eventfd 时，driver 只能用短超时轮询（5ms）保持响应，引入延迟。
eventfd 让任意线程能即时打断 `epoll_wait`，driver 可以无限阻塞但仍在
微秒级响应新任务。

## 运行

```bash
cargo run          # 运行所有示例
cargo test         # 运行全部 20 个测试
cargo clippy       # lint 检查
```
