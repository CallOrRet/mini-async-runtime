use mini_async_runtime::Runtime;

fn main() {
    println!("--- ST: TCP Echo ---");
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
