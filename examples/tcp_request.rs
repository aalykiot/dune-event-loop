extern crate dune_event_loop as ev_loop;

use anyhow::Result;
use ev_loop::EventLoop;
use ev_loop::Index;
use ev_loop::LoopHandle;
use ev_loop::TcpSocketInfo;

fn main() {
    let mut event_loop = EventLoop::default();
    let handle = event_loop.handle();

    let on_close = |_: LoopHandle| println!("Connection closed.");

    let on_read = move |h: LoopHandle, index: Index, data: Result<Vec<u8>>| {
        match data {
            Ok(data) if data.is_empty() => h.tcp_close(index, on_close),
            Ok(data) => println!("{}", String::from_utf8(data).unwrap()),
            Err(err) => println!("ERROR: {}", err),
        };
    };

    let on_write = |_: LoopHandle, _: Index, _: Result<usize>| {};

    const HTTP_REQUEST: &str =
        "GET / HTTP/1.1\r\nHost: rssweather.com\r\nConnection: close\r\n\r\n";

    let on_connection =
        move |h: LoopHandle, index: Index, socket: Result<TcpSocketInfo>| match socket {
            Ok(_) => {
                h.tcp_read_start(index, on_read);
                h.tcp_write(index, HTTP_REQUEST.as_bytes(), on_write);
            }
            Err(e) => {
                eprintln!("{}", e);
                h.tcp_close(index, |_: LoopHandle| {});
            }
        };

    handle
        .tcp_connect("104.21.45.178:80", on_connection)
        .unwrap();

    while event_loop.has_pending_events() {
        event_loop.tick();
    }
}
