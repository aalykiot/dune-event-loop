extern crate crabuv;

use crabuv::task::Output;
use crabuv::EventLoop;
use crabuv::LoopHandle;
use crabuv::RunMode;
use std::any::Any;
use std::fs;

fn main() {
    let mut event_loop = EventLoop::default();
    let handle = event_loop.handle();

    let read_file = || -> Output {
        fs::read_to_string("./examples/async.rs")
            .map(|content| Box::new(content) as Box<dyn Any + Send>)
            .map_err(Into::into)
    };

    let read_file_cb = |_: LoopHandle, output: Output| match output {
        Err(e) => eprintln!("{}", e.to_string()),
        Ok(output) => {
            let content = output.downcast_ref::<String>().unwrap();
            println!("{}", content);
        }
    };

    handle.spawn(read_file, Some(read_file_cb));

    event_loop.run(RunMode::Default);
}
