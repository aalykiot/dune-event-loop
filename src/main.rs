use anyhow::Result;
use downcast_rs::impl_downcast;
use downcast_rs::Downcast;
use mio::net::TcpListener;
use mio::net::TcpStream;
use mio::Events;
use mio::Interest;
use mio::Poll;
use mio::Registry;
use mio::Token;
use mio::Waker;
use std::any::type_name;
use std::borrow::Cow;
use std::cell::Cell;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::LinkedList;
use std::io;
use std::io::Read;
use std::io::Write;
use std::net::Shutdown;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::mpsc;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;
use threadpool::ThreadPool;

/// Wrapper type for resource identification.
type Index = u32;

/// All objects that are tracked by the event-loop should implement the `Resource` trait.
pub trait Resource: Downcast + 'static {
    /// Returns a string representation of the resource.
    fn name(&self) -> Cow<str> {
        type_name::<Self>().into()
    }
    /// Custom way to close any resources.
    fn close(&mut self) {}
}

impl_downcast!(Resource);

/// Describes a timer resource.
struct TimerWrap {
    cb: Box<dyn FnMut(LoopHandle) + 'static>,
    expires_at: Duration,
    repeat: bool,
}

impl Resource for TimerWrap {}

/// Describes an async task.
struct TaskWrap {
    inner: Option<TaskOnFinish>,
}

impl Resource for TaskWrap {}

// Wrapper types for the task resource.
type Task = Box<dyn FnOnce() -> TaskResult + Send>;
type TaskResult = Option<Result<Vec<u8>>>;
type TaskOnFinish = Box<dyn FnOnce(LoopHandle, TaskResult) + 'static>;

// Wrapper types for different TCP callbacks.
type TcpOnConnection = Box<dyn FnOnce(LoopHandle, Index, Result<TcpSocketInfo>) + 'static>;
type TcpListenerOnConnection = Box<dyn FnMut(LoopHandle, Index, Result<TcpSocketInfo>) + 'static>;
type TcpOnWrite = Box<dyn FnOnce(LoopHandle, Index, Result<usize>) + 'static>;
type TcpOnRead = Box<dyn FnMut(LoopHandle, Index, Result<Vec<u8>>) + 'static>;

// Wrapper around close callbacks.
type OnClose = Box<dyn FnOnce(LoopHandle) + 'static>;

/// Describes a TCP connection.
struct TcpStreamWrap {
    id: Index,
    socket: TcpStream,
    on_connection: Option<TcpOnConnection>,
    on_read: Option<TcpOnRead>,
    write_queue: LinkedList<(Vec<u8>, TcpOnWrite)>,
}

impl Resource for TcpStreamWrap {
    #[allow(unused_must_use)]
    fn close(&mut self) {
        // Shutdown the write side of the stream.
        self.socket.shutdown(Shutdown::Write);
    }
}

/// Describes a TCP server.
struct TcpListenerWrap {
    id: Index,
    socket: TcpListener,
    on_connection: TcpListenerOnConnection,
}

impl Resource for TcpListenerWrap {}

#[allow(dead_code)]
/// Useful information about a TCP socket.
pub struct TcpSocketInfo {
    id: Index,
    host: SocketAddr,
    remote: SocketAddr,
}

#[allow(clippy::enum_variant_names)]
enum Action {
    TimerReq(Index, TimerWrap),
    TimerDeleteReq(Index),
    SpawnReq(Index, Task, TaskWrap),
    TcpConnectionReq(Index, TcpStreamWrap),
    TcpListenReq(Index, TcpListenerWrap),
    TcpWriteReq(Index, Vec<u8>, TcpOnWrite),
    TcpReadStartReq(Index, TcpOnRead),
    TcpCloseReq(Index, OnClose),
    TcpShutdownReq(Index),
}

enum Event {
    ThreadPool(Index, TaskResult),
    Network(TcpEvent),
}

