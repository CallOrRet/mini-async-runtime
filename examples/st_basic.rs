use mini_async_runtime::Runtime;

fn main() {
    println!("--- ST: Basic block_on ---");
    let mut rt = Runtime::new().unwrap();
    let result = rt.block_on(async { 2 + 2 });
    println!("  2 + 2 = {result}");
}
