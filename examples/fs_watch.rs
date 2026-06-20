extern crate dune_event_loop;

use anyhow::Result;
use dune_event_loop::fs_watch::FsEvent;
use dune_event_loop::fs_watch::FsWatcherHandle;
use dune_event_loop::fs_watch::WatchMode;
use dune_event_loop::timers::TimerKind;
use dune_event_loop::EventLoop;
use dune_event_loop::LoopHandle;
use dune_event_loop::RunMode;
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