#[derive(Debug)]
enum TcpEvent {
    // Socket is (probably) ready for reading.
    Read(Index),
    // Socket is (probably) ready for writing.
    Write(Index),
}

pub struct EventLoop {
    index: Rc<Cell<Index>>,
    resources: HashMap<Index, Box<dyn Resource>>,
    timer_queue: BTreeMap<Instant, Index>,
    action_queue: mpsc::Receiver<Action>,
    action_queue_empty: Rc<Cell<bool>>,
    action_dispatcher: Rc<mpsc::Sender<Action>>,
    close_queue: Vec<(Index, OnClose)>,
    thread_pool: ThreadPool,
    event_dispatcher: Arc<Mutex<mpsc::Sender<Event>>>,
    event_queue: mpsc::Receiver<Event>,
    pending_tasks: u32,
    network_events: Registry,
    poll: Poll,
    waker: Arc<Waker>,
}

//---------------------------------------------------------
//  PUBLICLY EXPOSED METHODS.
//---------------------------------------------------------

impl EventLoop {
    /// Creates a new event-loop instance.
    pub fn new() -> Self {
        let (action_dispatcher, action_queue) = mpsc::channel();
        let (event_dispatcher, event_queue) = mpsc::channel();

        // Wrap event_dispatcher into a Arc<Mutex>.
        let event_dispatcher = Arc::new(Mutex::new(event_dispatcher));

        // Create network handles.
        let poll = Poll::new().unwrap();
        let waker = Waker::new(poll.registry(), Token(0)).unwrap();
        let registry = poll.registry().try_clone().unwrap();

        EventLoop {
            index: Rc::new(Cell::new(1)),
            resources: HashMap::new(),
            timer_queue: BTreeMap::new(),
            action_queue,
            action_queue_empty: Rc::new(Cell::new(true)),
            action_dispatcher: Rc::new(action_dispatcher),
            close_queue: Vec::new(),
            thread_pool: ThreadPool::new(4),
            event_dispatcher,
            event_queue,
            pending_tasks: 0,
            poll,
            network_events: registry,
            waker: Arc::new(waker),
        }
    }

    /// Returns a new handle to the event-loop.
    pub fn handle(&self) -> LoopHandle {
        LoopHandle {
            index: self.index.clone(),
            actions: self.action_dispatcher.clone(),
            actions_queue_empty: self.action_queue_empty.clone(),
        }
    }

    /// Returns a new interrupt-handle to the event-loop (sharable across threads).
    pub fn interrupt_handle(&self) -> LoopInterruptHandle {
        LoopInterruptHandle {
            waker: self.waker.clone(),
        }
    }

    /// Returns if there are pending events still ongoing.
    pub fn has_pending_events(&self) -> bool {
        !(self.resources.is_empty() && self.action_queue_empty.get())
    }

    /// Performs a single tick of the event-loop.
    pub fn tick(&mut self) {
        self.prepare();
        self.run_timers();
        self.run_poll();
        self.run_close();
    }
}

//---------------------------------------------------------
//  EVENT LOOP PHASES.
//---------------------------------------------------------

impl EventLoop {
    /// Drains the action_queue for requested async actions.
    fn prepare(&mut self) {
        while let Ok(action) = self.action_queue.try_recv() {
            match action {
                Action::TimerReq(index, timer) => self.timer_req(index, timer),
                Action::TimerDeleteReq(index) => self.timer_delete_req(&index),
                Action::SpawnReq(index, task, t_wrap) => self.spawn_req(index, task, t_wrap),
                Action::TcpConnectionReq(index, tc_wrap) => self.tcp_connection_req(index, tc_wrap),
                Action::TcpListenReq(index, tc_wrap) => self.tcp_listen_req(index, tc_wrap),
                Action::TcpWriteReq(index, data, cb) => self.tcp_write_req(index, data, cb),
                Action::TcpReadStartReq(index, cb) => self.tcp_read_start_req(index, cb),
                Action::TcpCloseReq(index, cb) => self.tcp_close_req(index, cb),
                Action::TcpShutdownReq(index) => self.tcp_shutdown_req(index),
            };
        }
        self.action_queue_empty.set(true);
    }

