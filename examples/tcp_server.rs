extern crate dune_event_loop;

use anyhow::Result;
use dune_event_loop::tcp_listener::TcpListenerHandle;
use dune_event_loop::tcp_stream::TcpStreamHandle;
use dune_event_loop::EventLoop;
use dune_event_loop::LoopHandle;
use dune_event_loop::RunMode;

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
        Ok(stream) => stream.set_read_callback(on_read),
        Err(e) => eprintln!("{}", e),
    };

    match handle.tcp_listen(address, on_connection) {
        Ok(_) => println!("Server is listening on {address}"),
        Err(e) => eprintln!("{}", e),
    };

    event_loop.run(RunMode::Default);
}
