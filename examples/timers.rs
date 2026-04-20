extern crate dune_event_loop;

use dune_event_loop::timers::TimerKind::Timeout;
use dune_event_loop::EventLoop;
use dune_event_loop::LoopHandle;
use dune_event_loop::RunMode;
use std::time::Duration;

fn main() {
    let mut event_loop = EventLoop::default();
    let handle = event_loop.handle();

    handle.timer(Duration::from_secs(2), Timeout, |_: LoopHandle| {
        println!("Hello, world!");
    });

    event_loop.run(RunMode::Default);
}
