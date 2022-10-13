extern crate dune_event_loop as ev_loop;

use ev_loop::EventLoop;
use ev_loop::LoopHandle;

fn main() {
    let mut event_loop = EventLoop::new();
    let handle = event_loop.handle();

    handle.timer(1000, false, |h: LoopHandle| {
        println!("Hello!");
        h.timer(2500, false, |_: LoopHandle| println!("Hello, world!"));
    });

    while event_loop.has_pending_events() {
        event_loop.tick();
    }
}
