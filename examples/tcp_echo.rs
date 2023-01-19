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

    let on_write = |_: LoopHandle, _: Index, result: Result<usize>| {
        if let Err(e) = result {
            eprintln!("{}", e);
        }
    };

    let on_read = move |h: LoopHandle, index: Index, data: Result<Vec<u8>>| {
        match data {
            Ok(data) if data.is_empty() => h.tcp_close(index, on_close),
            Ok(data) => h.tcp_write(index, &data, on_write),
            Err(e) => eprintln!("{}", e),
        };
    };

    let on_new_connection =
        move |h: LoopHandle, index: Index, socket: Result<TcpSocketInfo>| match socket {
            Ok(_) => h.tcp_read_start(index, on_read),
            Err(e) => eprintln!("{}", e),
        };

    match handle.tcp_listen("127.0.0.1:9000", on_new_connection) {
        Ok(_) => println!("Server is listening on 127.0.0.1:9000"),
        Err(e) => eprintln!("{}", e),
    };

    while event_loop.has_pending_events() {
        event_loop.tick();
    }
}
