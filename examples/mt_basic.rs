use mini_async_runtime::multi_thread::MultiThreadRuntime;

fn main() {
    println!("--- MT: Basic block_on ---");
    let rt = MultiThreadRuntime::new(4).unwrap();
    let result = rt.block_on(async { 2 + 2 });
    println!("  2 + 2 = {result}");
}