    /// Runs all expired timers.
    fn run_timers(&mut self) {
        // Note: We use this intermediate vector so we don't have Rust complaining
        // about holding multiple references.
        let timers_to_remove: Vec<Instant> = self
            .timer_queue
            .range(..Instant::now())
            .map(|(k, _)| *k)
            .collect();

        let indexes: Vec<Index> = timers_to_remove
            .iter()
            .filter_map(|instant| self.timer_queue.remove(instant))
            .collect();

        indexes.iter().for_each(|index| {
            // Create a new event-loop handle to pass in timer's callback.
            let handle = self.handle();

            if let Some(timer) = self
                .resources
                .get_mut(index)
                .map(|resource| resource.downcast_mut::<TimerWrap>().unwrap())
            {
                // Run timer's callback.
                (timer.cb)(handle);

                // If the timer is repeatable reschedule him, otherwise drop him.
                if timer.repeat {
                    let time_key = Instant::now() + timer.expires_at;
                    self.timer_queue.insert(time_key, *index);
                } else {
                    self.resources.remove(index);
                }
            }
        });

        self.prepare();
    }

    /// Polls for new I/O events (async-tasks, networking, etc).
    fn run_poll(&mut self) {
        // Based on what resources the event-loop is currently running will decide
        // how long we should wait on the this phase.
        let timeout = match self.timer_queue.iter().next() {
            Some((t, _)) => Some(*t - Instant::now()),
            None if self.pending_tasks > 0 => None,
            None => Some(Duration::ZERO),
        };

        let mut events = Events::with_capacity(1024);

        // Poll for new network events (this will block the thread).
        self.poll.poll(&mut events, timeout).unwrap();

        for event in &events {
            // Note: Token(0) is a special token signaling that someone woke us up.
            if event.token() == Token(0) {
                break;
            }

            let event_type = match (
                event.is_readable() || event.is_read_closed(),
                event.is_writable(),
            ) {
                (true, false) => TcpEvent::Read(event.token().0 as u32),
                (false, true) => TcpEvent::Write(event.token().0 as u32),
                _ => continue,
            };

            self.event_dispatcher
                .lock()
                .unwrap()
                .send(Event::Network(event_type))
                .unwrap();
        }

        while let Ok(event) = self.event_queue.try_recv() {
            match event {
                Event::ThreadPool(index, result) => self.task_complete(index, result),
                Event::Network(tcp_event) => match tcp_event {
                    TcpEvent::Write(index) => self.tcp_socket_write(index),
                    TcpEvent::Read(index) => self.tcp_socket_read(index),
                },
            }
            self.prepare();
        }
    }

    /// Cleans up `dying` resources.
    fn run_close(&mut self) {
        // Create a new event-loop handle.
        let handle = self.handle();

        // Clean up resources.
        for (rid, on_close) in self.close_queue.drain(..) {
            if let Some(mut resource) = self.resources.remove(&rid) {
                resource.close();
                (on_close)(handle.clone());
            }
        }
        self.prepare();
    }
}

//---------------------------------------------------------
//  INTERNAL (AFTER) ASYNC OPERATION HANDLES.
//---------------------------------------------------------

impl EventLoop {
    /// Runs callback of finished async task.
    fn task_complete(&mut self, index: Index, result: TaskResult) {
        if let Some(mut resource) = self.resources.remove(&index) {
            let task_wrap = resource.downcast_mut::<TaskWrap>().unwrap();
            let callback = task_wrap.inner.take().unwrap();
            (callback)(self.handle(), result);

            self.pending_tasks -= 1;
        }
    }

