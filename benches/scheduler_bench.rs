//! Criterion benchmarks for single-threaded and multi-threaded scheduler
//! performance.
//!
//! Run with: `cargo bench --bench scheduler_bench`
//!
//! Each benchmark measures a specific aspect of the scheduler:
//! - spawn overhead
//! - poll / wake / re-schedule round-trip
//! - context-switch cost (yield-based)
//! - JoinHandle await latency
//! - channel scheduling interaction

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use mini_async_runtime::multi_thread::MultiThreadRuntime;
use mini_async_runtime::Runtime;

// ---------------------------------------------------------------------------
// Helper futures
// ---------------------------------------------------------------------------

/// A future that yields `n` times before completing. Each yield calls
/// `wake_by_ref` so the scheduler must re-enqueue the task.
struct YieldN(usize);

impl Future for YieldN {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.0 == 0 {
            Poll::Ready(())
        } else {
            self.0 -= 1;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

// ---------------------------------------------------------------------------
// Single-threaded benchmarks
// ---------------------------------------------------------------------------

fn st_spawn_and_run(c: &mut Criterion) {
    let mut group = c.benchmark_group("st_spawn_and_run");

    for &count in &[1, 10, 100, 1_000, 10_000] {
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, &count| {
            b.iter(|| {
                let mut rt = Runtime::new().unwrap();
                let mut handles = Vec::with_capacity(count);
                for i in 0..count {
                    handles.push(rt.spawn(async move { black_box(i) }));
                }
                rt.block_on(async {
                    for h in handles {
                        black_box(h.await);
                    }
                });
            });
        });
    }
    group.finish();
}

fn st_yield_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("st_yield_throughput");

    // Measure how fast the scheduler can re-enqueue and re-poll a task.
    for &yields_per_task in &[10, 100, 1_000] {
        group.throughput(Throughput::Elements(yields_per_task as u64));
        group.bench_with_input(
            BenchmarkId::new("single_task", yields_per_task),
            &yields_per_task,
            |b, &n| {
                b.iter(|| {
                    let mut rt = Runtime::new().unwrap();
                    rt.block_on(YieldN(n));
                });
            },
        );
    }

    // Multiple tasks yielding concurrently — tests scheduler fairness overhead.
    for &num_tasks in &[10, 100] {
        let total_yields = num_tasks * 100;
        group.throughput(Throughput::Elements(total_yields as u64));
        group.bench_with_input(
            BenchmarkId::new("multi_task_100y", num_tasks),
            &num_tasks,
            |b, &n| {
                b.iter(|| {
                    let mut rt = Runtime::new().unwrap();
                    let mut handles = Vec::with_capacity(n);
                    for _ in 0..n {
                        handles.push(rt.spawn(YieldN(100)));
                    }
                    rt.block_on(async {
                        for h in handles {
                            h.await;
                        }
                    });
                });
            },
        );
    }
    group.finish();
}

fn st_block_on_overhead(c: &mut Criterion) {
    c.bench_function("st_block_on_noop", |b| {
        b.iter(|| {
            let mut rt = Runtime::new().unwrap();
            black_box(rt.block_on(async { 42 }));
        });
    });
}

fn st_channel_scheduling(c: &mut Criterion) {
    let mut group = c.benchmark_group("st_channel_scheduling");

    for &msg_count in &[100, 1_000, 10_000] {
        group.throughput(Throughput::Elements(msg_count as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(msg_count),
            &msg_count,
            |b, &count| {
                b.iter(|| {
                    let mut rt = Runtime::new().unwrap();
                    let (tx, mut rx) = mini_async_runtime::channel::unbounded();

                    let sender = rt.spawn(async move {
                        for i in 0..count {
                            tx.send(i).unwrap();
                        }
                    });

                    rt.block_on(async move {
                        sender.await;
                        let mut n = 0usize;
                        while let Some(v) = rx.recv().await {
                            black_box(v);
                            n += 1;
                            if n == count {
                                break;
                            }
                        }
                    });
                });
            },
        );
    }
    group.finish();
}

fn st_waker_wake_cost(c: &mut Criterion) {
    // Measure the cost of waker clone + wake_by_ref (the ReadyQueue enqueue
    // path) by spawning one task that yields many times.
    c.bench_function("st_waker_10000_wakes", |b| {
        b.iter(|| {
            let mut rt = Runtime::new().unwrap();
            rt.block_on(YieldN(10_000));
        });
    });
}

// ---------------------------------------------------------------------------
// Multi-threaded benchmarks
// ---------------------------------------------------------------------------

fn mt_spawn_and_run(c: &mut Criterion) {
    let mut group = c.benchmark_group("mt_spawn_and_run");

    for &count in &[1, 10, 100, 1_000, 10_000] {
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, &count| {
            b.iter(|| {
                let rt = MultiThreadRuntime::new(4).unwrap();
                let mut handles = Vec::with_capacity(count);
                for i in 0..count {
                    handles.push(rt.spawn(async move { black_box(i) }));
                }
                rt.block_on(async {
                    for h in handles {
                        black_box(h.await);
                    }
                });
            });
        });
    }
    group.finish();
}

