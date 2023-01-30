extern crate dune_event_loop as ev_loop;

use ev_loop::EventLoop;
use ev_loop::FsEvent;
use ev_loop::LoopHandle;
fn main() {
    let mut event_loop = EventLoop::default();
    let handle = event_loop.handle();

    let on_event = |_: LoopHandle, event: FsEvent| {
        println!("{event:?}");
    };

    let directory = "./examples/";
    let rid = match handle.fs_event_start(directory, true, on_event) {
        Ok(rid) => rid,
        Err(e) => {
            println!("{e}");
            return;
        }
    };

    handle.timer(10000, false, move |h: LoopHandle| h.fs_event_stop(&rid));

    while event_loop.has_pending_events() {
        event_loop.tick();
    }
}
