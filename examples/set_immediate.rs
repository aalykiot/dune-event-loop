extern crate dune_event_loop;

use dune_event_loop::check::CheckHandle;
use dune_event_loop::EventLoop;
use dune_event_loop::RunMode;

fn main() {
    let mut event_loop = EventLoop::default();
    let handle = event_loop.handle();

    handle.check(|handle: CheckHandle| {
        println!("This will be called once, after the poll phase!");
        handle.remove();
    });

    event_loop.run(RunMode::Default);
}
