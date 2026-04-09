use mini_async_runtime::multi_thread::MultiThreadRuntime;
use std::time::Instant;

/// Demonstrates true parallelism: spawn CPU work on multiple threads.
fn main() {
    println!("--- MT: Parallel Computation ---");
    let rt = MultiThreadRuntime::new(4).unwrap();

    let start = Instant::now();

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
