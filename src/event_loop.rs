use crate::thread_pool::ThreadPool;
use crate::timers::Timer;
use crate::timers::TimerKind;
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
pub trait Resource: Downcast + 'static {
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

    /// Drains the request_queue to schedule new workload.
    fn process_requests(&mut self) {
        while let Ok(request) = self.request_queue.try_recv() {
            match request {}
        }
        self.request_queue_empty.set(true);
    }

    /// Updates the event-loop's current time of now.
    fn update_current_time(&mut self) {
        self.current_time = Instant::now();
    }

    /// Performs a single tick of the event-loop.
    pub fn tick(&mut self) {
        self.update_current_time();
        self.process_requests();
        self.run_timers();
    }

    /// Runs all expired timers.
    fn run_timers(&mut self) {
        // Iterate through all the expired timers.
        for id in self.timers.split_expired_timers(self.current_time) {
            // In case we have a timer in the list but we don't have it as
            // a resource that means the timer was canceled.
            let handle = self.handle();
            let timer = match self.resources.get_mut(id) {
                Some(resource) => resource.downcast_mut::<Timer>().unwrap(),
                None => continue,
            };

            timer.run_callback(handle);

            // If the timer is repeatable reschedule it, otherwise drop it.
            if let TimerKind::Interval = timer.kind {
                let expires_at = self.current_time + timer.delay;
                self.timers.insert(expires_at, id);
            } else {
                self.resources.remove(id);
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct LoopHandle {
    request_sender: Rc<mpsc::Sender<Request>>,
    request_queue_empty: Rc<Cell<bool>>,
}