    /// Tries to write to a (ready) TCP socket.
    /// `ready` = the operation won't block the current thread.
    fn tcp_socket_write(&mut self, index: Index) {
        // Create a new handle.
        let handle = self.handle();

        // Try to get a reference to the resource.
        let resource = match self.resources.get_mut(&index) {
            Some(resource) => resource,
            None => return,
        };

        // Cast resource to TcpStreamWrap.
        let tcp_wrap = resource.downcast_mut::<TcpStreamWrap>().unwrap();

        // Check if the socket is in error state.
        if let Ok(Some(err)) | Err(err) = tcp_wrap.socket.take_error() {
            // If `on_connection` is available it means the socket error happened
            // while trying to connect.
            if let Some(on_connection) = tcp_wrap.on_connection.take() {
                (on_connection)(handle, index, Result::Err(err.into()));
                return;
            }
            // Otherwise the error happened while writing.
            if let Some((_, on_write)) = tcp_wrap.write_queue.pop_front() {
                (on_write)(handle, index, Result::Err(err.into()));
                return;
            }
        }

        // Note: If the on_connection callback is None it means that in some
        // previous iteration we made sure the TCP socket is well connected
        // with the remote host.

        if let Some(on_connection) = tcp_wrap.on_connection.take() {
            // Run socket's on_connection callback.
            (on_connection)(
                handle,
                index,
                Ok(TcpSocketInfo {
                    id: index,
                    host: tcp_wrap.socket.local_addr().unwrap(),
                    remote: tcp_wrap.socket.peer_addr().unwrap(),
                }),
            );

            let token = Token(index as usize);

            self.network_events
                .reregister(&mut tcp_wrap.socket, token, Interest::READABLE)
                .unwrap();

            return;
        }

        // Connection is OK, let's write some bytes...

        if !tcp_wrap.write_queue.is_empty() {
            // Get data and on_write callback.
            let (data, on_write) = tcp_wrap.write_queue.pop_front().unwrap();

            // Try write some bytes to the socket.
            match tcp_wrap.socket.write(&data) {
                Ok(bytes_written) => (on_write)(handle, index, Result::Ok(bytes_written)),
                Err(err) => (on_write)(handle, index, Result::Err(err.into())),
            };
        }
    }

    /// Tries to read from a (ready) TCP socket.
    /// `ready` = the operation won't block the current thread.
    fn tcp_socket_read(&mut self, index: Index) {
        // Create a new handle.
        let handle = self.handle();

        // Try to get a reference to the resource.
        let resource = match self.resources.get_mut(&index) {
            Some(resource) => resource,
            None => return,
        };

        // Check if the TCP read event is really a TCP accept for some listener.
        if resource.downcast_ref::<TcpListenerWrap>().is_some() {
            self.tcp_try_accept(index);
            return;
        }

        // Cast resource to TcpStreamWrap.
        let tcp_wrap = resource.downcast_mut::<TcpStreamWrap>().unwrap();

        // Prepare the read buffer.
        let mut buffer = vec![0; 4096];
        let mut bytes_read = 0;

        // This will help us catch errors and FIN packets.
        let mut read_error: Option<io::Error> = None;
        let mut connection_closed = false;

        // We can (maybe) read from the connection.
        loop {
            match tcp_wrap.socket.read(&mut buffer[bytes_read..]) {
                // Reading 0 bytes means the other side has closed the
                // connection or is done writing.
                Ok(0) => {
                    connection_closed = true;
                    break;
                }
                Ok(n) => {
                    bytes_read += n;
                    // If the buffer is not big enough, extend it.
                    if bytes_read == buffer.len() {
                        buffer.resize(buffer.len() + 1024, 0);
                    }
                }
                // Would block "errors" are the OS's way of saying that the
                // connection is not actually ready to perform this I/O operation.
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                // Other errors we'll be considered fatal.
                Err(err) => read_error = Some(err),
            }
        }

        let on_read = tcp_wrap.on_read.as_mut().unwrap();

        // Check if we had any errors while reading.
        if let Some(err) = read_error {
            // Run on_read callback.
            (on_read)(handle, index, Result::Err(err.into()));
            return;
        }

        buffer.resize(bytes_read, 0);

        match buffer.len() {
            // FIN packet.
            0 => (on_read)(handle, index, Result::Ok(buffer)),
            // We read some bytes.
            _ if !connection_closed => (on_read)(handle, index, Result::Ok(buffer)),
            // FIN packet is included to the bytes we read.
            _ => {
                (on_read)(handle.clone(), index, Result::Ok(buffer));
                (on_read)(handle, index, Result::Ok(vec![]));
            }
        };
    }

