use mini_async_runtime::Runtime;

fn main() {
    println!("--- ST: Spawn & JoinHandle ---");
    let mut rt = Runtime::new().unwrap();

    let h1 = rt.spawn(async { 10 });
    let h2 = rt.spawn(async { 20 });
    let h3 = rt.spawn(async { 30 });

    let sum = rt.block_on(async { h1.await + h2.await + h3.await });
    println!("  sum = {sum}");
}
