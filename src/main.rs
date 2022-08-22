use anyhow::Result;
use downcast_rs::impl_downcast;
use downcast_rs::Downcast;
use mio::net::TcpStream;
use mio::Events;
use mio::Interest;
use mio::Poll;
use mio::Registry;
use mio::Token;
use std::any::type_name;
use std::borrow::Cow;
use std::cell::Cell;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::LinkedList;
use std::io;
use std::io::Read;
use std::io::Write;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::mpsc;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;
use threadpool::ThreadPool;

type Index = u32;

type TaskResult = Option<Result<Vec<u8>>>;

/// All objects that are tracked by the event-loop should implement the `Resource` trait.
pub trait Resource: Downcast + 'static {
    /// Returns a string representation of the resource.
    fn name(&self) -> Cow<str> {
        type_name::<Self>().into()
    }
}

impl_downcast!(Resource);

struct TimerWrap {
    cb: Box<dyn FnMut(LoopHandle) + 'static>,
    expires_at: Duration,
    repeat: bool,
}

impl Resource for TimerWrap {}

struct TaskWrap {
    inner: Option<Box<dyn FnOnce(LoopHandle, TaskResult) + 'static>>,
}

impl Resource for TaskWrap {}

type TcpOnConnection = Box<dyn FnOnce(LoopHandle, Index, Result<TcpSocketInfo>) + 'static>;
type TcpOnWrite = Box<dyn FnOnce(LoopHandle, Index, Result<usize>) + 'static>;
type TcpOnRead = Box<dyn FnMut(LoopHandle, Index, Result<Vec<u8>>) + 'static>;

struct TcpStreamWrap {
    id: Index,
    socket: TcpStream,
    on_connection: Option<TcpOnConnection>,
    on_read: Option<TcpOnRead>,
    write_queue: LinkedList<(&'static [u8], TcpOnWrite)>,
}

impl Resource for TcpStreamWrap {}

pub struct TcpSocketInfo {
    id: Index,
    local_address: SocketAddr,
    remote_address: SocketAddr,
}

enum Action {
    NewTimer(Index, TimerWrap),
    RemoveTimer(Index),
    SpawnTask(Index, Box<dyn FnOnce() -> TaskResult + Send>, TaskWrap),
    NewTcpConnection(Index, TcpStreamWrap),
    NewTcpWriteRequest(Index, &'static [u8], TcpOnWrite),
    NewTcpReadStart(Index, TcpOnRead),
    CloseTcpSocket(Index, Box<dyn FnOnce(LoopHandle) + 'static>),
}

enum Event {
    Interrupt,
    ThreadPool(Index, TaskResult),
    Network(TcpEvent),
}

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
    thread_pool: ThreadPool,
    event_dispatcher: Arc<Mutex<mpsc::Sender<Event>>>,
    event_queue: mpsc::Receiver<Event>,
    pending_tasks: u32,
    network: Registry,
}

impl EventLoop {
    pub fn new() -> Self {
        let (action_dispatcher, action_queue) = mpsc::channel();
        let (event_dispatcher, event_queue) = mpsc::channel();

        // Wrap event_dispatcher into a Arc<Mutex>.
        let event_dispatcher = Arc::new(Mutex::new(event_dispatcher));

        // Create network handles.
        let poll = Poll::new().unwrap();
        let registry = poll.registry().try_clone().unwrap();

        // Start the network thread.
        Self::start_network_thread(poll, event_dispatcher.clone());

        EventLoop {
            index: Rc::new(Cell::new(1)),
            resources: HashMap::new(),
            timer_queue: BTreeMap::new(),
            action_queue,
            action_queue_empty: Rc::new(Cell::new(true)),
            action_dispatcher: Rc::new(action_dispatcher),
            thread_pool: ThreadPool::new(4),
            event_dispatcher,
            event_queue,
            pending_tasks: 0,
            network: registry,
        }
    }

