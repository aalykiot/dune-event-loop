extern crate dune_event_loop as ev_loop;

use ev_loop::EventLoop;
use ev_loop::LoopHandle;
use ev_loop::TaskResult;

fn main() {
    let mut event_loop = EventLoop::default();
    let handle = event_loop.handle();

    let read_file = || {
        let content = std::fs::read_to_string("./examples/async.rs").unwrap();
        Some(Ok(content.as_bytes().to_vec()))
    };

    let read_file_cb = |_: LoopHandle, result: TaskResult| {
        let bytes = result.unwrap().unwrap();
        let content = std::str::from_utf8(&bytes).unwrap();
        println!("{}", content);
    };

    handle.spawn(read_file, Some(read_file_cb));

    while event_loop.has_pending_events() {
        event_loop.tick();
    }
}
