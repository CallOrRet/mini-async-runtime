use mini_async_runtime::Runtime;
use std::time::{Duration, Instant};

fn main() {
    println!("--- ST: Sleep ---");
    let mut rt = Runtime::new().unwrap();
    let sleep_future = rt.sleep(Duration::from_millis(100));

    let start = Instant::now();
    rt.block_on(sleep_future);
    println!("  slept for {:?}", start.elapsed());
}
