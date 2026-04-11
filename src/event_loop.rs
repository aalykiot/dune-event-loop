use crate::tcp_connection::TcpEventKind;
use crate::thread_pool::ThreadPool;
use crate::timers::Timer;
use crate::timers::TimerHandle;
use crate::timers::TimerKind;
use crate::timers::TimersCollection;
use downcast_rs::impl_downcast;
use downcast_rs::Downcast;
use mio::Poll;
use mio::Registry;
use mio::Token;
use mio::Waker;
use slotmap::DefaultKey;
use slotmap::Key;
use slotmap::SlotMap;
use std::cell::Cell;
use std::num::NonZeroUsize;
use std::rc::Rc;
use std::sync::mpsc;
use std::sync::Arc;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;
use std::time::Instant;

/// A type alias for resource identification.
pub type ResourceId = DefaultKey;

/// All objects that are tracked by the event-loop should implement the `Resource` trait.
pub trait Resource: Downcast + 'static {
    /// Implements any clean up actions.
    fn close(&mut self, _: LoopHandle) {}
}

impl_downcast!(Resource);

pub(crate) type BasicQueue = Vec<ResourceId>;

enum Request {
    StartTimer(Timer),
    CancelTimer(TimerHandle),
}

#[allow(dead_code)]
enum Event {
    /// A network operation is available.
    Network(TcpEventKind),
}

pub struct EventLoop {
    current_time: Instant,
    resources: SlotMap<ResourceId, Box<dyn Resource>>,
    timers: TimersCollection,
    close_queue: BasicQueue,
    request_queue: mpsc::Receiver<Request>,
    request_queue_empty: Rc<Cell<bool>>,
    request_sender: Rc<mpsc::Sender<Request>>,
    thread_pool: ThreadPool,
    event_queue: mpsc::Receiver<Event>,
    event_sender: mpsc::Sender<Event>,
    registry: Registry,
    poll: Poll,
    waker: Arc<Waker>,
}

impl EventLoop {
    /// Creates a new event-loop instance.
    pub fn new(num_threads: usize) -> EventLoop {
        // Number of threads should always be a positive non-zero number.
        assert!(num_threads > 0);

        let thread_pool = ThreadPool::new(num_threads);

        let (request_sender, request_queue) = mpsc::channel();
        let (event_sender, event_queue) = mpsc::channel();

        // Initialize the kernel notification multiplexer.
        let poll = Poll::new().unwrap();
        let registry = poll.registry().try_clone().unwrap();

        let waker = Waker::new(poll.registry(), Token(0)).unwrap();
        let waker = Arc::new(waker);

        EventLoop {
            current_time: Instant::now(),
            resources: SlotMap::new(),
            timers: TimersCollection::new(),
            close_queue: Vec::new(),
            request_queue,
            request_queue_empty: Rc::new(Cell::new(true)),
            request_sender: Rc::new(request_sender),
            thread_pool,
            event_queue,
            event_sender,
            registry,
            poll,
            waker,
        }
    }

    /// Returns a new handle to the event-loop.
    pub fn handle(&self) -> LoopHandle {
        LoopHandle {
            request_sender: self.request_sender.clone(),
            request_queue_empty: self.request_queue_empty.clone(),
        }
    }

    /// Returns if there is pending work still ongoing.
    pub fn has_pending_events(&self) -> bool {
        !self.resources.is_empty()
            || !self.request_queue_empty.get()
            || self.thread_pool.pending_count() != 0
    }

    /// Drains the request_queue to schedule new workload.
    fn process_requests(&mut self) {
        while let Ok(request) = self.request_queue.try_recv() {
            match request {
                Request::StartTimer(timer) => self.start_timer(timer),
                Request::CancelTimer(handle) => self.cancel_timer(handle),
            }
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

    /// Schedules a new timer in the event-loop.
    fn start_timer(&mut self, timer: Timer) {
        // First insert the new timer into the resources map, then set its
        // resource ID to the value returned by the insertion operation.
        let expires_at = self.current_time + timer.delay;

        let resource_id_slot = timer.id.clone();
        let resource_id = self.resources.insert(Box::new(timer));

        resource_id_slot.set(resource_id);

        self.timers.insert(expires_at, resource_id);
    }

    /// Removes a previously scheduled timer.
    fn cancel_timer(&mut self, handle: TimerHandle) {
        // To achieve O(1) cancellation, we remove the resource but keep the entry
        // in the timer collection. When processing expired timers, canceled
        // ones are simply ignored.
        self.resources.remove(handle.id.get());
    }
}

impl Default for EventLoop {
    fn default() -> Self {
        let default_pool_size = NonZeroUsize::new(4).unwrap();
        let num_cores = thread::available_parallelism().unwrap_or(default_pool_size);

        Self::new(num_cores.into())
    }
}

#[derive(Debug, Clone)]
pub struct LoopHandle {
    request_sender: Rc<mpsc::Sender<Request>>,
    request_queue_empty: Rc<Cell<bool>>,
}

impl LoopHandle {
    /// Schedules a new timer to the event-loop.
    pub fn timer<F>(&self, delay: Duration, kind: TimerKind, callback: F) -> TimerHandle
    where
        F: FnMut(LoopHandle) + 'static,
    {
        // Since the resource is not yet scheduled in the event-loop, we create a
        // null ID. The event-loop will update this value with a real ID later.
        let id = Rc::new(Cell::new(DefaultKey::null()));
        let callback = Box::new(callback);

        let timer = Timer {
            id,
            delay,
            kind,
            callback,
        };

        // Create a timer handle that we will return to the caller.
        let handle = timer.handle(self.clone());
        let request = Request::StartTimer(timer);

        self.request_sender.send(request).unwrap();
        self.request_queue_empty.set(false);

        handle
    }

    /// Removes a timer from the event-loop.
    pub fn cancel_timer(&self, handle: TimerHandle) {
        // Send a cancel request.
        let request = Request::CancelTimer(handle);

        self.request_sender.send(request).unwrap();
        self.request_queue_empty.set(false);
    }
}
