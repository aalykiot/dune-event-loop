extern crate dune_event_loop;

use dune_event_loop::fs_watch::FileChangeEvent;
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

    let on_event = |_: FsWatcherHandle, event: FileChangeEvent| {
        println!("{event:?}");
    };

    let timeout = Duration::from_secs(10);
    let watcher = handle.fs_watcher_start(directory, mode, on_event).unwrap();

    handle.timer(timeout, TimerKind::Timeout, move |_: LoopHandle| {
        watcher.stop()
    });

    event_loop.run(RunMode::Default);
}
