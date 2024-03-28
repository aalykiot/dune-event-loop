extern crate dune_event_loop as ev_loop;

use ev_loop::EventLoop;
use ev_loop::LoopHandle;
use signal_hook::consts::SIGINT;

fn main() {
    let mut event_loop = EventLoop::default();
    let handle = event_loop.handle();

    handle.timer(100000, false, |_: LoopHandle| {
        println!("Hello!");
    });

    let on_signal = move |_: LoopHandle, _: i32| {
        println!("Ctrl+C pressed!");
    };

    handle.signal_start_oneshot(SIGINT, on_signal).unwrap();

    while event_loop.has_pending_events() {
        event_loop.tick();
    }
}