    /// Tries to accept a new TCP connection.
    fn tcp_try_accept(&mut self, index: Index) {
        // Create a new handle.
        let handle = self.handle();

        // Try to get a reference to the resource.
        let resource = match self.resources.get_mut(&index) {
            Some(resource) => resource,
            None => return,
        };

        // Note: In case the downcast to TcpListenerWrap fails it means that the event
        // fired by the network thread is not for a TCP accept.

        let tcp_wrap = match resource.downcast_mut::<TcpListenerWrap>() {
            Some(tcp_wrap) => tcp_wrap,
            None => return,
        };

        let on_connection = tcp_wrap.on_connection.as_mut();

        // Received an event for the TCP server socket, which indicates we can accept a connection.
        let (socket, _) = match tcp_wrap.socket.accept() {
            Ok(sock) => sock,
            // If we get a `WouldBlock` error we know our
            // listener has no more incoming connections queued,
            // so we can return to polling and wait for some
            // more.
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
            Err(e) => {
                (on_connection)(handle, index, Result::Err(e.into()));
                return;
            }
        };

        // Create a new ID for the socket.
        let id = handle.index();

        // Create a TCP wrap from the raw socket.
        let mut stream = TcpStreamWrap {
            id,
            socket,
            on_connection: None,
            on_read: None,
            write_queue: LinkedList::new(),
        };

        (on_connection)(
            handle,
            id,
            Ok(TcpSocketInfo {
                id: index,
                host: stream.socket.local_addr().unwrap(),
                remote: stream.socket.peer_addr().unwrap(),
            }),
        );

        // Initialize socket with a READABLE event.
        self.network_events
            .register(&mut stream.socket, Token(id as usize), Interest::READABLE)
            .unwrap();

        // Register the new TCP stream to the event-loop.
        self.resources.insert(id, Box::new(stream));
    }
}

//---------------------------------------------------------
//  INTERNAL (SCHEDULING) ASYNC OPERATION HANDLES.
//---------------------------------------------------------

impl EventLoop {
    /// Schedules a new timer.
    fn timer_req(&mut self, index: Index, timer: TimerWrap) {
        let time_key = Instant::now() + timer.expires_at;
        self.resources.insert(index, Box::new(timer));
        self.timer_queue.insert(time_key, index);
    }

    /// Removes an existed timer.
    fn timer_delete_req(&mut self, index: &Index) {
        self.resources.remove(index);
        self.timer_queue.retain(|_, v| *v != *index);
    }

    /// Spawns a new task to the thread-pool.
    fn spawn_req(
        &mut self,
        index: Index,
        task: Box<dyn FnOnce() -> TaskResult + Send>,
        task_wrap: TaskWrap,
    ) {
        let notifier = self.event_dispatcher.clone();

        if task_wrap.inner.is_some() {
            self.resources.insert(index, Box::new(task_wrap));
        }

        self.thread_pool.execute({
            let waker = self.waker.clone();
            move || {
                let result = (task)();
                let notifier = notifier.lock().unwrap();

                notifier.send(Event::ThreadPool(index, result)).unwrap();
                waker.wake().unwrap();
            }
        });

        self.pending_tasks += 1;
    }

    /// Registers interest for connecting to a TCP socket.
    fn tcp_connection_req(&mut self, index: Index, mut tcp_wrap: TcpStreamWrap) {
        // When we create a new TCP socket connection we have to make sure
        // it's well connected with the remote host.
        //
        // See https://docs.rs/mio/0.8.4/mio/net/struct.TcpStream.html#notes
        let socket = &mut tcp_wrap.socket;
        let token = Token(tcp_wrap.id as usize);

        self.network_events
            .register(socket, token, Interest::WRITABLE)
            .unwrap();

        self.resources.insert(index, Box::new(tcp_wrap));
    }

