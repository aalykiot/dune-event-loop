//! A simple TCP echo server example.
//!
//! Listens on port 3000 and echoes back any data received from connected
//! clients.

use anyhow::Result;
use crabuv::tcp_listener::TcpListenerHandle;
use crabuv::tcp_stream::TcpStreamHandle;
use crabuv::EventLoop;
use crabuv::LoopHandle;
use crabuv::RunMode;

fn main() {
    let address = "0.0.0.0:3000".parse().unwrap();
    let mut event_loop = EventLoop::default();
    let handle = event_loop.handle();

    let on_close = |_: LoopHandle| println!("Connection closed.");
    let on_write = |_: TcpStreamHandle, result: Result<usize>| {
        if let Err(e) = result {
            eprintln!("{}", e);
        }
    };

    let on_read = move |stream: TcpStreamHandle, data: Result<Vec<u8>>| {
        match data {
            Ok(data) if data.is_empty() => stream.close(on_close),
            Ok(data) => stream.write(data, on_write),
            Err(e) => eprintln!("{}", e),
        };
    };

    let on_connection = move |_: TcpListenerHandle, stream: Result<TcpStreamHandle>| match stream {
        Ok(stream) => stream.start_reading(on_read),
        Err(e) => eprintln!("{}", e),
    };

    match handle.tcp_listen(address, on_connection) {
        Ok(_) => println!("Server is listening on {address}"),
        Err(e) => eprintln!("{}", e),
    };

    event_loop.run(RunMode::Default);
}
