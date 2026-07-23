//! A simple timer example.
//!
//! Prints "Hello, world!" after a 2-second timeout.

use crabuv::timers::TimerKind::Timeout;
use crabuv::EventLoop;
use crabuv::LoopHandle;
use crabuv::RunMode;
use std::time::Duration;

fn main() {
    let mut event_loop = EventLoop::default();
    let handle = event_loop.handle();

    handle.timer(Duration::from_secs(2), Timeout, |_: LoopHandle| {
        println!("Hello, world!");
    });

    event_loop.run(RunMode::Default);
}
