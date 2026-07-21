extern crate crabuv;

use crabuv::check::CheckHandle;
use crabuv::EventLoop;
use crabuv::RunMode;

fn main() {
    let mut event_loop = EventLoop::default();
    let handle = event_loop.handle();

    handle.check(|handle: CheckHandle| {
        println!("This will be called once, after the poll phase!");
        handle.remove();
    });

    event_loop.run(RunMode::Default);
}
