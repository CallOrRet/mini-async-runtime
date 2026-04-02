use mini_async_runtime::channel;
use mini_async_runtime::multi_thread::MultiThreadRuntime;
use mini_async_runtime::Runtime;
use std::time::{Duration, Instant};

fn main() {
    println!("========================================");
    println!("  Single-Threaded Runtime Examples");
    println!("========================================\n");

    st_basic();
    st_spawn_and_join();
    st_sleep();
    st_channel();
    st_tcp_echo();

    println!("\n========================================");
    println!("  Multi-Threaded Runtime Examples");
    println!("========================================\n");

    mt_basic();
    mt_spawn_and_join();
    mt_sleep();
    mt_channel();
    mt_tcp_echo();
    mt_parallel_compute();

    println!("\nAll examples completed!");
}

fn st_basic() {
    println!("--- ST 1. Basic block_on ---");
    let mut rt = Runtime::new().unwrap();
    let result = rt.block_on(async { 2 + 2 });
    println!("  2 + 2 = {result}");
}

fn st_spawn_and_join() {
    println!("\n--- ST 2. Spawn & JoinHandle ---");
    let mut rt = Runtime::new().unwrap();

    let h1 = rt.spawn(async { 10 });
    let h2 = rt.spawn(async { 20 });
    let h3 = rt.spawn(async { 30 });

    let sum = rt.block_on(async { h1.await + h2.await + h3.await });
    println!("  sum = {sum}");
}

fn st_sleep() {
    println!("\n--- ST 3. Sleep ---");
    let mut rt = Runtime::new().unwrap();
    let sleep_future = rt.sleep(Duration::from_millis(100));

    let start = Instant::now();
    rt.block_on(sleep_future);
    println!("  slept for {:?}", start.elapsed());
}

fn st_channel() {
    println!("\n--- ST 4. Channel ---");
    let mut rt = Runtime::new().unwrap();
    let (tx, mut rx) = channel::unbounded();
    let tx2 = tx.clone();

    rt.spawn(async move {
        for i in 0..3 {
            tx.send(format!("producer-1: msg {i}")).unwrap();
        }
    });
    rt.spawn(async move {
        for i in 0..3 {
            tx2.send(format!("producer-2: msg {i}")).unwrap();
        }
    });
    rt.block_on(async move {
        while let Some(msg) = rx.recv().await {
            println!("  received: {msg}");
        }
        println!("  channel closed");
    });
}

fn st_tcp_echo() {
    println!("\n--- ST 5. TCP Echo ---");
    let mut rt = Runtime::new().unwrap();

    let listener = rt.tcp_listen("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    println!("  server listening on {addr}");

    let server = rt.spawn(async move {
        let (mut stream, peer) = listener.accept().await.unwrap();
        println!("  server: accepted from {peer}");
        let mut buf = [0u8; 1024];
        let n = stream.async_read(&mut buf).await.unwrap();
        let _ = stream.async_write(&buf[..n]).await.unwrap();
    });

    let connect_fut = rt.tcp_connect(addr);
    let client = rt.spawn(async move {
        let mut stream = connect_fut.await.unwrap();
        stream.async_write(b"hello ST!").await.unwrap();
        let mut buf = [0u8; 1024];
        let n = stream.async_read(&mut buf).await.unwrap();
        println!("  client echo: \"{}\"", String::from_utf8_lossy(&buf[..n]));
    });

    rt.block_on(async {
        server.await;
        client.await;
    });
}

// =========================================================================
// Multi-threaded examples
// =========================================================================

fn mt_basic() {
    println!("--- MT 1. Basic block_on ---");
    let rt = MultiThreadRuntime::new(4).unwrap();
    let result = rt.block_on(async { 2 + 2 });
    println!("  2 + 2 = {result}");
}

fn mt_spawn_and_join() {
    println!("\n--- MT 2. Spawn & JoinHandle ---");
    let rt = MultiThreadRuntime::new(4).unwrap();

    let h1 = rt.spawn(async { 10 });
    let h2 = rt.spawn(async { 20 });
    let h3 = rt.spawn(async { 30 });

    let sum = rt.block_on(async { h1.await + h2.await + h3.await });
    println!("  sum = {sum}");
}

fn mt_sleep() {
    println!("\n--- MT 3. Sleep ---");
    let rt = MultiThreadRuntime::new(4).unwrap();
    let sleep_future = rt.sleep(Duration::from_millis(100));

    let start = Instant::now();
    rt.block_on(sleep_future);
    println!("  slept for {:?}", start.elapsed());
}

fn mt_channel() {
    println!("\n--- MT 4. Channel ---");
    let rt = MultiThreadRuntime::new(4).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let tx2 = tx.clone();

    rt.spawn(async move {
        for i in 0..3 {
            tx.send(format!("producer-1: msg {i}")).unwrap();
        }
    });
    rt.spawn(async move {
        for i in 0..3 {
            tx2.send(format!("producer-2: msg {i}")).unwrap();
        }
    });

    rt.block_on(async move {
        // Collect all 6 messages. Use std::sync::mpsc since our async channel
        // uses Rc internally and is not Send.
        let mut count = 0;
        while count < 6 {
            if let Ok(msg) = rx.try_recv() {
                println!("  received: {msg}");
                count += 1;
            } else {
                // Yield to let producers run.
                std::thread::yield_now();
            }
        }
        println!("  all messages received");
    });
}

fn mt_tcp_echo() {
    println!("\n--- MT 5. TCP Echo ---");
    let rt = MultiThreadRuntime::new(4).unwrap();

    let listener = rt.tcp_listen("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    println!("  server listening on {addr}");

    let server = rt.spawn(async move {
        let (mut stream, peer) = listener.accept().await.unwrap();
        println!("  server: accepted from {peer}");
        let mut buf = [0u8; 1024];
        let n = stream.async_read(&mut buf).await.unwrap();
        let _ = stream.async_write(&buf[..n]).await.unwrap();
    });

    let mut client_stream = rt.tcp_connect(addr).unwrap();
    let client = rt.spawn(async move {
        client_stream.async_write(b"hello MT!").await.unwrap();
        let mut buf = [0u8; 1024];
        let n = client_stream.async_read(&mut buf).await.unwrap();
        println!("  client echo: \"{}\"", String::from_utf8_lossy(&buf[..n]));
    });

    rt.block_on(async {
        server.await;
        client.await;
    });
}

/// Demonstrates true parallelism: spawn CPU work on multiple threads.
fn mt_parallel_compute() {
    println!("\n--- MT 6. Parallel Computation ---");
    let rt = MultiThreadRuntime::new(4).unwrap();

    let start = Instant::now();

    // Spawn 4 tasks that each do some "heavy" work.
    let mut handles = Vec::new();
    for i in 0..4 {
        let h = rt.spawn(async move {
            let mut sum: u64 = 0;
            for j in 0..1_000_000u64 {
                sum = sum.wrapping_add(j.wrapping_mul(i as u64 + 1));
            }
            (
                i,
                sum,
                std::thread::current().name().unwrap_or("?").to_string(),
            )
        });
        handles.push(h);
    }

    rt.block_on(async {
        for h in handles {
            let (task_id, result, thread_name) = h.await;
            println!("  task {task_id}: result={result}, ran on [{thread_name}]");
        }
    });

    println!("  completed in {:?}", start.elapsed());
}