    /// Registers the TCP listener to the event-loop.
    fn tcp_listen_req(&mut self, index: Index, mut tcp_wrap: TcpListenerWrap) {
        let listener = &mut tcp_wrap.socket;
        let token = Token(tcp_wrap.id as usize);

        self.network_events
            .register(listener, token, Interest::READABLE)
            .unwrap();

        self.resources.insert(index, Box::new(tcp_wrap));
    }

    /// Registers interest for writing to an open TCP socket.
    fn tcp_write_req(&mut self, index: Index, data: Vec<u8>, on_write: TcpOnWrite) {
        let resource = match self.resources.get_mut(&index) {
            Some(resource) => resource,
            None => return,
        };

        // Cast resource to TcpStreamWrap.
        let tcp_wrap = resource.downcast_mut::<TcpStreamWrap>().unwrap();
        let token = Token(index as usize);

        // Push data to socket's write queue.
        tcp_wrap.write_queue.push_back((data, on_write));

        let interest = Interest::WRITABLE | Interest::READABLE;

        self.network_events
            .reregister(&mut tcp_wrap.socket, token, interest)
            .unwrap();
    }

    /// Registers interest for reading of an open TCP socket.
    fn tcp_read_start_req(&mut self, index: Index, on_read: TcpOnRead) {
        let resource = match self.resources.get_mut(&index) {
            Some(resource) => resource,
            None => return,
        };

        // Cast resource to TcpStreamWrap.
        let tcp_wrap = resource.downcast_mut::<TcpStreamWrap>().unwrap();
        let token = Token(index as usize);

        // Register the on_read callback.
        tcp_wrap.on_read = Some(on_read);

        let interest = match tcp_wrap.write_queue.len() {
            0 => Interest::READABLE,
            _ => Interest::READABLE | Interest::WRITABLE,
        };

        self.network_events
            .reregister(&mut tcp_wrap.socket, token, interest)
            .unwrap();
    }

    /// Schedules a TCP socket shutdown.
    fn tcp_close_req(&mut self, index: Index, on_close: Box<dyn FnOnce(LoopHandle) + 'static>) {
        // Schedule resource for graceful shutdown and removal.
        self.close_queue.push((index, on_close));
    }

    /// Closes the write side of the stream.
    fn tcp_shutdown_req(&mut self, index: Index) {
        // Get resource by it's ID.
        let resource = match self.resources.get_mut(&index) {
            Some(resource) => resource,
            None => return,
        };

        // Cast resource to TcpStreamWrap.
        resource.downcast_mut::<TcpStreamWrap>().unwrap().close();
    }
}

impl Default for EventLoop {
    fn default() -> Self {
        Self::new()
    }
}

impl std::ops::Drop for EventLoop {
    fn drop(&mut self) {
        self.thread_pool.join();
    }
}

#[derive(Clone)]
pub struct LoopHandle {
    index: Rc<Cell<Index>>,
    actions: Rc<mpsc::Sender<Action>>,
    actions_queue_empty: Rc<Cell<bool>>,
}

#[allow(dead_code)]
impl LoopHandle {
    /// Returns the next available resource index.
    pub fn index(&self) -> Index {
        let index = self.index.get();
        self.index.set(index + 1);
        index
    }

    /// Schedules a new timer to the event-loop.
    pub fn timer<F>(&self, delay: u64, repeat: bool, cb: F) -> Index
    where
        F: FnMut(LoopHandle) + 'static,
    {
        let index = self.index();
        let expires_at = Duration::from_millis(delay);

        let timer = TimerWrap {
            cb: Box::new(cb),
            expires_at,
            repeat,
        };

        self.actions.send(Action::TimerReq(index, timer)).unwrap();
        self.actions_queue_empty.set(false);

        index
    }