    fn start_network_thread(mut poll: Poll, event_dispatcher: Arc<Mutex<mpsc::Sender<Event>>>) {
        std::thread::spawn(move || {
            // Create a MIO event store.
            let mut events = Events::with_capacity(1024);

            loop {
                // Poll for new network events (this will block the network thread).
                poll.poll(&mut events, None).unwrap();

                for event in &events {
                    // Signal the main thread for the new TCP event.
                    let event_type = match (
                        event.is_readable() || event.is_read_closed(),
                        event.is_writable(),
                    ) {
                        (true, false) => TcpEvent::Read(event.token().0 as u32),
                        (false, true) => TcpEvent::Write(event.token().0 as u32),
                        _ => continue,
                    };

                    event_dispatcher
                        .lock()
                        .unwrap()
                        .send(Event::Network(event_type))
                        .unwrap();
                }
            }
        });
    }

    pub fn handle(&self) -> LoopHandle {
        LoopHandle {
            index: self.index.clone(),
            actions: self.action_dispatcher.clone(),
            actions_queue_empty: self.action_queue_empty.clone(),
        }
    }

    pub fn interrupt_handle(&self) -> LoopInterruptHandle {
        LoopInterruptHandle {
            events: self.event_dispatcher.clone(),
        }
    }

    pub fn has_pending_events(&self) -> bool {
        !(self.resources.is_empty() && self.action_queue_empty.get())
    }

    pub fn tick(&mut self) {
        self.prepare();
        self.run_timers();
        self.run_poll();
    }

    fn prepare(&mut self) {
        while let Ok(action) = self.action_queue.try_recv() {
            match action {
                Action::NewTimer(index, timer) => self.ev_new_timer(index, timer),
                Action::RemoveTimer(index) => self.ev_remove_timer(&index),
                Action::SpawnTask(index, task, t_wrap) => self.ev_spawn_task(index, task, t_wrap),
                Action::NewTcpConnection(index, tc_wrap) => {
                    self.ev_new_tcp_connection(index, tc_wrap)
                }
                Action::NewTcpWriteRequest(index, data, on_write) => {
                    self.ev_new_tcp_write_request(index, data, on_write)
                }
                Action::NewTcpReadStart(index, on_read) => {
                    self.ev_new_tcp_read_start(index, on_read)
                }
                Action::CloseTcpSocket(index, on_close) => {
                    self.ev_close_tcp_socket(index, on_close)
                }
            };
        }
        self.action_queue_empty.set(true);
    }

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

    fn run_poll(&mut self) {
        // Based on what resources the event-loop is currently running will decide
        // how long we should wait on the this phase.
        let timeout = match self.timer_queue.iter().next() {
            Some((t, _)) => *t - Instant::now(),
            None if self.pending_tasks > 0 => Duration::MAX,
            None => Duration::ZERO,
        };

        if let Ok(event) = self.event_queue.recv_timeout(timeout) {
            match event {
                Event::Interrupt => return,
                Event::ThreadPool(index, result) => self.run_task_callback(index, result),
                Event::Network(tcp_event) => match tcp_event {
                    TcpEvent::Write(index) => self.run_tcp_socket_write(index),
                    TcpEvent::Read(index) => self.run_tcp_socket_read(index),
                },
            }
            self.prepare();
        }
    }

    fn run_task_callback(&mut self, index: Index, result: TaskResult) {
        if let Some(mut resource) = self.resources.remove(&index) {
            let task_wrap = resource.downcast_mut::<TaskWrap>().unwrap();
            let callback = task_wrap.inner.take().unwrap();
            (callback)(self.handle(), result);

            self.pending_tasks -= 1;
        }
    }

    fn run_tcp_socket_write(&mut self, index: Index) {
        // Create a new handle.
        let handle = self.handle();

        // Try to get a reference to the resource.
        let resource = match self.resources.get_mut(&index) {
            Some(resource) => resource,
            None => return,
        };

        // Cast resource to TcpStreamWrap.
        let tcp_wrap = resource.downcast_mut::<TcpStreamWrap>().unwrap();

        // If we hit an error while connecting return that error.
        if let Ok(Some(err)) | Err(err) = tcp_wrap.socket.take_error() {
            // Run on_connection callback.
            let on_connection = tcp_wrap.on_connection.take().unwrap();
            (on_connection)(handle, index, Result::Err(err.into()));

            self.resources.remove(&index).unwrap();
            return;
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
                    local_address: tcp_wrap.socket.local_addr().unwrap(),
                    remote_address: tcp_wrap.socket.peer_addr().unwrap(),
                }),
            );

