use crate::tcp_stream::OnCloseCallback;
use crate::tcp_stream::OnReadCallback;
use crate::tcp_stream::OnWriteCallback;
use crate::tcp_stream::SocketInfo;
use crate::tcp_stream::TcpEventKind;
use crate::tcp_stream::TcpStream;
use crate::tcp_stream::TcpStreamHandle;
use crate::thread_pool::ThreadPool;
use crate::timers::Timer;
use crate::timers::TimerHandle;
use crate::timers::TimerKind;
use crate::timers::TimersCollection;
use anyhow::Result;
use downcast_rs::impl_downcast;
use downcast_rs::Downcast;
use mio::net::TcpStream as MioSocket;
use mio::Events;
use mio::Interest;
use mio::Poll;
use mio::Registry;
use mio::Token;
use mio::Waker;
use slotmap::DefaultKey;
use slotmap::Key;
use slotmap::KeyData;
use slotmap::SlotMap;
use std::cell::Cell;
use std::collections::VecDeque;
use std::io;
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::rc::Rc;
use std::sync::mpsc;
use std::sync::Arc;
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
    TimerStart(Timer),
    TimerCancel(ResourceId),
    TcpInit(TcpStream),
    TcpWrite(ResourceId, Vec<u8>, OnWriteCallback),
    TcpRead(ResourceId, OnReadCallback),
    TcpShutdown(ResourceId, OnCloseCallback),
    TcpClose(ResourceId, OnCloseCallback),
}

#[allow(dead_code)]
enum Event {
    /// A network operation is available.
    Network(TcpEventKind),
}

#[derive(Debug)]
pub enum RunMode {
    /// Runs the event loop until there are no resources.
    Default,
    /// Polls for I/O events once.
    Once,
    /// Does not block if there are no pending events.
    NoWait,
}

pub struct EventLoop {
    current_time: Instant,
    resources: SlotMap<ResourceId, Box<dyn Resource>>,
    timers: TimersCollection,
    check_queue: BasicQueue,
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
            check_queue: Vec::new(),
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

    /// Runs the even-loop by providing a run mode.
    pub fn run(&mut self, mode: RunMode) {
        loop {
            self.current_time = Instant::now();
            self.process_requests();
            self.run_timers();

            // Based on the run_mode and what resources the event-loop is currently
            // running, we will calculate the timout of the poll phase.
            let timeout = self.poll_timeout(&mode);
            self.run_poll(timeout);

            // Check if we need to exit, or continue the loop.
            match mode {
                RunMode::Once | RunMode::NoWait => break,
                RunMode::Default if !self.has_pending_events() => break,
                RunMode::Default => {}
            };
        }
    }

