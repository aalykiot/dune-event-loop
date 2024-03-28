extern crate dune_event_loop as ev_loop;

use ev_loop::EventLoop;
use ev_loop::LoopHandle;
use ev_loop::Signal::SIGINT;
use std::cell::Cell;
use std::rc::Rc;

fn main() {
    let mut event_loop = EventLoop::default();
    let handle = event_loop.handle();
    let ctrl_c = Rc::new(Cell::new(false));

    // Exit the program on double CTRL+C.
    let on_signal = move |_: LoopHandle, _: i32| {
        match ctrl_c.get() {
            true => std::process::exit(0),
            false => ctrl_c.set(true),
        };
    };

    handle.signal_start(SIGINT, on_signal).unwrap();

    loop {
        // We need somehow to keep the program running cause signal
        // listeners wont keep the event-loop alive.
        event_loop.tick();
    }
}
