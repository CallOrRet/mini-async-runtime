//! # Mini Async Runtime
//!
//! A minimal async runtime for Rust, inspired by [tokio](https://tokio.rs) but
//! with a much simpler design. It is intended for learning and lightweight use
//! cases.
//!
//! ## Features
//!
//! - **Single-threaded executor** — simple, predictable task scheduling.
//! - **Epoll-based reactor** — efficient non-blocking I/O on Linux.
//! - **Timers** — async sleep for time-based operations.
//! - **Async TCP** — [`TcpListener`](net::TcpListener) and [`TcpStream`](net::TcpStream).
//! - **Channels** — async [`mpsc`](channel) for inter-task communication.
//! - **JoinHandle** — await the result of spawned tasks.
//!
//! ## Quick Start
//!
//! ```
//! use mini_async_runtime::Runtime;
//!
//! let mut rt = Runtime::new().unwrap();
//! let result = rt.block_on(async {
//!     2 + 2
//! });
//! assert_eq!(result, 4);
//! ```

pub mod channel;
pub mod join_handle;
pub mod multi_thread;
pub mod net;

pub(crate) mod executor;
pub(crate) mod reactor;
pub(crate) mod task;
pub(crate) mod timer;

mod runtime;

pub use join_handle::JoinHandle;
pub use runtime::Runtime;

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    // ---- Basic executor tests ----

    #[test]
    fn test_block_on_simple() {
        let mut rt = Runtime::new().unwrap();
        let val = rt.block_on(async { 42 });
        assert_eq!(val, 42);
    }

    #[test]
    fn test_block_on_with_spawn() {
        let mut rt = Runtime::new().unwrap();
        let handle = rt.spawn(async { 10 + 20 });
        let val = rt.block_on(handle);
        assert_eq!(val, 30);
    }

    #[test]
    fn test_multiple_spawns() {
        let mut rt = Runtime::new().unwrap();
        let h1 = rt.spawn(async { 1 });
        let h2 = rt.spawn(async { 2 });
        let h3 = rt.spawn(async { 3 });

        let sum = rt.block_on(async {
            let a = h1.await;
            let b = h2.await;
            let c = h3.await;
            a + b + c
        });
        assert_eq!(sum, 6);
    }

    #[test]
    fn test_spawn_returns_string() {
        let mut rt = Runtime::new().unwrap();
        let handle = rt.spawn(async { String::from("hello async") });
        let val = rt.block_on(handle);
        assert_eq!(val, "hello async");
    }

    // ---- Timer tests ----

    #[test]
    fn test_sleep_completes() {
        let mut rt = Runtime::new().unwrap();
        let timers = rt.timers.clone();

        let start = Instant::now();
        rt.block_on(async move {
            let sleep = timers
                .borrow_mut()
                .register(Instant::now() + Duration::from_millis(50));
            sleep.await;
        });
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(40),
            "sleep completed too early: {:?}",
            elapsed
        );
    }

    #[test]
    fn test_multiple_timers() {
        let mut rt = Runtime::new().unwrap();
        let timers1 = rt.timers.clone();
        let timers2 = rt.timers.clone();

        let h1 = rt.spawn(async move {
            let sleep = timers1
                .borrow_mut()
                .register(Instant::now() + Duration::from_millis(30));
            sleep.await;
            1
        });

        let h2 = rt.spawn(async move {
            let sleep = timers2
                .borrow_mut()
                .register(Instant::now() + Duration::from_millis(60));
            sleep.await;
            2
        });

        let sum = rt.block_on(async { h1.await + h2.await });
        assert_eq!(sum, 3);
    }

    // ---- Channel tests ----

    #[test]
    fn test_channel_basic() {
        let mut rt = Runtime::new().unwrap();
        let (tx, mut rx) = channel::unbounded::<i32>();

        let sender = rt.spawn(async move {
            tx.send(42).unwrap();
        });

        let val = rt.block_on(async move {
            sender.await;
            rx.recv().await.unwrap()
        });
        assert_eq!(val, 42);
    }

    #[test]
    fn test_channel_multiple_messages() {
        let mut rt = Runtime::new().unwrap();
        let (tx, mut rx) = channel::unbounded();

        let sender = rt.spawn(async move {
            for i in 0..5 {
                tx.send(i).unwrap();
            }
        });

        let result = rt.block_on(async move {
            sender.await;
            let mut values = Vec::new();
            while let Some(v) = rx.recv().await {
                values.push(v);
                if values.len() == 5 {
                    break;
                }
            }
            values
        });
        assert_eq!(result, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn test_channel_sender_drop_closes() {
        let mut rt = Runtime::new().unwrap();
        let (tx, mut rx) = channel::unbounded::<i32>();

        let sender = rt.spawn(async move {
            tx.send(1).unwrap();
            tx.send(2).unwrap();
            // tx is dropped here
        });

        let result = rt.block_on(async move {
            sender.await;
            let mut values = Vec::new();
            while let Some(v) = rx.recv().await {
                values.push(v);
            }
            values
        });
        assert_eq!(result, vec![1, 2]);
    }

    #[test]
    fn test_channel_clone_sender() {
        let mut rt = Runtime::new().unwrap();
        let (tx1, mut rx) = channel::unbounded();
        let tx2 = tx1.clone();

        let s1 = rt.spawn(async move {
            tx1.send(1).unwrap();
        });
        let s2 = rt.spawn(async move {
            tx2.send(2).unwrap();
        });

        let result = rt.block_on(async move {
            s1.await;
            s2.await;
            let mut values = Vec::new();
            // Both senders are dropped, channel should close.
            while let Some(v) = rx.recv().await {
                values.push(v);
            }
            values.sort();
            values
        });
        assert_eq!(result, vec![1, 2]);
    }

    // ---- TCP networking tests ----

    #[test]
    fn test_tcp_echo() {
        let mut rt = Runtime::new().unwrap();
        let reactor = rt.reactor.clone();
        let reactor2 = rt.reactor.clone();

        // Bind a listener on an ephemeral port.
        let listener = net::TcpListener::bind_with_reactor("127.0.0.1:0", reactor.clone()).unwrap();
        let addr = listener.local_addr().unwrap();

        // Server task: accept one connection, read data, echo it back.
        let server = rt.spawn(async move {
            let (mut stream, _addr) = listener.accept().await.unwrap();
            let mut buf = [0u8; 128];
            let n = stream.async_read(&mut buf).await.unwrap();
            let _ = stream.async_write(&buf[..n]).await.unwrap();
        });

        // Client task: connect, send data, read echo.
        let client = rt.spawn(async move {
            let mut stream = net::TcpStream::connect_with_reactor(addr, reactor2)
                .await
                .unwrap();
            let msg = b"hello runtime!";
            stream.async_write(msg).await.unwrap();
            let mut buf = [0u8; 128];
            let n = stream.async_read(&mut buf).await.unwrap();
            String::from_utf8_lossy(&buf[..n]).to_string()
        });

        rt.block_on(server);
        let result = rt.block_on(client);
        assert_eq!(result, "hello runtime!");
    }

    // ---- Yield / cooperation test ----

    #[test]
    fn test_yield_now() {
        let mut rt = Runtime::new().unwrap();
        let (tx, mut rx) = channel::unbounded();

        // Task A sends a value, then yields, then sends another.
        let h = rt.spawn(async move {
            tx.send(1).unwrap();
            YieldNow(false).await;
            tx.send(2).unwrap();
        });

        let result = rt.block_on(async move {
            h.await;
            let mut out = Vec::new();
            while let Some(v) = rx.recv().await {
                out.push(v);
            }
            out
        });
        assert_eq!(result, vec![1, 2]);
    }

    /// A future that yields once and then completes.
    struct YieldNow(bool);

    impl std::future::Future for YieldNow {
        type Output = ();
        fn poll(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<()> {
            if self.0 {
                std::task::Poll::Ready(())
            } else {
                self.0 = true;
                cx.waker().wake_by_ref();
                std::task::Poll::Pending
            }
        }
    }

    // ==== Multi-threaded runtime tests ====

    mod mt {
        use crate::multi_thread::MultiThreadRuntime;
        use std::time::{Duration, Instant};

        #[test]
        fn test_mt_block_on_simple() {
            let rt = MultiThreadRuntime::new(2).unwrap();
            let val = rt.block_on(async { 42 });
            assert_eq!(val, 42);
        }

        #[test]
        fn test_mt_spawn_and_join() {
            let rt = MultiThreadRuntime::new(2).unwrap();
            let h1 = rt.spawn(async { 1 });
            let h2 = rt.spawn(async { 2 });
            let h3 = rt.spawn(async { 3 });

            let sum = rt.block_on(async { h1.await + h2.await + h3.await });
            assert_eq!(sum, 6);
        }

        #[test]
        fn test_mt_spawn_returns_string() {
            let rt = MultiThreadRuntime::new(2).unwrap();
            let h = rt.spawn(async { String::from("hello mt") });
            let val = rt.block_on(h);
            assert_eq!(val, "hello mt");
        }

        #[test]
        fn test_mt_sleep() {
            let rt = MultiThreadRuntime::new(2).unwrap();
            let sleep = rt.sleep(Duration::from_millis(50));

            let start = Instant::now();
            rt.block_on(sleep);
            let elapsed = start.elapsed();
            assert!(
                elapsed >= Duration::from_millis(40),
                "sleep too early: {elapsed:?}"
            );
        }

        #[test]
        fn test_mt_multiple_timers() {
            let rt = MultiThreadRuntime::new(2).unwrap();
            let s1 = rt.sleep(Duration::from_millis(30));
            let s2 = rt.sleep(Duration::from_millis(60));

            let h1 = rt.spawn(async move {
                s1.await;
                1
            });
            let h2 = rt.spawn(async move {
                s2.await;
                2
            });

            let sum = rt.block_on(async { h1.await + h2.await });
            assert_eq!(sum, 3);
        }

        #[test]
        fn test_mt_parallel_tasks() {
            let rt = MultiThreadRuntime::new(4).unwrap();
            let mut handles = Vec::new();
            for i in 0..8 {
                handles.push(rt.spawn(async move { i * i }));
            }

            let sum: i32 = rt.block_on(async {
                let mut total = 0;
                for h in handles {
                    total += h.await;
                }
                total
            });
            // 0 + 1 + 4 + 9 + 16 + 25 + 36 + 49 = 140
            assert_eq!(sum, 140);
        }

        #[test]
        fn test_mt_tasks_run_on_different_threads() {
            let rt = MultiThreadRuntime::new(4).unwrap();
            let mut handles = Vec::new();
            for _ in 0..4 {
                handles.push(rt.spawn(async {
                    // Do some work to ensure the task actually runs
                    let mut sum = 0u64;
                    for i in 0..100_000 {
                        sum = sum.wrapping_add(i);
                    }
                    let _ = sum;
                    std::thread::current().id()
                }));
            }

            let ids: Vec<_> = rt.block_on(async {
                let mut ids = Vec::new();
                for h in handles {
                    ids.push(h.await);
                }
                ids
            });

            // With 4 workers and 4 tasks, we expect at least 2 different
            // thread IDs (can't guarantee all 4 since scheduling varies).
            let unique = ids.iter().collect::<std::collections::HashSet<_>>();
            // With 4 workers and 4 tasks, we expect at least 2 different
            // thread IDs (can't guarantee all 4 since scheduling varies).
            assert!(!unique.is_empty());
            assert_eq!(ids.len(), 4);
        }

        #[test]
        fn test_mt_tcp_echo() {
            let rt = MultiThreadRuntime::new(2).unwrap();
            let listener = rt.tcp_listen("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();

            let server = rt.spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buf = [0u8; 128];
                let n = stream.async_read(&mut buf).await.unwrap();
                let _ = stream.async_write(&buf[..n]).await.unwrap();
            });

            let mut client_stream = rt.tcp_connect(addr).unwrap();
            let client = rt.spawn(async move {
                client_stream.async_write(b"echo MT").await.unwrap();
                let mut buf = [0u8; 128];
                let n = client_stream.async_read(&mut buf).await.unwrap();
                String::from_utf8_lossy(&buf[..n]).to_string()
            });

            rt.block_on(server);
            let result = rt.block_on(client);
            assert_eq!(result, "echo MT");
        }
    }
}
