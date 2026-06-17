use crate::check::Check;
use crate::check::CheckHandle;
use crate::fs_event::FileChangeEvent;
use crate::fs_event::FsEvent;
use crate::fs_event::FsEventHandle;
use crate::fs_event::WatchMode;
use crate::resource::ResourceId;
use crate::resource::ResourceMap;
use crate::resource::Shared;
use crate::task::Output as TaskOutput;
use crate::task::Task;
use crate::task::TaskHandle;
use crate::task::WorkFn;
use crate::tcp_listener::TcpListener;
use crate::tcp_listener::TcpListenerHandle;
use crate::tcp_stream::OnCloseCallback;
use crate::tcp_stream::OnReadCallback;
use crate::tcp_stream::OnWriteCallback;
use crate::tcp_stream::TcpEventKind;
use crate::tcp_stream::TcpStream;
use crate::tcp_stream::TcpStreamHandle;
use crate::tcp_stream::READ_BUFFER_SIZE;
use crate::thread_pool::ThreadPool;
use crate::timers::Timer;
use crate::timers::TimerHandle;
use crate::timers::TimerKind;
use crate::timers::TimersCollection;
use anyhow::Result;
use mio::net::TcpListener as MioListener;
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
use std::cell::Cell;
use std::collections::VecDeque;
use std::io;
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::path::Path;
use std::rc::Rc;
use std::sync::mpsc;
use std::sync::Arc;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;
use std::time::Instant;

pub(crate) type BasicQueue = Vec<ResourceId>;

enum Request {
    TimerStart(Timer),
    TimerCancel(Shared<ResourceId>),
    TcpInit(Box<TcpStream>),
    TcpWrite(Shared<ResourceId>, Vec<u8>, OnWriteCallback),
    TcpRead(Shared<ResourceId>, OnReadCallback),
    TcpShutdown(Shared<ResourceId>, OnCloseCallback),
    TcpClose(Shared<ResourceId>, OnCloseCallback),
    TcpListen(Box<TcpListener>),
    TcpListenStop(Shared<ResourceId>, OnCloseCallback),
    TaskSpawn(Task, WorkFn, mpsc::Receiver<()>),
    TaskCancel(Shared<ResourceId>),
    CheckInit(Check),
    CheckRemove(Shared<ResourceId>),
    FsEventStart(FsEvent),
    FsEventStop(Shared<ResourceId>),
}

#[allow(dead_code)]
pub(crate) enum Event {
    /// A network operation is available.
    Network(TcpEventKind),
    /// A thread-pool task has been completed.
    ThreadPool(ResourceId, TaskOutput),
    /// A file-system change has been detected.
    Watch(ResourceId, FileChangeEvent),
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
    resources: ResourceMap,
    timers: TimersCollection,
    check_queue: BasicQueue,
    close_queue: BasicQueue,
    request_queue: mpsc::Receiver<Request>,
    request_queue_empty: Rc<Cell<bool>>,
    request_sender: Rc<mpsc::Sender<Request>>,
    thread_pool: ThreadPool,
    event_queue: mpsc::Receiver<Event>,
    event_sender: Arc<Mutex<mpsc::Sender<Event>>>,
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

        // Wrap the sender part of the channel so it can be safely shared
        // across the workers of the thread-pool.
        let event_sender = Arc::new(Mutex::new(event_sender));

        // Initialize the kernel notification multiplexer.
        let poll = Poll::new().unwrap();
        let registry = poll.registry().try_clone().unwrap();

        let waker = Waker::new(poll.registry(), Token(0)).unwrap();
        let waker = Arc::new(waker);

        EventLoop {
            current_time: Instant::now(),
            resources: ResourceMap::default(),
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
            self.run_check_callbacks();
            self.run_close();

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
                let refs = self.check_queue.len() + self.close_queue.len();
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
            let timer = match self.resources.get_mut_as::<Timer>(id) {
                Some(resource) => resource,
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

        // We will aquire the lock here and hold it until we process all
        // the available network events.
        let sender_lock = self.event_sender.lock().unwrap();

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
                _ => continue,
            };

            sender_lock.send(Event::Network(event_type)).unwrap();
        }