fn mt_yield_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("mt_yield_throughput");

    for &yields_per_task in &[10, 100, 1_000] {
        group.throughput(Throughput::Elements(yields_per_task as u64));
        group.bench_with_input(
            BenchmarkId::new("single_task", yields_per_task),
            &yields_per_task,
            |b, &n| {
                b.iter(|| {
                    let rt = MultiThreadRuntime::new(4).unwrap();
                    let h = rt.spawn(YieldN(n));
                    rt.block_on(h);
                });
            },
        );
    }

    for &num_tasks in &[10, 100] {
        let total_yields = num_tasks * 100;
        group.throughput(Throughput::Elements(total_yields as u64));
        group.bench_with_input(
            BenchmarkId::new("multi_task_100y", num_tasks),
            &num_tasks,
            |b, &n| {
                b.iter(|| {
                    let rt = MultiThreadRuntime::new(4).unwrap();
                    let mut handles = Vec::with_capacity(n);
                    for _ in 0..n {
                        handles.push(rt.spawn(YieldN(100)));
                    }
                    rt.block_on(async {
                        for h in handles {
                            h.await;
                        }
                    });
                });
            },
        );
    }
    group.finish();
}

fn mt_block_on_overhead(c: &mut Criterion) {
    c.bench_function("mt_block_on_noop", |b| {
        b.iter(|| {
            let rt = MultiThreadRuntime::new(4).unwrap();
            black_box(rt.block_on(async { 42 }));
        });
    });
}

fn mt_cross_thread_wake(c: &mut Criterion) {
    let mut group = c.benchmark_group("mt_cross_thread_wake");

    // Ping-pong: two tasks wake each other through a shared counter.
    // This exercises cross-thread waker delivery.
    for &rounds in &[100, 1_000] {
        group.throughput(Throughput::Elements(rounds as u64 * 2));
        group.bench_with_input(
            BenchmarkId::from_parameter(rounds),
            &rounds,
            |b, &rounds| {
                b.iter(|| {
                    let rt = MultiThreadRuntime::new(4).unwrap();

                    use std::sync::atomic::{AtomicUsize, Ordering};
                    use std::sync::Arc;
                    let counter = Arc::new(AtomicUsize::new(0));
                    let target = rounds * 2;

                    // Task A increments on even values.
                    let c1 = counter.clone();
                    let a = rt.spawn(async move {
                        loop {
                            let v = c1.load(Ordering::Acquire);
                            if v >= target {
                                return;
                            }
                            if v.is_multiple_of(2) {
                                c1.fetch_add(1, Ordering::Release);
                            }
                            // Yield to let the scheduler run task B.
                            YieldN(0).await;
                        }
                    });

                    // Task B increments on odd values.
                    let c2 = counter.clone();
                    let b_handle = rt.spawn(async move {
                        loop {
                            let v = c2.load(Ordering::Acquire);
                            if v >= target {
                                return;
                            }
                            if v % 2 == 1 {
                                c2.fetch_add(1, Ordering::Release);
                            }
                            YieldN(0).await;
                        }
                    });

                    rt.block_on(a);
                    rt.block_on(b_handle);
                    assert_eq!(counter.load(Ordering::Relaxed), target);
                });
            },
        );
    }
    group.finish();
}

