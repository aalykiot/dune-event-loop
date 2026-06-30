extern crate crabuv;

use crabuv::task::Output;
use crabuv::EventLoop;
use crabuv::LoopHandle;
use crabuv::RunMode;
use std::fs;

fn main() {
    let mut event_loop = EventLoop::default();
    let handle = event_loop.handle();

    let read_file = || {
        let content = fs::read_to_string("./examples/async.rs").unwrap();
        Some(Ok(content.as_bytes().to_vec()))
    };

    let read_file_cb = |_: LoopHandle, output: Output| {
        let bytes = output.unwrap().unwrap();
        let content = std::str::from_utf8(&bytes).unwrap();
        println!("{}", content);
    };

    handle.spawn_with_callback(read_file, read_file_cb);

    event_loop.run(RunMode::Default);
}
