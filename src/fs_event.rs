use crate::event_loop::Event;
use crate::event_loop::LoopHandle;
use crate::resource::Resource;
use crate::resource::ResourceId;
use crate::resource::Shared;
use mio::Waker;
use notify::Config;
use notify::RecommendedWatcher;
use notify::RecursiveMode;
use slotmap::DefaultKey;
use slotmap::Key;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::sync::Mutex;

pub type WatchMode = RecursiveMode;
pub type FileChangeEvent = notify::Event;

/// The data required for a file-system watcher.
pub(crate) struct FsEvent {
    pub id: Shared<ResourceId>,
    pub path: PathBuf,
    pub watcher: Option<RecommendedWatcher>,
    pub mode: WatchMode,
    pub waker: Arc<Waker>,
    pub event_sender: Arc<Mutex<Sender<Event>>>,
}

impl FsEvent {
    /// Starts watching for file events in the specified path.
    pub fn start(&self, handle: LoopHandle) {
        // Create an appropriate watcher for the current system.
        let handle = self.handle(handle);
        let mut watcher = RecommendedWatcher::new(handle, Config::default()).unwrap();
    }

    /// Returns a handle to the fs event resource.
    pub fn handle(&self, handle: LoopHandle) -> FsEventHandle {
        FsEventHandle {
            // id: Arc::new(Mutex::new(self.id.clone())),
            waker: self.waker.clone(),
            event_sender: self.event_sender.clone(),
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
    /// The raw event-loop waker.
    waker: Arc<Waker>,
    /// Dispatcher of event-loop events.
    event_sender: Arc<Mutex<Sender<Event>>>,
}

struct FsNotifyHandler {
    /// A shared pointer to the resource ID.
    id: Arc<Mutex<Shared<ResourceId>>>,
    /// The raw event-loop waker.
    waker: Arc<Waker>,
    /// Dispatcher of event-loop events.
    event_sender: Arc<Mutex<Sender<Event>>>,
}

impl notify::EventHandler for FsNotifyHandler {
    /// Handles an event.
    fn handle_event(&mut self, event: notify::Result<notify::Event>) {
        // Notify the main thread about this fs event.
        let rid = self.id.lock().unwrap().get();
        let event = Event::Watch(rid, event.unwrap());

        self.event_sender.lock().unwrap().send(event).unwrap();
        self.waker.wake().unwrap();
    }
}