fn mt_contended_spawn(c: &mut Criterion) {
    let mut group = c.benchmark_group("mt_contended_spawn");

    // Many tasks are spawned all at once, stressing the shared ready queue
    // under contention from 4 workers.
    for &count in &[100, 1_000, 10_000] {
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::from_parameter(count), &count, |b, &count| {
            b.iter(|| {
                let rt = MultiThreadRuntime::new(4).unwrap();
                let mut handles = Vec::with_capacity(count);
                for _ in 0..count {
                    handles.push(rt.spawn(async { black_box(1u64) }));
                }
                let sum: u64 = rt.block_on(async {
                    let mut s = 0u64;
                    for h in handles {
                        s += h.await;
                    }
                    s
                });
                assert_eq!(sum, count as u64);
            });
        });
    }
    group.finish();
}

fn mt_worker_scalability(c: &mut Criterion) {
    let mut group = c.benchmark_group("mt_worker_scalability");
    group.measurement_time(Duration::from_secs(5));

    let task_count = 1_000;
    group.throughput(Throughput::Elements(task_count as u64));

    // Same workload on 1, 2, 4 workers — shows scaling.
    for &workers in &[1, 2, 4] {
        group.bench_with_input(BenchmarkId::new("workers", workers), &workers, |b, &w| {
            b.iter(|| {
                let rt = MultiThreadRuntime::new(w).unwrap();
                let mut handles = Vec::with_capacity(task_count);
                for _ in 0..task_count {
                    handles.push(rt.spawn(async {
                        // Light compute to avoid being purely scheduler-bound.
                        let mut s = 0u64;
                        for i in 0..1_000u64 {
                            s = s.wrapping_add(i);
                        }
                        black_box(s)
                    }));
                }
                rt.block_on(async {
                    for h in handles {
                        black_box(h.await);
                    }
                });
            });
        });
    }
    group.finish();
}

// ---------------------------------------------------------------------------
// Comparison: ST vs MT for the same workload
// ---------------------------------------------------------------------------

fn st_vs_mt_spawn_1000(c: &mut Criterion) {
    let mut group = c.benchmark_group("st_vs_mt_spawn_1000");
    let count = 1_000usize;
    group.throughput(Throughput::Elements(count as u64));

    group.bench_function("single_thread", |b| {
        b.iter(|| {
            let mut rt = Runtime::new().unwrap();
            let mut handles = Vec::with_capacity(count);
            for i in 0..count {
                handles.push(rt.spawn(async move { black_box(i) }));
            }
            rt.block_on(async {
                for h in handles {
                    black_box(h.await);
                }
            });
        });
    });

    group.bench_function("multi_thread_4w", |b| {
        b.iter(|| {
            let rt = MultiThreadRuntime::new(4).unwrap();
            let mut handles = Vec::with_capacity(count);
            for i in 0..count {
                handles.push(rt.spawn(async move { black_box(i) }));
            }
            rt.block_on(async {
                for h in handles {
                    black_box(h.await);
                }
            });
        });
    });

    group.finish();
}

fn st_vs_mt_yield_storm(c: &mut Criterion) {
    let mut group = c.benchmark_group("st_vs_mt_yield_storm");
    let num_tasks = 100;
    let yields_each = 100;
    group.throughput(Throughput::Elements((num_tasks * yields_each) as u64));

    group.bench_function("single_thread", |b| {
        b.iter(|| {
            let mut rt = Runtime::new().unwrap();
            let mut handles = Vec::with_capacity(num_tasks);
            for _ in 0..num_tasks {
                handles.push(rt.spawn(YieldN(yields_each)));
            }
            rt.block_on(async {
                for h in handles {
                    h.await;
                }
            });
        });
    });

    group.bench_function("multi_thread_4w", |b| {
        b.iter(|| {
            let rt = MultiThreadRuntime::new(4).unwrap();
            let mut handles = Vec::with_capacity(num_tasks);
            for _ in 0..num_tasks {
                handles.push(rt.spawn(YieldN(yields_each)));
            }
            rt.block_on(async {
                for h in handles {
                    h.await;
                }
            });
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Register all groups
// ---------------------------------------------------------------------------

criterion_group!(
    st_benches,
    st_spawn_and_run,
    st_yield_throughput,
    st_block_on_overhead,
    st_channel_scheduling,
    st_waker_wake_cost,
);

criterion_group!(
    mt_benches,
    mt_spawn_and_run,
    mt_yield_throughput,
    mt_block_on_overhead,
    mt_cross_thread_wake,
    mt_contended_spawn,
    mt_worker_scalability,
);

criterion_group!(
    comparison_benches,
    st_vs_mt_spawn_1000,
    st_vs_mt_yield_storm,
);

criterion_main!(st_benches, mt_benches, comparison_benches);
