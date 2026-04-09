use mini_async_runtime::{channel, Runtime};

fn main() {
    println!("--- ST: Channel ---");
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
