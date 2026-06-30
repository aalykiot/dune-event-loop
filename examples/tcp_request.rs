extern crate crabuv;

use anyhow::Result;
use crabuv::tcp_stream::TcpStreamHandle;
use crabuv::EventLoop;
use crabuv::LoopHandle;
use crabuv::RunMode;

fn main() {
    let address = "188.184.67.127:80".parse().unwrap();
    let mut event_loop = EventLoop::default();
    let handle = event_loop.handle();

    let on_write = |_: TcpStreamHandle, _: Result<usize>| {};
    let on_close = |_: LoopHandle| println!("Connection closed.");

    let on_read = move |stream: TcpStreamHandle, data: Result<Vec<u8>>| {
        match data {
            Ok(data) if data.is_empty() => stream.close(on_close),
            Ok(data) => println!("{}", String::from_utf8(data).unwrap()),
            Err(err) => println!("ERROR: {}", err),
        };
    };

    const HTTP_REQUEST: &[u8] =
        b"GET / HTTP/1.1\r\nHost: info.cern.ch\r\nConnection: close\r\n\r\n";

    let on_connection = move |stream: Result<TcpStreamHandle>| match stream {
        Err(e) => eprintln!("{}", e),
        Ok(stream) => {
            stream.set_read_callback(on_read);
            stream.write(HTTP_REQUEST.to_vec(), on_write);
        }
    };

    handle.tcp_connect(address, on_connection).unwrap();
    event_loop.run(RunMode::Default);
}