            let token = Token(index as usize);

            self.network
                .reregister(&mut tcp_wrap.socket, token, Interest::READABLE)
                .unwrap();

            return;
        }

        // Connection is OK, let's write some bytes...

        if !tcp_wrap.write_queue.is_empty() {
            // Get data and on_write callback.
            let (data, on_write) = tcp_wrap.write_queue.pop_front().unwrap();

            // Try write some bytes to the socket.
            match tcp_wrap.socket.write(data) {
                Ok(bytes_written) => (on_write)(handle, index, Result::Ok(bytes_written)),
                Err(err) => (on_write)(handle, index, Result::Err(err.into())),
            };
        }
    }

    fn run_tcp_socket_read(&mut self, index: Index) {
        // Create a new handle.
        let handle = self.handle();

        // Try to get a reference to the resource.
        let resource = match self.resources.get_mut(&index) {
            Some(resource) => resource,
            None => return,
        };

        // Cast resource to TcpStreamWrap.
        let tcp_wrap = resource.downcast_mut::<TcpStreamWrap>().unwrap();

        // Prepare the read buffer.
        let mut buffer = vec![0; 4096];
        let mut bytes_read = 0;

        // This will help us catch errors inside the loop.
        let mut read_error: Option<io::Error> = None;

        // We can (maybe) read from the connection.
        loop {
            match tcp_wrap.socket.read(&mut buffer[bytes_read..]) {
                // Reading 0 bytes means the other side has closed the
                // connection or is done writing.
                Ok(0) => break,
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

        // Resize the buffer.
        buffer.resize(bytes_read, 0);

        (on_read)(handle, index, Result::Ok(buffer));
    }

    fn ev_new_timer(&mut self, index: Index, timer: TimerWrap) {
        let time_key = Instant::now() + timer.expires_at;
        self.resources.insert(index, Box::new(timer));
        self.timer_queue.insert(time_key, index);
    }

    fn ev_remove_timer(&mut self, index: &Index) {
        self.resources.remove(index);
        self.timer_queue.retain(|_, v| *v != *index);
    }

    fn ev_spawn_task(
        &mut self,
        index: Index,
        task: Box<dyn FnOnce() -> TaskResult + Send>,
        task_wrap: TaskWrap,
    ) {
        let notifier = self.event_dispatcher.clone();

        if task_wrap.inner.is_some() {
            self.resources.insert(index, Box::new(task_wrap));
        }

        self.thread_pool.execute(move || {
            let result = (task)();
            let notifier = notifier.lock().unwrap();

            notifier.send(Event::ThreadPool(index, result)).unwrap();
        });

        self.pending_tasks += 1;
    }

    fn ev_new_tcp_connection(&mut self, index: Index, mut tcp_wrap: TcpStreamWrap) {
        // When we create a new TCP socket connection we have to make sure
        // it's well connected with the remote host.
        //
        // See https://docs.rs/mio/0.8.4/mio/net/struct.TcpStream.html#notes
        let socket = &mut tcp_wrap.socket;
        let token = Token(tcp_wrap.id as usize);

        self.network
            .register(socket, token, Interest::WRITABLE)
            .unwrap();

        self.resources.insert(index, Box::new(tcp_wrap));
    }

    fn ev_new_tcp_write_request(
        &mut self,
        index: Index,
        data: &'static [u8],
        on_write: TcpOnWrite,
    ) {
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

        self.network
            .reregister(&mut tcp_wrap.socket, token, interest)
            .unwrap();
    }

    fn ev_new_tcp_read_start(&mut self, index: Index, on_read: TcpOnRead) {
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

        self.network
            .reregister(&mut tcp_wrap.socket, token, interest)
            .unwrap();
    }

    fn ev_close_tcp_socket(
        &mut self,
        index: Index,
        on_close: Box<dyn FnOnce(LoopHandle) + 'static>,
    ) {
        // Remove resource from the event-loop.
        self.resources.remove(&index).unwrap();

        // Run on_close callback
        (on_close)(self.handle());
    }
}

impl Default for EventLoop {
    fn default() -> Self {
        Self::new()
    }
}

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

        self.actions.send(Action::NewTimer(index, timer)).unwrap();
        self.actions_queue_empty.set(false);

        index
    }

    /// Removes a scheduled timer from the event-loop.
    pub fn remove_timer(&self, index: &Index) {
        self.actions.send(Action::RemoveTimer(*index)).unwrap();
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
            .send(Action::SpawnTask(index, Box::new(task), task_wrap))
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
            .send(Action::NewTcpConnection(index, stream))
            .unwrap();

        self.actions_queue_empty.set(false);

        Ok(index)
    }

    /// Writes bytes to an open TCP socket.
    pub fn tcp_write<F>(&self, index: Index, data: &'static [u8], on_write: F)
    where
        F: FnOnce(LoopHandle, Index, Result<usize>) + 'static,
    {
        self.actions
            .send(Action::NewTcpWriteRequest(index, data, Box::new(on_write)))
            .unwrap();

        self.actions_queue_empty.set(false);
    }

    /// Starts reading from an open socket.
    pub fn tcp_read_start<F>(&self, index: Index, on_read: F)
    where
        F: FnMut(LoopHandle, Index, Result<Vec<u8>>) + 'static,
    {
        self.actions
            .send(Action::NewTcpReadStart(index, Box::new(on_read)))
            .unwrap();

        self.actions_queue_empty.set(false);
    }

    /// Closes an open TCP socket.
    pub fn tcp_close<F>(&self, index: Index, on_close: F)
    where
        F: FnOnce(LoopHandle) + 'static,
    {
        self.actions
            .send(Action::CloseTcpSocket(index, Box::new(on_close)))
            .unwrap();

        self.actions_queue_empty.set(false);
    }
}

