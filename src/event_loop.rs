use crate::thread_pool::ThreadPool;
use crate::timers::TimersCollection;
use downcast_rs::impl_downcast;
use downcast_rs::Downcast;
use slotmap::DefaultKey;
use slotmap::SlotMap;
use std::cell::Cell;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Instant;

/// A type alias for resource identification.
pub type ResourceId = DefaultKey;

/// All objects that are tracked by the event-loop should implement the `Resource` trait.
trait Resource: Downcast + 'static {
    /// Implements any clean up actions.
    fn close(&mut self) {}
}

impl_downcast!(Resource);

enum Request {}

pub struct EventLoop {
    current_time: Instant,
    resources: SlotMap<ResourceId, Box<dyn Resource>>,
    timers: TimersCollection,
    request_queue: mpsc::Receiver<Request>,
    request_queue_empty: Rc<Cell<bool>>,
    request_sender: Rc<mpsc::Sender<Request>>,
    thread_pool: ThreadPool,
}

impl EventLoop {
    /// Creates a new event-loop instance.
    pub fn new(num_threads: usize) -> EventLoop {
        // Number of threads should always be a positive non-zero number.
        assert!(num_threads > 0);

        let thread_pool = ThreadPool::new(num_threads);
        let (request_sender, request_queue) = mpsc::channel();

        EventLoop {
            current_time: Instant::now(),
            resources: SlotMap::new(),
            timers: TimersCollection::new(),
            request_queue,
            request_queue_empty: Rc::new(Cell::new(true)),
            request_sender: Rc::new(request_sender),
            thread_pool,
        }
    }

    /// Returns a new handle to the event-loop.
    pub fn handle(&self) -> LoopHandle {
        LoopHandle {
            request_sender: self.request_sender.clone(),
            request_queue_empty: self.request_queue_empty.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct LoopHandle {
    request_sender: Rc<mpsc::Sender<Request>>,
    request_queue_empty: Rc<Cell<bool>>,
}
