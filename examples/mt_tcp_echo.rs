use mini_async_runtime::multi_thread::MultiThreadRuntime;

fn main() {
    println!("--- MT: TCP Echo ---");
    let rt = MultiThreadRuntime::new(4).unwrap();

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

    let mut client_stream = rt.tcp_connect(addr).unwrap();
    let client = rt.spawn(async move {
        client_stream.async_write(b"hello MT!").await.unwrap();
        let mut buf = [0u8; 1024];
        let n = client_stream.async_read(&mut buf).await.unwrap();
        println!("  client echo: \"{}\"", String::from_utf8_lossy(&buf[..n]));
    });

    rt.block_on(async {
        server.await;
        client.await;
    });
}