        drop(sender_lock);

        while let Ok(event) = self.event_queue.try_recv() {
            match event {
                Event::Network(event) => self.process_network_event(event),
                Event::ThreadPool(id, output) => self.process_finished_task(id, output),
                Event::Watch(_, _) => todo!(),
            }

            // Since each event might schedule additional I/O we need to process
            // the requests queue in every iteration.
            self.process_requests();
        }
    }

    /// Runs all check callbacks for this loop iteration.
    fn run_check_callbacks(&mut self) {
        // Due to Rust's ownership model we need to create a loop handle
        // here and re-use it when necessery.
        let handle = self.handle();

        for id in self.check_queue.iter() {
            // Get the resource from the collection.
            let check = match self.resources.get_mut_as::<Check>(*id) {
                Some(check) => check,
                None => continue,
            };

            check.run_callback(handle.clone());
        }

        // At this point we need to process the request queue in case
        // a check callback scheduled new resources.
        self.process_requests();
    }

    /// Runs any clean-up actions on "dying" resources.
    fn run_close(&mut self) {
        // Due to ownership constraints we need to get a handle
        // to the event-loop before the for-loop.
        let handle = self.handle();

        for id in self.close_queue.drain(..) {
            if let Some(mut resource) = self.resources.remove(id) {
                resource.destroy(handle.clone());
            }
        }

        self.process_requests();
    }

    /// Drains the request_queue to schedule new workload.
    fn process_requests(&mut self) {
        while let Ok(request) = self.request_queue.try_recv() {
            match request {
                Request::TimerStart(timer) => self.timer_start(timer),
                Request::TimerCancel(id) => self.timer_cancel(id),
                Request::TcpInit(stream) => self.tcp_stream_init(stream),
                Request::TcpRead(id, callback) => self.tcp_stream_read_start(id, callback),
                Request::TcpWrite(id, data, cb) => self.tcp_stream_write(id, data, cb),
                Request::TcpShutdown(id, callback) => self.tcp_stream_shutdown(id, callback),
                Request::TcpClose(id, callback) => self.tcp_stream_close(id, callback),
                Request::TcpListen(listener) => self.tcp_listener_init(listener),
                Request::TcpListenStop(id, callback) => self.tcp_listener_stop(id, callback),
                Request::TaskSpawn(task, work, cancel_rx) => self.task_spawn(task, work, cancel_rx),
                Request::TaskCancel(id) => self.task_cancel(id),
                Request::CheckInit(check) => self.check_init(check),
                Request::CheckRemove(id) => self.check_remove(id),
                Request::FsEventStart(fs_event) => self.fs_event_start(fs_event),
                Request::FsEventStop(id) => self.fs_event_stop(id),
            }
        }
        self.request_queue_empty.set(true);
    }

    /// Processes a finished task from the thread-pool.
    fn process_finished_task(&mut self, id: ResourceId, output: TaskOutput) {
        // If we receive a task ID that doesn't exist as a resource,
        // it means the task has already been canceled.
        let handle = self.handle();

        if let Some(mut resource) = self.resources.remove(id) {
            let task = resource.downcast_mut::<Task>().unwrap();
            task.run_callback(output, handle);
        }
    }

    /// Processes any ready network events from MIO.
    fn process_network_event(&mut self, event: TcpEventKind) {
        // Due to borrowing constraints down the line, we need to acquire
        // a loop handle at this point.
        let handle = self.handle();

        match event {
            TcpEventKind::Read(token) => {
                // MIO doesn't have a way to tell us if the event is for a socket or a listener
                // so we'll try to cast the resource into a tcp stream first. If that fails
                // will try to cast the resource into a tcp listener.
                let id = DefaultKey::from(KeyData::from_ffi(token.0 as u64));

                if let Some(stream) = self.resources.get_mut_as::<TcpStream>(id) {
                    stream.read_from_socket(handle, &mut self.registry, &mut self.close_queue);
                    return;
                }

                let listener = self.resources.get_mut_as::<TcpListener>(id).unwrap();
                let clients = listener.accept(handle);

                for stream in clients {
                    // For every new connection, add the stream to the resources so we can
                    // attach an actual ID and declare interest in the registry.
                    let id_slot = stream.id.clone();
                    let id = self.resources.insert(Box::new(stream));

                    id_slot.set(id);

                    // We need to get the stream from the resources to satisfy
                    // rust's ownership and borrowing rules.
                    let stream = self.resources.get_mut_as::<TcpStream>(id).unwrap();
                    let token = stream.token();
                    let socket = &mut stream.socket;

                    self.registry
                        .register(socket, token, Interest::READABLE)
                        .unwrap();
                }
            }
            TcpEventKind::Write(token) => {
                // We need to get the resource ID from the MIO token.
                let id = DefaultKey::from(KeyData::from_ffi(token.0 as u64));
                let stream = self.resources.get_mut_as::<TcpStream>(id).unwrap();
                let registry = &mut self.registry;
                let close_queue = &mut self.close_queue;

                stream.write_to_socket(handle, registry, close_queue);
            }
        }
    }

    /// Schedules a new timer in the event-loop.
    fn timer_start(&mut self, timer: Timer) {
        // First insert the new timer into the resources map, then set its
        // resource ID to the value returned by the insertion operation.
        let expires_at = self.current_time + timer.delay;

        let id_slot = timer.id.clone();
        let id = self.resources.insert(Box::new(timer));

        id_slot.set(id);

        self.timers.insert(expires_at, id);
    }

    /// Removes a previously scheduled timer.
    fn timer_cancel(&mut self, id: Shared<ResourceId>) {
        // To achieve O(1) cancellation, we remove the resource but keep the entry
        // in the timer collection. When processing expired timers, canceled
        // ones are simply ignored.
        self.resources.remove(id.get());
    }

    /// Schedules a new task for execution to the event-loop.
    fn task_spawn(&mut self, task: Task, work: WorkFn, cancel_rx: mpsc::Receiver<()>) {
        // The reason we insert the stream to the map and then we get a reference
        // is so we can create a token with the correct resource ID.
        let id_slot = task.id.clone();
        let id = self.resources.insert(Box::new(task));

        id_slot.set(id);

        let event_sender = self.event_sender.clone();
        let waker = self.waker.clone();

        self.thread_pool.spawn(
            move || {
                let output = work();
                let event = Event::ThreadPool(id, output);
                let event_sender = event_sender.lock().unwrap();

                event_sender.send(event).unwrap();
                waker.wake().unwrap();
            },
            cancel_rx,
        );
    }

    /// Removes a previously queued task.
    fn task_cancel(&mut self, id: Shared<ResourceId>) {
        // We only need to remove the resource from the map. If a task is already
        // running in the thread pool, its result will be discarded when
        // finished tasks are processed.
        self.resources.remove(id.get());
    }

    /// Initializes a new tcp connection.
    fn tcp_stream_init(&mut self, stream: Box<TcpStream>) {
        // The reason we insert the stream to the map and then we get a reference
        // is so we can create a token with the correct resource ID.
        let id_slot = stream.id.clone();
        let id = self.resources.insert(stream);

        id_slot.set(id);

        let stream = self.resources.get_mut_as::<TcpStream>(id).unwrap();

        // When we create a new tcp socket connection we have to make sure
        // it's well connected with the remote host.
        //
        // See https://docs.rs/mio/0.8.4/mio/net/struct.TcpStream.html#notes
        let token = stream.token();
        let socket = &mut stream.socket;

        self.registry
            .register(socket, token, Interest::WRITABLE)
            .unwrap();
    }

    /// Initializes a new tcp listener.
    fn tcp_listener_init(&mut self, listener: Box<TcpListener>) {
        // The reason we insert the stream to the map and then we get a reference
        // is so we can create a token with the correct resource ID.
        let id_slot = listener.id.clone();
        let id = self.resources.insert(listener);

        id_slot.set(id);

        let listener = self.resources.get_mut_as::<TcpListener>(id).unwrap();
        let token = listener.token();
        let socket = &mut listener.socket;

        self.registry
            .register(socket, token, Interest::READABLE)
            .unwrap();
    }

    /// Registers interest for writing to a tcp socket.
    fn tcp_stream_write(
        &mut self,
        id: Shared<ResourceId>,
        data: Vec<u8>,
        callback: OnWriteCallback,
    ) {
        // Get a mut reference to a tcp stream resource.
        let stream = self.resources.get_mut_as::<TcpStream>(id.get()).unwrap();
        stream.enqueue(data, callback);

        let token = stream.token();
        let interest = Interest::READABLE.add(Interest::WRITABLE);

        self.registry
            .reregister(&mut stream.socket, token, interest)
            .unwrap();
    }

    /// Registers interest for reading from a tcp socket.
    fn tcp_stream_read_start(&mut self, id: Shared<ResourceId>, callback: OnReadCallback) {
        // Get a mut reference to a tcp stream resource.
        let stream = self.resources.get_mut_as::<TcpStream>(id.get()).unwrap();
        let token = stream.token();

        stream.on_read = Some(callback);

        let interest = match stream.write_queue.len() {
            0 => Interest::READABLE,
            _ => Interest::READABLE.add(Interest::WRITABLE),
        };

        self.registry
            .reregister(&mut stream.socket, token, interest)
            .unwrap();
    }

    /// Schedules a full tcp stream shutdown.
    fn tcp_stream_close(&mut self, id: Shared<ResourceId>, callback: OnCloseCallback) {
        // Get a mut reference to a tcp stream resource.
        let stream = self.resources.get_mut_as::<TcpStream>(id.get()).unwrap();
        stream.on_close = Some(callback);

        self.close_queue.push(id.get());
    }

    /// Stops the listener from accepting new connections.
    fn tcp_listener_stop(&mut self, id: Shared<ResourceId>, callback: OnCloseCallback) {
        // Get a mut reference to a tcp stream resource.
        let stream = self.resources.get_mut_as::<TcpListener>(id.get()).unwrap();
        stream.on_close = Some(callback);

        self.close_queue.push(id.get());
    }

    /// Closes the write side of the tcp stream.
    fn tcp_stream_shutdown(&mut self, id: Shared<ResourceId>, mut callback: OnCloseCallback) {
        // We need to take the handle here due to borrowing constraints.
        let handle = self.handle();

        if let Some(resource) = self.resources.get_mut(id.get()) {
            resource.destroy(handle.clone());
            callback(handle);
        }
    }

    /// Initializes a check resources to the event-loop.
    fn check_init(&mut self, check: Check) {
        // The reason we insert the stream to the map and then we get a reference
        // is so we can create a token with the correct resource ID.
        let id_slot = check.id.clone();
        let id = self.resources.insert(Box::new(check));

        id_slot.set(id);

        self.check_queue.push(id);
    }

    /// Removes a check resource from the event-loop.
    fn check_remove(&mut self, id: Shared<ResourceId>) {
        // TODO: Using retain is not very performant since we're checking every
        // element in the vector. In the future it's best to come up with
        // a better solution.
        let id = id.get();

        self.resources.remove(id);
        self.check_queue.retain(|i| *i != id);
    }

    /// Initializes and starts a new file-system watcher.
    fn fs_event_start(&mut self, fs_event: FsEvent) {
        // The reason we insert the stream to the map and then we get a reference
        // is so we can create a token with the correct resource ID.
        let id_slot = fs_event.id.clone();
        let id = self.resources.insert(Box::new(fs_event));

        id_slot.set(id);

        // Note: We obtain a reference to the newly inserted fs_event before starting
        // the watcher because the watcher requires a valid resource ID, which is
        // only assigned once the fs_event has been inserted.
        let fs_event = self.resources.get_mut_as::<FsEvent>(id).unwrap();

        fs_event.watch(self.waker.clone(), self.event_sender.clone());
    }

    /// Stops and removes a file-system watcher from the event-loop.
    fn fs_event_stop(&mut self, id: Shared<ResourceId>) {
        self.resources.remove(id.get());
    }

    /// Returns true if there is pending work still ongoing.
    pub fn has_pending_events(&self) -> bool {
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
    pub(crate) fn cancel_timer(&self, id: Shared<ResourceId>) {
        // Send a cancel request.
        let request = Request::TimerCancel(id);

        self.request_sender.send(request).unwrap();
        self.request_queue_empty.set(false);
    }

    /// Schedules a new task to the event-loop.
    pub fn spawn<F>(&self, work: F) -> TaskHandle
    where
        F: FnOnce() -> TaskOutput + Send + 'static,
    {
        // Since the resource is not yet scheduled in the event-loop, we create a
        // null ID. The event-loop will update this value with a real ID later.
        let id = Rc::new(Cell::new(DefaultKey::null()));
        let work = Box::new(work);

        let (cancel_tx, cancel_rx) = mpsc::channel();

        let mut task = Task {
            id,
            on_complete: None,
            cancel_tx,
        };

        let handle = task.handle(self.clone());
        let request = Request::TaskSpawn(task, work, cancel_rx);

        self.request_sender.send(request).unwrap();
        self.request_queue_empty.set(false);

        handle
    }

    /// Schedules a new task with a callback to the event-loop.
    pub fn spawn_with_callback<F, U>(&self, work: F, callback: U) -> TaskHandle
    where
        F: FnOnce() -> TaskOutput + Send + 'static,
        U: FnMut(LoopHandle, TaskOutput) + 'static,
    {
        // Since the resource is not yet scheduled in the event-loop, we create a
        // null ID. The event-loop will update this value with a real ID later.
        let id = Rc::new(Cell::new(DefaultKey::null()));
        let work = Box::new(work);
        let on_complete = Box::new(callback);

        let (cancel_tx, cancel_rx) = mpsc::channel();

        let mut task = Task {
            id,
            on_complete: Some(on_complete),
            cancel_tx,
        };

        let handle = task.handle(self.clone());
        let request = Request::TaskSpawn(task, work, cancel_rx);

        self.request_sender.send(request).unwrap();
        self.request_queue_empty.set(false);

        handle
    }

    /// Removes a task from the event-loop.
    pub(crate) fn cancel_task(&self, id: Shared<ResourceId>) {
        // Send a cancel request.
        let request = Request::TaskCancel(id);

        self.request_sender.send(request).unwrap();
        self.request_queue_empty.set(false);
    }

    /// Creates a new tcp stream and connects to the specified address.
    pub fn tcp_connect<F>(&self, address: SocketAddr, callback: F) -> Result<()>
    where
        F: FnMut(Result<TcpStreamHandle>) + 'static,
    {
        // Since the resource is not yet scheduled in the event-loop, we create a
        // null ID. The event-loop will update this value with a real ID later.
        let id = Rc::new(Cell::new(DefaultKey::null()));
        let callback = Box::new(callback);

        // Connect to the remote host.
        let stream = Box::new(TcpStream {
            id,
            socket: MioSocket::connect(address)?,
            read_buffer: [0; READ_BUFFER_SIZE],
            on_connection: Some(callback),
            on_read: None,
            on_close: None,
            write_queue: VecDeque::new(),
        });

        self.request_sender.send(Request::TcpInit(stream)).unwrap();
        self.request_queue_empty.set(false);

        Ok(())
    }

    /// Starts listening for incoming connections.
    pub fn tcp_listen<F>(&self, address: SocketAddr, callback: F) -> Result<()>
    where
        F: FnMut(TcpListenerHandle, Result<TcpStreamHandle>) + 'static,
    {
        // Since the resource is not yet scheduled in the event-loop, we create a
        // null ID. The event-loop will update this value with a real ID later.
        let id = Rc::new(Cell::new(DefaultKey::null()));
        let on_connection = Box::new(callback);

        // Bind address to the socket.
        let socket = MioListener::bind(address)?;
        let listener = Box::new(TcpListener {
            id,
            socket,
            on_connection,
            on_close: None,
        });

        self.request_sender
            .send(Request::TcpListen(listener))
            .unwrap();
        self.request_queue_empty.set(false);

        Ok(())
    }

    /// Writes bytes to an open tcp stream.
    pub(crate) fn tcp_write<F>(&self, id: Shared<ResourceId>, data: Vec<u8>, callback: F)
    where
        F: FnMut(TcpStreamHandle, Result<usize>) + 'static,
    {
        let request = Request::TcpWrite(id, data, Box::new(callback));

        self.request_sender.send(request).unwrap();
        self.request_queue_empty.set(false);
    }

    /// Starts reading from an open tcp stream.
    pub(crate) fn tcp_read_start<F>(&self, id: Shared<ResourceId>, callback: F)
    where
        F: Fn(TcpStreamHandle, Result<Vec<u8>>) + 'static,
    {
        let request = Request::TcpRead(id, Box::new(callback));

        self.request_sender.send(request).unwrap();
        self.request_queue_empty.set(false);
    }

    /// Closes the write side of the tcp stream.
    pub(crate) fn tcp_shutdown<F>(&self, id: Shared<ResourceId>, callback: F)
    where
        F: FnMut(LoopHandle) + 'static,
    {
        let request = Request::TcpShutdown(id, Box::new(callback));

        self.request_sender.send(request).unwrap();
        self.request_queue_empty.set(false);
    }

    /// Completely shutdowns the tcp stream.
    pub(crate) fn tcp_close<F>(&self, id: Shared<ResourceId>, callback: F)
    where
        F: FnMut(LoopHandle) + 'static,
    {
        let request = Request::TcpClose(id, Box::new(callback));

        self.request_sender.send(request).unwrap();
        self.request_queue_empty.set(false);
    }

    /// Stops a listener from accepting new connections.
    pub(crate) fn tcp_stop<F>(&self, id: Shared<ResourceId>, callback: F)
    where
        F: FnMut(LoopHandle) + 'static,
    {
        let request = Request::TcpListenStop(id, Box::new(callback));

        self.request_sender.send(request).unwrap();
        self.request_queue_empty.set(false);
    }

    /// Initializes a new check callback to the event-loop. Check resources will run the
    /// given callback once per loop iteration, right after polling for I/O.
    pub fn check<F>(&self, callback: F) -> CheckHandle
    where
        F: FnMut(CheckHandle) + 'static,
    {
        // Since the resource is not yet scheduled in the event-loop, we create a
        // null ID. The event-loop will update this value with a real ID later.
        let id = Rc::new(Cell::new(DefaultKey::null()));
        let callback = Box::new(callback);

        let check = Check { id, callback };
        let handle = check.handle(self.clone());

        let request = Request::CheckInit(check);

        self.request_sender.send(request).unwrap();
        self.request_queue_empty.set(false);

        handle
    }

    /// Removes a check resources from the event-loop.
    pub(crate) fn check_remove(&self, id: Shared<ResourceId>) {
        // Send a remove request.
        let request = Request::CheckRemove(id);

        self.request_sender.send(request).unwrap();
        self.request_queue_empty.set(false);
    }

    /// Creates a watcher that monitors the specified path for changes.
    pub fn fs_event_start<P, F>(
        &self,
        path: P,
        mode: WatchMode,
        callback: F,
    ) -> Result<FsEventHandle>
    where
        F: FnMut(FsEventHandle, FileChangeEvent) + 'static,
        P: AsRef<Path>,
    {
        // Since the resource is not yet scheduled in the event-loop, we create a
        // null ID. The event-loop will update this value with a real ID later.
        let id = Rc::new(Cell::new(DefaultKey::null()));
        let callback = Box::new(callback);

        // Check if path exists.
        std::fs::metadata(path.as_ref())?;

        let fs_event = FsEvent {
            id,
            path: path.as_ref().to_path_buf(),
            callback,
            mode,
            watcher: None,
        };

        let handle = fs_event.handle(self.clone());
        let request = Request::FsEventStart(fs_event);

        self.request_sender.send(request).unwrap();
        self.request_queue_empty.set(false);

        Ok(handle)
    }

    /// Stops the watcher, the callback will no longer be called.
    pub(crate) fn fs_event_stop(&self, id: Shared<ResourceId>) {
        // Send a remove request.
        let request = Request::FsEventStop(id);

        self.request_sender.send(request).unwrap();
        self.request_queue_empty.set(false);
    }
}
