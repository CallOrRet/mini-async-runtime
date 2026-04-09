use mini_async_runtime::multi_thread::MultiThreadRuntime;

fn main() {
    println!("--- MT: Spawn & JoinHandle ---");
    let rt = MultiThreadRuntime::new(4).unwrap();

    let h1 = rt.spawn(async { 10 });
    let h2 = rt.spawn(async { 20 });
    let h3 = rt.spawn(async { 30 });

    let sum = rt.block_on(async { h1.await + h2.await + h3.await });
    println!("  sum = {sum}");
}
