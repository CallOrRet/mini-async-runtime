use mini_async_runtime::multi_thread::MultiThreadRuntime;
use std::time::{Duration, Instant};

fn main() {
    println!("--- MT: Sleep ---");
    let rt = MultiThreadRuntime::new(4).unwrap();
    let sleep_future = rt.sleep(Duration::from_millis(100));

    let start = Instant::now();
    rt.block_on(sleep_future);
    println!("  slept for {:?}", start.elapsed());
}
