//! A signal handling example.
//!
//! Listens for SIGINT (Ctrl+C) and exits on a second press. Uses oneshot
//! policy so the callback fires only once per signal registration.

use crabuv::signals::Kind;
use crabuv::signals::Policy;
use crabuv::signals::SignalHandle;
use crabuv::EventLoop;
use crabuv::RunMode;
use std::cell::Cell;

fn main() {
    let mut event_loop = EventLoop::default();
    let handle = event_loop.handle();
    let ctrl_c = Cell::new(false);

    // Exit the program on double CTRL+C.
    let cb = move |_: SignalHandle, _: i32| {
        match ctrl_c.get() {
            true => std::process::exit(0),
            false => ctrl_c.set(true),
        };
    };

    handle.signal(Kind::SIGINT, Policy::Oneshot, cb).unwrap();

    loop {
        // We need somehow to keep the program running because signal
        // listeners wont keep the event-loop alive.
        event_loop.run(RunMode::Once);
    }
}
