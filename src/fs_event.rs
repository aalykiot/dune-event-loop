use crate::event_loop::Event;
use crate::event_loop::LoopHandle;
use crate::resource::Resource;
use crate::resource::ResourceId;
use crate::resource::Shared;
use mio::Waker;
use notify::Config;
use notify::RecommendedWatcher;
use notify::RecursiveMode;
use notify::Watcher;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::sync::Mutex;

pub type WatchMode = RecursiveMode;
pub type FileChangeEvent = notify::Event;

pub type FsEventCallback = Box<dyn FnMut(FsEventHandle, FileChangeEvent) + 'static>;

/// The data required for a file-system watcher.
pub(crate) struct FsEvent {
    pub id: Shared<ResourceId>,
    pub path: PathBuf,
    pub callback: FsEventCallback,
    pub watcher: Option<RecommendedWatcher>,
    pub mode: WatchMode,
}

impl FsEvent {
    /// Starts watching for file events in the specified path.
    pub fn watch(&mut self, waker: Arc<Waker>, event_sender: Arc<Mutex<Sender<Event>>>) {
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

    /// Returns a handle to the fs event resource.
    pub fn handle(&self, handle: LoopHandle) -> FsEventHandle {
        FsEventHandle {
            id: self.id.clone(),
            handle,
        }
    }
}

impl Resource for FsEvent {}

/// A handle to an active fs event resource, watching for file changes.
#[derive(Clone)]
pub struct FsEventHandle {
    /// A shared pointer to the resource ID.
    pub(crate) id: Shared<ResourceId>,
    /// A handle to the event-loop.
    handle: LoopHandle,
}

impl FsEventHandle {
    /// Stops the watcher, the callback will no longer be called.
    pub fn stop(self) {
        self.handle.fs_event_stop(self.id.clone());
    }
}

/// An instance that knows how to handle fs events.
struct FsNotifyHandler {
    /// An actual ID tied to the resource.
    id: ResourceId,
    /// The raw event-loop waker.
    waker: Arc<Waker>,
    /// Dispatcher of event-loop events.
    event_sender: Arc<Mutex<Sender<Event>>>,
}

impl notify::EventHandler for FsNotifyHandler {
    /// Handles an event.
    fn handle_event(&mut self, event: notify::Result<notify::Event>) {
        // Notify the main thread about this fs event.
        let event = Event::Watch(self.id, event.unwrap());

        self.event_sender.lock().unwrap().send(event).unwrap();
        self.waker.wake().unwrap();
    }
}