    /// Calculates the waiting time the poll phase should block for events.
    fn poll_timeout(&self, mode: &RunMode) -> Option<Duration> {
        // TODO: Describe how the calculation works..
        match mode {
            RunMode::NoWait => Some(Duration::ZERO),
            _ if !self.has_pending_events() => Some(Duration::ZERO),
            _ => {
                let refs = self.check_queue.len() + self.check_queue.len();
                match self.timers.next() {
                    _ if refs > 0 => Some(Duration::ZERO),
                    Some((t, _)) => Some(*t - self.current_time),
                    None => None,
                }
            }
        }
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

    /// Polls for new I/O events (async-tasks, networking, etc).
    fn run_poll(&mut self, timeout: Option<Duration>) {
        // Buffer to hold ready events.
        let mut events = Events::with_capacity(1024);

        // Poll for new network events (this will block the thread).
        if let Err(e) = self.poll.poll(&mut events, timeout) {
            match e.kind() {
                io::ErrorKind::Interrupted => return,
                _ => panic!("{}", e),
            };
        }

        for event in &events {
            // Note: Token(0) is a special token signaling that someone woke us up.
            if event.token() == Token(0) {
                continue;
            }

            let token = event.token();
            let readable = event.is_readable() || event.is_read_closed();
            let writable = event.is_writable();

            let event_type = match (readable, writable) {
                (true, _) => TcpEventKind::Read(token),
                (false, true) => TcpEventKind::Write(token),
                (false, false) => continue,
            };

            self.event_sender.send(Event::Network(event_type)).unwrap();
        }

        while let Ok(event) = self.event_queue.try_recv() {
            match event {
                Event::Network(event) => self.process_network_event(event),
            }
        }
    }

    /// Drains the request_queue to schedule new workload.
    fn process_requests(&mut self) {
        while let Ok(request) = self.request_queue.try_recv() {
            match request {
                Request::TimerStart(timer) => self.timer_start(timer),
                Request::TimerCancel(rid) => self.timer_cancel(rid),
                Request::TcpInit(stream) => self.tcp_stream_init(stream),
                Request::TcpRead(rid, callback) => self.tcp_stream_read_start(rid, callback),
                Request::TcpWrite(rid, data, cb) => self.tcp_stream_write(rid, data, cb),
                Request::TcpShutdown(rid, callback) => self.tcp_stream_shutdown(rid, callback),
                Request::TcpClose(rid, callback) => self.tcp_stream_close(rid, callback),
            }
        }
        self.request_queue_empty.set(true);
    }

    // Processes any ready network events from MIO.
    fn process_network_event(&mut self, event: TcpEventKind) {
        // Due to borrowing  constraints down the line, we need to
        // aqquire a loop handle at this point.
        let handle = self.handle();

        match event {
            TcpEventKind::Read(token) => {
                // We need to get the resource ID from the MIO token.
                let id = DefaultKey::from(KeyData::from_ffi(token.0 as u64));
                let stream = match self.resources.get_mut(id) {
                    Some(resource) => resource.downcast_mut::<TcpStream>().unwrap(),
                    None => return,
                };

                stream.write_to_socket(handle, &mut self.registry);
            }
            TcpEventKind::Write(token) => {
                // We need to get the resource ID from the MIO token.
                let id = DefaultKey::from(KeyData::from_ffi(token.0 as u64));
                let stream = match self.resources.get_mut(id) {
                    Some(resource) => resource.downcast_mut::<TcpStream>().unwrap(),
                    None => return,
                };

                stream.read_from_socket(handle, &mut self.registry, &mut self.close_queue);
            }
        }
    }

    /// Schedules a new timer in the event-loop.
    fn timer_start(&mut self, timer: Timer) {
        // First insert the new timer into the resources map, then set its
        // resource ID to the value returned by the insertion operation.
        let expires_at = self.current_time + timer.delay;

        let resource_id_slot = timer.id.clone();
        let resource_id = self.resources.insert(Box::new(timer));

        resource_id_slot.set(resource_id);

        self.timers.insert(expires_at, resource_id);
    }

    /// Removes a previously scheduled timer.
    fn timer_cancel(&mut self, rid: ResourceId) {
        // To achieve O(1) cancellation, we remove the resource but keep the entry
        // in the timer collection. When processing expired timers, canceled
        // ones are simply ignored.
        self.resources.remove(rid);
    }

    /// Initializes a new tcp connection.
    fn tcp_stream_init(&mut self, mut stream: TcpStream) {
        // When we create a new tcp socket connection we have to make sure
        // it's well connected with the remote host.
        //
        // See https://docs.rs/mio/0.8.4/mio/net/struct.TcpStream.html#notes
        let token = Token(stream.get_resource_id());
        let socket = &mut stream.socket;

        self.registry
            .register(socket, token, Interest::WRITABLE)
            .unwrap();

        let resource_id_slot = stream.id.clone();
        let resource_id = self.resources.insert(Box::new(stream));

        resource_id_slot.set(resource_id);
    }

    /// Registers interest for writing to a tcp socket.
    fn tcp_stream_write(&mut self, rid: ResourceId, data: Vec<u8>, callback: OnWriteCallback) {
        // Get the resource from the slotmap.
        let tcp_stream = match self.resources.get_mut(rid) {
            Some(resource) => resource.downcast_mut::<TcpStream>().unwrap(),
            None => return,
        };

        tcp_stream.enqueue(data, callback);

        let token = Token(tcp_stream.get_resource_id());
        let interest = Interest::READABLE.add(Interest::WRITABLE);

        self.registry
            .reregister(&mut tcp_stream.socket, token, interest)
            .unwrap();
    }

    /// Registers interest for reading from a tcp socket.
    fn tcp_stream_read_start(&mut self, rid: ResourceId, callback: OnReadCallback) {
        // Get resource from the slotmap.
        let tcp_stream = match self.resources.get_mut(rid) {
            Some(resource) => resource.downcast_mut::<TcpStream>().unwrap(),
            None => return,
        };

        let token = Token(tcp_stream.get_resource_id());
        tcp_stream.on_read = Some(callback);

        let interest = match tcp_stream.write_queue.len() {
            0 => Interest::READABLE,
            _ => Interest::READABLE.add(Interest::WRITABLE),
        };

        self.registry
            .reregister(&mut tcp_stream.socket, token, interest)
            .unwrap();
    }

    /// Schedules a full tcp stream shutdown.
    fn tcp_stream_close(&mut self, rid: ResourceId, callback: OnCloseCallback) {
        // Get the tcp stream resource.
        let tcp_stream = match self.resources.get_mut(rid) {
            Some(resource) => resource.downcast_mut::<TcpStream>().unwrap(),
            None => return,
        };

        tcp_stream.on_close = Some(callback);
        self.close_queue.push(rid);
    }

    /// Closes the write side of the tcp stream.
    fn tcp_stream_shutdown(&mut self, rid: ResourceId, callback: OnCloseCallback) {
        // We need to take the handle here due to borrowing constraints.
        let handle = self.handle();

        if let Some(resource) = self.resources.get_mut(rid) {
            resource.close(handle.clone());
            callback(handle);
        }
    }

    /// Returns if there is pending work still ongoing.
    fn has_pending_events(&self) -> bool {
        !self.resources.is_empty()
            || !self.request_queue_empty.get()
            || self.thread_pool.pending_count() != 0
    }

    /// Returns a new handle to the event-loop.
    pub fn handle(&self) -> LoopHandle {
        LoopHandle {
            request_sender: self.request_sender.clone(),
            request_queue_empty: self.request_queue_empty.clone(),
        }
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
        let request = Request::TimerStart(timer);

        self.request_sender.send(request).unwrap();
        self.request_queue_empty.set(false);

        handle
    }

    /// Removes a timer from the event-loop.
    pub(crate) fn cancel_timer(&self, id: ResourceId) {
        // Send a cancel request.
        let request = Request::TimerCancel(id);

        self.request_sender.send(request).unwrap();
        self.request_queue_empty.set(false);
    }

    /// Creates a new tcp stream and connects to the specified address.
    pub fn tcp_connect<F>(&self, address: SocketAddr, callback: F) -> Result<TcpStreamHandle>
    where
        F: Fn(TcpStreamHandle, Result<SocketInfo>) + 'static,
    {
        // Since the resource is not yet scheduled in the event-loop, we create a
        // null ID. The event-loop will update this value with a real ID later.
        let id = Rc::new(Cell::new(DefaultKey::null()));
        let callback = Box::new(callback);

        // Connect to the remote host.
        let stream = TcpStream {
            id,
            socket: MioSocket::connect(address)?,
            on_connection: Some(callback),
            on_read: None,
            on_close: None,
            write_queue: VecDeque::new(),
        };

        // Create a stream handle that we will return to the caller.
        let handle = stream.handle(self.clone());
        let request = Request::TcpInit(stream);

        self.request_sender.send(request).unwrap();
        self.request_queue_empty.set(false);

        Ok(handle)
    }

    /// Writes bytes to an open tcp stream.
    pub(crate) fn tcp_write<F>(&self, id: ResourceId, data: Vec<u8>, callback: F)
    where
        F: Fn(TcpStreamHandle, Result<usize>) + 'static,
    {
        let request = Request::TcpWrite(id, data, Box::new(callback));

        self.request_sender.send(request).unwrap();
        self.request_queue_empty.set(false);
    }

    /// Starts reading from an open tcp stream.
    pub(crate) fn tcp_read_start<F>(&self, id: ResourceId, callback: F)
    where
        F: Fn(TcpStreamHandle, Result<Vec<u8>>) + 'static,
    {
        let request = Request::TcpRead(id, Box::new(callback));

        self.request_sender.send(request).unwrap();
        self.request_queue_empty.set(false);
    }

    /// Closes the write side of the tcp stream.
    pub(crate) fn tcp_shutdown<F>(&self, id: ResourceId, callback: F)
    where
        F: Fn(LoopHandle) + 'static,
    {
        let request = Request::TcpShutdown(id, Box::new(callback));

        self.request_sender.send(request).unwrap();
        self.request_queue_empty.set(false);
    }

    /// Completely shutdowns the tcp stream.
    pub(crate) fn tcp_close<F>(&self, id: ResourceId, callback: F)
    where
        F: Fn(LoopHandle) + 'static,
    {
        let request = Request::TcpClose(id, Box::new(callback));

        self.request_sender.send(request).unwrap();
        self.request_queue_empty.set(false);
    }
}
