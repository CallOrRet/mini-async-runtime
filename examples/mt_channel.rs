use mini_async_runtime::multi_thread::MultiThreadRuntime;

fn main() {
    println!("--- MT: Channel ---");
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
        let mut count = 0;
        while count < 6 {
            if let Ok(msg) = rx.try_recv() {
                println!("  received: {msg}");
                count += 1;
            } else {
                std::thread::yield_now();
            }
        }
        println!("  all messages received");
    });
}