    /// Removes a scheduled timer from the event-loop.
    pub fn remove_timer(&self, index: &Index) {
        self.actions.send(Action::TimerDeleteReq(*index)).unwrap();
        self.actions_queue_empty.set(false);
    }

    /// Spawns a new task without blocking the main thread.
    pub fn spawn<F, U>(&self, task: F, task_cb: Option<U>) -> Index
    where
        F: FnOnce() -> TaskResult + Send + 'static,
        U: FnOnce(LoopHandle, TaskResult) + 'static,
    {
        let index = self.index();

        // Note: I tried to use `.and_then` instead of this ugly match statement but Rust complains
        // about mismatch types having no idea why.
        let task_cb: Option<Box<dyn FnOnce(LoopHandle, TaskResult)>> = match task_cb {
            Some(cb) => Some(Box::new(cb)),
            None => None,
        };

        let task_wrap = TaskWrap { inner: task_cb };

        self.actions
            .send(Action::SpawnReq(index, Box::new(task), task_wrap))
            .unwrap();

        self.actions_queue_empty.set(false);

        index
    }

    /// Creates a new TCP stream and issue a non-blocking connect to the specified address.
    pub fn tcp_connect<F>(&self, address: &str, on_connection: F) -> Result<Index>
    where
        F: FnOnce(LoopHandle, Index, Result<TcpSocketInfo>) + 'static,
    {
        // Create a SocketAddr from the provided string.
        let address: SocketAddr = address.parse()?;
        let index = self.index();

        // Connect the stream.
        let socket = TcpStream::connect(address)?;

        let stream = TcpStreamWrap {
            id: index,
            socket,
            on_connection: Some(Box::new(on_connection)),
            on_read: None,
            write_queue: LinkedList::new(),
        };

        self.actions
            .send(Action::TcpConnectionReq(index, stream))
            .unwrap();

        self.actions_queue_empty.set(false);

        Ok(index)
    }

    /// Starts listening for incoming connections.
    pub fn tcp_listen<F>(&self, host: &str, on_connection: F) -> Result<Index>
    where
        F: FnMut(LoopHandle, Index, Result<TcpSocketInfo>) + 'static,
    {
        // Create a SocketAddr from the provided host.
        let address: SocketAddr = host.parse()?;
        let index = self.index();

        // Bind address to the socket.
        let socket = TcpListener::bind(address)?;

        let listener = TcpListenerWrap {
            id: index,
            socket,
            on_connection: Box::new(on_connection),
        };

        self.actions
            .send(Action::TcpListenReq(index, listener))
            .unwrap();

        self.actions_queue_empty.set(false);

        Ok(index)
    }

    /// Writes bytes to an open TCP socket.
    pub fn tcp_write<F>(&self, index: Index, data: &[u8], on_write: F)
    where
        F: FnOnce(LoopHandle, Index, Result<usize>) + 'static,
    {
        self.actions
            .send(Action::TcpWriteReq(
                index,
                data.to_vec(),
                Box::new(on_write),
            ))
            .unwrap();

        self.actions_queue_empty.set(false);
    }

    /// Starts reading from an open socket.
    pub fn tcp_read_start<F>(&self, index: Index, on_read: F)
    where
        F: FnMut(LoopHandle, Index, Result<Vec<u8>>) + 'static,
    {
        self.actions
            .send(Action::TcpReadStartReq(index, Box::new(on_read)))
            .unwrap();

        self.actions_queue_empty.set(false);
    }

    /// Closes an open TCP socket.
    pub fn tcp_close<F>(&self, index: Index, on_close: F)
    where
        F: FnOnce(LoopHandle) + 'static,
    {
        self.actions
            .send(Action::TcpCloseReq(index, Box::new(on_close)))
            .unwrap();

        self.actions_queue_empty.set(false);
    }

