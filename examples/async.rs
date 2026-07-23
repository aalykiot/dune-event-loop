//! An async task example.
//!
//! Spawns a file-read task on a background thread and prints the result when
//! it completes.

use anyhow::Result;
use crabuv::EventLoop;
use crabuv::LoopHandle;
use crabuv::RunMode;
use std::fs;

fn main() {
    let mut event_loop = EventLoop::default();
    let handle = event_loop.handle();

    let read_file = || -> Result<String> { Ok(fs::read_to_string("./examples/async.rs")?) };

    let read_file_cb = |_: LoopHandle, content: Result<String>| match content {
        Ok(text) => println!("{text}"),
        Err(e) => eprintln!("{}", e.to_string()),
    };

    handle.spawn(read_file, Some(read_file_cb));

    event_loop.run(RunMode::Default);
}