pub struct LoopInterruptHandle {
    events: Arc<Mutex<mpsc::Sender<Event>>>,
}

impl LoopInterruptHandle {
    // Interrupts the poll phase of the event-loop.
    pub fn interrupt(&self) {
        self.events.lock().unwrap().send(Event::Interrupt).unwrap();
    }
}

fn main() {
    let mut event_loop = EventLoop::new();
    let handle = event_loop.handle();

    // let read_file = || {
    //     let content = std::fs::read_to_string("./src/main.rs").unwrap();
    //     Some(Ok(content.as_bytes().to_vec()))
    // };

    // let read_file_cb = |_: LoopHandle, result: TaskResult| {
    //     let bytes = result.unwrap().unwrap();
    //     let content = std::str::from_utf8(&bytes).unwrap();
    //     println!("{}", content);
    // };

    // handle.timer(1000, false, |h: LoopHandle| {
    //     println!("Hello!");
    //     h.timer(2500, false, |_: LoopHandle| println!("Hello, world!"));
    // });

    // handle.spawn(read_file, Some(read_file_cb));

    let on_close = |_: LoopHandle| println!("Socket closed!");

    let on_read = move |h: LoopHandle, index: Index, data: Result<Vec<u8>>| {
        match data {
            Ok(data) if data.is_empty() => h.tcp_close(index, on_close),
            Ok(data) => println!("{}", String::from_utf8(data).unwrap()),
            Err(err) => println!("ERROR: {}", err),
        };
    };

    let on_write = |_: LoopHandle, __: Index, ___: Result<usize>| {};

    let on_connection =
        move |h: LoopHandle, index: Index, socket: Result<TcpSocketInfo>| match socket {
            Ok(_) => {
                println!("Connection established!!");
                h.tcp_read_start(index, on_read);
                h.tcp_write(index, "Hello!".as_bytes(), on_write)
            }
            Err(err) => println!("ERROR: {}", err),
        };

    handle.tcp_connect("127.0.0.1:3000", on_connection).unwrap();

    while event_loop.has_pending_events() {
        event_loop.tick();
    }
}