    /// Closes the write side of the TCP stream.
    pub fn tcp_shutdown(&self, index: Index) {
        self.actions.send(Action::TcpShutdownReq(index)).unwrap();
        self.actions_queue_empty.set(false);
    }
}

pub struct LoopInterruptHandle {
    waker: Arc<Waker>,
}

impl LoopInterruptHandle {
    // Interrupts the poll phase of the event-loop.
    pub fn interrupt(&self) {
        self.waker.wake().unwrap();
    }
}

fn main() {
    let mut event_loop = EventLoop::new();
    let handle = event_loop.handle();

    // ============================================================================
    //
    // TIMERS EXAMPLE (uncomment to run)
    //
    // handle.timer(1000, false, |h: LoopHandle| {
    //     println!("Hello!");
    //     h.timer(2500, false, |_: LoopHandle| println!("Hello, world!"));
    // });
    //
    // ============================================================================
    //
    // FILE SYSTEM OPERATIONS EXAMPLE (uncomment to run)
    //
    // let read_file = || {
    //     let content = std::fs::read_to_string("./src/main.rs").unwrap();
    //     Some(Ok(content.as_bytes().to_vec()))
    // };

    // let read_file_cb = |_: LoopHandle, result: TaskResult| {
    //     let bytes = result.unwrap().unwrap();
    //     let content = std::str::from_utf8(&bytes).unwrap();
    //     println!("{}", content);
    // };

    // handle.spawn(read_file, Some(read_file_cb));
    //
    // ============================================================================
    //
    // TCP ECHO SERVER EXAMPLE (uncomment to run)
    //
    // let on_close = |_: LoopHandle| println!("Connection closed.");
    //
    // let on_write = |_: LoopHandle, _: Index, result: Result<usize>| {
    //     if let Err(e) = result {
    //         eprintln!("{}", e);
    //     }
    // };
    //
    // let on_read = move |h: LoopHandle, index: Index, data: Result<Vec<u8>>| {
    //     match data {
    //         Ok(data) if data.is_empty() => h.tcp_close(index, on_close),
    //         Ok(data) => h.tcp_write(index, &data, on_write),
    //         Err(e) => eprintln!("{}", e),
    //     };
    // };
    //
    // let on_new_connection =
    //     move |h: LoopHandle, index: Index, socket: Result<TcpSocketInfo>| match socket {
    //         Ok(_) => h.tcp_read_start(index, on_read),
    //         Err(e) => eprintln!("{}", e),
    //     };
    //
    // match handle.tcp_listen("127.0.0.1:9000", on_new_connection) {
    //     Ok(_) => println!("Server is listening on 127.0.0.1:9000"),
    //     Err(e) => eprintln!("{}", e),
    // };
    //
    // ============================================================================

    // TCP CLIENT EXAMPLE (uncomment to run)

    let on_close = |_: LoopHandle| println!("Connection closed.");

    let on_read = move |h: LoopHandle, index: Index, data: Result<Vec<u8>>| {
        match data {
            Ok(data) if data.is_empty() => h.tcp_close(index, on_close),
            Ok(data) => println!("{}", String::from_utf8(data).unwrap()),
            Err(err) => println!("ERROR: {}", err),
        };
    };

    let on_write = |_: LoopHandle, _: Index, _: Result<usize>| {};

    const HTTP_REQUEST: &str =
        "GET / HTTP/1.1\r\nHost: rssweather.com\r\nConnection: close\r\n\r\n";

    let on_connection =
        move |h: LoopHandle, index: Index, socket: Result<TcpSocketInfo>| match socket {
            Ok(_) => {
                h.tcp_read_start(index, on_read);
                h.tcp_write(index, HTTP_REQUEST.as_bytes(), on_write);
            }
            Err(e) => {
                eprintln!("{}", e);
                h.tcp_close(index, |_: LoopHandle| {});
            }
        };

    handle
        .tcp_connect("104.21.45.178:80", on_connection)
        .unwrap();

    while event_loop.has_pending_events() {
        event_loop.tick();
    }
}
