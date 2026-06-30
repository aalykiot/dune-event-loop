extern crate crabuv;

use anyhow::Result;
use crabuv::fs::FsEvent;
use crabuv::fs::FsWatcherHandle;
use crabuv::fs::WatchMode;
use crabuv::timers::TimerKind;
use crabuv::EventLoop;
use crabuv::LoopHandle;
use crabuv::RunMode;
use std::time::Duration;

fn main() {
    let mut event_loop = EventLoop::default();
    let handle = event_loop.handle();

    let directory = "./examples/";
    let mode = WatchMode::Recursive;

    let on_event = |_: FsWatcherHandle, event: Result<FsEvent>| {
        println!("{event:?}");
    };

    let timeout = Duration::from_secs(10);
    let watcher = handle.fs_watcher(directory, mode, on_event).unwrap();

    let on_timeout = move |_: LoopHandle| {
        watcher.stop();
    };

    handle.timer(timeout, TimerKind::Timeout, on_timeout);

    event_loop.run(RunMode::Default);
}
