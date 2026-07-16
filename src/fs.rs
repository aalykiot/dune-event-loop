use crate::event_loop::Event;
use crate::event_loop::LoopHandle;
use crate::resource::Resource;
use crate::resource::ResourceId;
use crate::resource::Shared;
use anyhow::Result;
use mio::Waker;
use notify::Config;
use notify::RecommendedWatcher;
use notify::RecursiveMode;
use notify::Watcher;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::Arc;

pub type WatchMode = RecursiveMode;

pub type FsEvent = notify::Event;
pub type FsEventKind = notify::EventKind;

pub type FsWatcherCallback = Box<dyn FnMut(FsWatcherHandle, Result<FsEvent>) + 'static>;

/// The data required for a file-system watcher.
pub(crate) struct FsWatcher {
    pub id: Shared<ResourceId>,
    pub path: PathBuf,
    pub callback: FsWatcherCallback,
    pub watcher: Option<RecommendedWatcher>,
    pub mode: WatchMode,
}

impl FsWatcher {
    /// Starts watching for file events in the specified path.
    pub fn watch(&mut self, waker: Arc<Waker>, event_sender: Sender<Event>) {
        // Create an appropriate watcher for the current system.
        let fs_handler = FsNotifyHandler {
            id: self.id.get(),
            waker,
            event_sender,
        };

        // Start watching requested path(s).
        let mut watcher = RecommendedWatcher::new(fs_handler, Config::default()).unwrap();
        watcher.watch(&self.path, self.mode).unwrap();

        self.watcher = Some(watcher);
    }

    /// Runs the callback of the file-system watcher.
    pub fn run_callback(&mut self, handle: LoopHandle, event: Result<FsEvent>) {
        // We need a handle to the resource that we will
        // pass to the callback.
        let handle = self.handle(handle);

        (self.callback)(handle, event);
    }

    /// Returns a handle to the fs event resource.
    pub fn handle(&self, handle: LoopHandle) -> FsWatcherHandle {
        FsWatcherHandle {
            id: self.id.clone(),
            handle,
        }
    }
}

impl Resource for FsWatcher {}

/// A handle to a file-system watcher resource.
#[derive(Clone)]
pub struct FsWatcherHandle {
    /// A shared pointer to the resource ID.
    pub(crate) id: Shared<ResourceId>,
    /// A handle to the event-loop.
    handle: LoopHandle,
}

impl FsWatcherHandle {
    /// Stops the watcher, the callback will no longer be called.
    pub fn stop(&self) {
        self.handle.fs_watcher_stop(self.id.clone());
    }

    /// Returns a handle to the event-loop.
    pub fn get_loop(&self) -> LoopHandle {
        self.handle.clone()
    }
}

/// An instance that knows how to handle fs events.
struct FsNotifyHandler {
    /// An actual ID tied to the resource.
    id: ResourceId,
    /// The raw event-loop waker.
    waker: Arc<Waker>,
    /// Dispatcher of event-loop events.
    event_sender: Sender<Event>,
}

impl notify::EventHandler for FsNotifyHandler {
    /// Handles an event.
    fn handle_event(&mut self, event: notify::Result<notify::Event>) {
        // Notify the main thread about the fs event.
        let event = Event::FsWatch(self.id, event.map_err(Into::into));

        self.event_sender.send(event).unwrap();
        self.waker.wake().unwrap();
    }
}
