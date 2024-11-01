use anyhow::anyhow;
use anyhow::bail;
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
pub use notify::Event as FsEvent;
pub use notify::EventKind as FsEventKind;
use notify::RecommendedWatcher;
use notify::RecursiveMode;
use notify::Watcher;
use rayon::ThreadPool;
use rayon::ThreadPoolBuilder;
pub use signal_hook::consts::signal as Signal;
use signal_hook::low_level::emulate_default_handler;
use std::cell::Cell;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::LinkedList;
use std::io;
use std::io::Read;
use std::io::Write;
use std::net::Shutdown;
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::path::Path;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc;
use std::sync::Arc;
use std::sync::Mutex;
use std::thread;
use std::time::Duration;
use std::time::Instant;

#[cfg(target_family = "unix")]
use signal_hook_mio::v0_8::Signals;

/// Wrapper type for resource identification.
pub type Index = u32;

/// All objects that are tracked by the event-loop should implement the `Resource` trait.
trait Resource: Downcast + 'static {
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

type OneShotCallback = dyn FnOnce(LoopHandle) + 'static;

// Wrapper types for the task resource.
type Task = Box<dyn FnOnce() -> TaskResult + Send>;
type TaskOnFinish = Box<dyn FnOnce(LoopHandle, TaskResult) + 'static>;
pub type TaskResult = Option<Result<Vec<u8>>>;

// Wrapper types for different TCP callbacks.
type TcpOnConnection = Box<dyn FnOnce(LoopHandle, Index, Result<TcpSocketInfo>) + 'static>;
type TcpListenerOnConnection = Box<dyn FnMut(LoopHandle, Index, Result<TcpSocketInfo>) + 'static>;
type TcpOnWrite = Box<dyn FnOnce(LoopHandle, Index, Result<usize>) + 'static>;
type TcpOnRead = Box<dyn FnMut(LoopHandle, Index, Result<Vec<u8>>) + 'static>;

// Wrapper around fs events callbacks.
type FsWatchOnEvent = Box<dyn FnMut(LoopHandle, FsEvent) + 'static>;

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
    pub id: Index,
    pub host: SocketAddr,
    pub remote: SocketAddr,
}

/// Describes a callback that will run once after the Poll phase.
struct CheckWrap {
    cb: Option<Box<OneShotCallback>>,
}

impl Resource for CheckWrap {}

/// Describes a file-system watcher.
struct FsWatcherWrap {
    inner: Option<RecommendedWatcher>,
    on_event: Option<FsWatchOnEvent>,
    path: PathBuf,
    recursive: bool,
}

impl Resource for FsWatcherWrap {}

// Based on linux systems the max allowed signal number is 31.
// https://www-uxsup.csx.cam.ac.uk/courses/moved.Building/signals.pdf
const MAX_SIGNAL_VALUE: i32 = 31;

#[derive(Debug)]
struct SignalNum(i32);

impl SignalNum {
    // Note: This allows us to have signal validation on the type level
    // instead in the logic when used.
    fn parse(signal: i32) -> Result<Self> {
        // Check if signal is within UNIX range.
        if signal > MAX_SIGNAL_VALUE {
            bail!("Signal number out of range.");
        }
        // Forbidden signals are not allowed to be registered.
        if signal_hook::consts::FORBIDDEN.contains(&signal) {
            bail!("Forbidden signal provided.");
        }

        Ok(SignalNum(signal))
    }

    fn to_i32(&self) -> i32 {
        self.0
    }
}

struct SignalWrap {
    id: Index,
    on_signal: SignalHandler,
    oneshot: bool,
}

type SignalHandler = Box<dyn FnMut(LoopHandle, i32) + 'static>;

/// Abstracts signal handling across platforms.
struct OsSignals {
    #[cfg(target_family = "unix")]
    sources: Signals,
    handlers: HashMap<i32, Vec<SignalWrap>>,
}

impl OsSignals {
    #[cfg(target_family = "unix")]
    fn new(registry: &Registry) -> Self {
        let mut sources = Signals::new::<[_; 0], i32>([]).unwrap();
        let _ = registry.register(&mut sources, Token(1), Interest::READABLE);

        OsSignals {
            sources,
            handlers: HashMap::new(),
        }
    }

    #[cfg(target_family = "windows")]
    fn new(notifier: Arc<Mutex<mpsc::Sender<Event>>>, waker: Arc<Waker>) -> Self {
        // Spawn signal watching thread.
        let on_signal_handler = move || {
            notifier.lock().unwrap().send(Event::WinSigInt).unwrap();
            waker.wake().unwrap();
        };

        ctrlc::set_handler(on_signal_handler).unwrap();

        OsSignals {
            handlers: HashMap::new(),
        }
    }

    #[cfg(target_family = "unix")]
    fn run_pending(&mut self, handle: LoopHandle) {
        // Going through the available signals.
        for signal in self.sources.pending() {
            // Get all handlers for the given signal.
            let handlers = match self.handlers.get_mut(&signal) {
                Some(handlers) => handlers,
                None => continue,
            };

            // No listeners for this signal, running default action.
            if handlers.is_empty() {
                emulate_default_handler(signal).unwrap();
                continue;
            }

            handlers.retain_mut(|handler| {
                // Run handler's callback.
                (handler.on_signal)(handle.clone(), signal);
                // Keep the listener if it's not a oneshot.
                !handler.oneshot
            });
        }
    }

    #[cfg(target_family = "windows")]
    fn run_pending(&mut self, handle: LoopHandle) {
        // Note: In Windows, a dedicated thread is always on standby to listen for
        // CTRL+C signals. Consequently, this function may be activated even if a
        // signal handler was never initiated. Therefore, it's necessary to mimic
        // the default action when no signals are registered or if the list of
        // handlers is currently empty.
        let handlers = match self.handlers.get_mut(&Signal::SIGINT) {
            Some(handlers) if !handlers.is_empty() => handlers,
            _ => {
                emulate_default_handler(Signal::SIGINT).unwrap();
                return;
            }
        };

        handlers.retain_mut(|handler| {
            // Run handler's callback.
            (handler.on_signal)(handle.clone(), Signal::SIGINT);
            // Keep the listener if it's not a oneshot.
            !handler.oneshot
        });
    }

    fn remove_handler(&mut self, index: Index) {
        // Note: Given the structure of this struct we're following the simplest
        // approach to remove the element with O(n^2) complexity. Further
        // improvements can be made in the future if necessary.
        if let Some((_, list)) = self
            .handlers
            .iter_mut()
            .find(|(_, list)| list.iter().any(|handler| handler.id == index))
        {
            list.retain(|handler| handler.id != index);
        }
    }
}

#[allow(clippy::enum_variant_names)]
enum Action {
    TimerReq(Index, TimerWrap),
    TimerRemoveReq(Index),
    SpawnReq(Index, Task, TaskWrap),
    TcpConnectionReq(Index, TcpStreamWrap),
    TcpListenReq(Index, TcpListenerWrap),
    TcpWriteReq(Index, Vec<u8>, TcpOnWrite),
    TcpReadStartReq(Index, TcpOnRead),
    TcpCloseReq(Index, Box<OneShotCallback>),
    TcpShutdownReq(Index, Box<OneShotCallback>),
    CheckReq(Index, CheckWrap),
    CheckRemoveReq(Index),
    FsEventStartReq(Index, FsWatcherWrap),
    FsEventStopReq(Index),
    SignalStartReq(SignalNum, SignalWrap),
    SignalStopReq(Index),
}

#[allow(dead_code)]
enum Event {
    /// A thread-pool task has been completed.
    ThreadPool(Index, TaskResult),
    /// A network operation is available.
    Network(TcpEvent),
    /// A file-system change has been detected.
    Watch(Index, FsEvent),
    /// An interrupt signal detected (Windows platform).
    WinSigInt,
}

#[derive(Debug)]
enum TcpEvent {
    /// Socket is (probably) ready for reading.
    Read(Index),
    /// Socket is (probably) ready for writing.
    Write(Index),
}

/// An instance that knows how to handle fs events.
struct FsEventHandler {
    id: Index,
    waker: Arc<Waker>,
    sender: Arc<Mutex<mpsc::Sender<Event>>>,
}

impl notify::EventHandler for FsEventHandler {
    /// Handles an event.
    fn handle_event(&mut self, event: notify::Result<FsEvent>) {
        // Notify the main thread about this fs event.
        let event = Event::Watch(self.id, event.unwrap());

        self.sender.lock().unwrap().send(event).unwrap();
        self.waker.wake().unwrap();
    }
}

pub struct EventLoop {
    index: Rc<Cell<Index>>,
    resources: HashMap<Index, Box<dyn Resource>>,
    timer_queue: BTreeMap<Instant, Index>,
    action_queue: mpsc::Receiver<Action>,
    action_queue_empty: Rc<Cell<bool>>,
    action_dispatcher: Rc<mpsc::Sender<Action>>,
    check_queue: Vec<Index>,
    close_queue: Vec<(Index, Option<Box<OneShotCallback>>)>,
    thread_pool: ThreadPool,
    thread_pool_tasks: usize,
    event_dispatcher: Arc<Mutex<mpsc::Sender<Event>>>,
    event_queue: mpsc::Receiver<Event>,
    network_events: Registry,
    signals: OsSignals,
    poll: Poll,
    waker: Arc<Waker>,
}

//---------------------------------------------------------
//  PUBLICLY EXPOSED METHODS.
//---------------------------------------------------------

impl EventLoop {
    /// Creates a new event-loop instance.
    pub fn new(num_threads: usize) -> Self {
        // Number of threads should always be a positive non-zero number.
        assert!(num_threads > 0);

        let (action_dispatcher, action_queue) = mpsc::channel();
        let (event_dispatcher, event_queue) = mpsc::channel();

        let thread_pool = ThreadPoolBuilder::new()
            .num_threads(num_threads)
            .build()
            .unwrap();

        let event_dispatcher = Arc::new(Mutex::new(event_dispatcher));

        // Create network handles.
        let poll = Poll::new().unwrap();
        let registry = poll.registry().try_clone().unwrap();
        let waker = Waker::new(poll.registry(), Token(0)).unwrap();
        let waker = Arc::new(waker);

        // Monitor system signals through dedicated thread.
        #[cfg(target_family = "windows")]
        let signals = OsSignals::new(event_dispatcher.clone(), waker.clone());

        // Monitor system signals through MIO.
        #[cfg(target_family = "unix")]
        let signals = OsSignals::new(&registry);

        EventLoop {
            index: Rc::new(Cell::new(2)),
            resources: HashMap::new(),
            timer_queue: BTreeMap::new(),
            action_queue,
            action_queue_empty: Rc::new(Cell::new(true)),
            action_dispatcher: Rc::new(action_dispatcher),
            check_queue: Vec::new(),
            close_queue: Vec::new(),
            thread_pool,
            thread_pool_tasks: 0,
            event_dispatcher,
            event_queue,
            poll,
            network_events: registry,
            waker,
            signals,
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
        !(self.resources.is_empty() && self.action_queue_empty.get() && self.thread_pool_tasks == 0)
    }

    /// Performs a single tick of the event-loop.
    pub fn tick(&mut self) {
        self.prepare();
        self.run_timers();
        self.run_poll();
        self.run_check();
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
                Action::TimerRemoveReq(index) => self.timer_remove_req(index),
                Action::SpawnReq(index, task, t_wrap) => self.spawn_req(index, task, t_wrap),
                Action::TcpConnectionReq(index, tc_wrap) => self.tcp_connection_req(index, tc_wrap),
                Action::TcpListenReq(index, tc_wrap) => self.tcp_listen_req(index, tc_wrap),
                Action::TcpWriteReq(index, data, cb) => self.tcp_write_req(index, data, cb),
                Action::TcpReadStartReq(index, cb) => self.tcp_read_start_req(index, cb),
                Action::TcpCloseReq(index, cb) => self.tcp_close_req(index, cb),
                Action::TcpShutdownReq(index, cb) => self.tcp_shutdown_req(index, cb),
                Action::CheckReq(index, cb) => self.check_req(index, cb),
                Action::CheckRemoveReq(index) => self.check_remove_req(index),
                Action::FsEventStartReq(index, w_wrap) => self.fs_event_start_req(index, w_wrap),
                Action::FsEventStopReq(index) => self.fs_event_stop_req(index),
                Action::SignalStartReq(signum, s_wrap) => self.signal_start_req(signum, s_wrap),
                Action::SignalStopReq(index) => self.signal_stop_req(index),
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

                // If the timer is repeatable reschedule it, otherwise drop it.
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
        // Based on what resources the event-loop is currently running will
        // decide how long we should wait on the this phase.
        let timeout = if self.has_pending_events() {
            let refs = self.check_queue.len() + self.close_queue.len();
            match self.timer_queue.iter().next() {
                _ if refs > 0 => Some(Duration::ZERO),
                Some((t, _)) => Some(*t - Instant::now()),
                None => None,
            }
        } else {
            Some(Duration::ZERO)
        };

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

            // Note: Token(1) is another special token signaling an OS
            // signal is available for processing.
            if event.token() == Token(1) {
                self.signals.run_pending(self.handle());
                continue;
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
                Event::Watch(index, event) => self.fs_event(index, event),
                Event::Network(tcp_event) => match tcp_event {
                    TcpEvent::Write(index) => self.tcp_socket_write(index),
                    TcpEvent::Read(index) => self.tcp_socket_read(index),
                },
                Event::WinSigInt => self.signals.run_pending(self.handle()),
            }
            self.prepare();
        }
    }

    /// Runs all check callbacks.
    fn run_check(&mut self) {
        // Create a new event-loop handle.
        let handle = self.handle();

        for rid in self.check_queue.drain(..) {
            // Remove resource from the event-loop.
            let mut resource = match self.resources.remove(&rid) {
                Some(resource) => resource,
                None => continue,
            };

            if let Some(cb) = resource
                .downcast_mut::<CheckWrap>()
                .map(|wrap| wrap.cb.take().unwrap())
            {
                // Run callback.
                (cb)(handle.clone());
            }
        }
        self.prepare();
    }

    /// Cleans up `dying` resources.
    fn run_close(&mut self) {
        // Create a new event-loop handle.
        let handle = self.handle();

        // Clean up resources.
        for (rid, on_close) in self.close_queue.drain(..) {
            if let Some(mut resource) = self.resources.remove(&rid) {
                resource.close();
                if let Some(cb) = on_close {
                    (cb)(handle.clone());
                }
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
        }
        self.thread_pool_tasks -= 1;
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
        if let Ok(Some(e)) | Err(e) = tcp_wrap.socket.take_error() {
            // If `on_connection` is available it means the socket error happened
            // while trying to connect.
            if let Some(on_connection) = tcp_wrap.on_connection.take() {
                (on_connection)(handle, index, Result::Err(e.into()));
                return;
            }
            // Otherwise the error happened while writing.
            if let Some((_, on_write)) = tcp_wrap.write_queue.pop_front() {
                (on_write)(handle, index, Result::Err(e.into()));
                return;
            }
        }

        // Note: If the on_connection callback is None it means that in some
        // previous iteration we made sure the TCP socket is well connected
        // with the remote host.

        if let Some(on_connection) = tcp_wrap.on_connection.take() {
            // Run socket's on_connection callback.
            (on_connection)(
                handle.clone(),
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
        }

        loop {
            // Due to loop ownership issues we need to clone the handle.
            let handle = handle.clone();

            // Connection is OK, let's write some bytes...
            let (data, on_write) = match tcp_wrap.write_queue.pop_front() {
                Some(value) => value,
                None => break,
            };

            match tcp_wrap.socket.write(&data) {
                // We want to write the entire `data` buffer in a single go. If we
                // write less we'll return a short write error (same as
                // `io::Write::write_all` does).
                Ok(n) if n < data.len() => {
                    let err_message = io::ErrorKind::WriteZero.to_string();
                    (on_write)(handle, index, Result::Err(anyhow!("{}", err_message)));
                }
                // All bytes were written to socket.
                Ok(n) => (on_write)(handle, index, Result::Ok(n)),
                // Would block "errors" are the OS's way of saying that the
                // connection is not actually ready to perform this I/O operation.
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    // Since we couldn't send this data we need to put it
                    // back into the write_queue.
                    tcp_wrap.write_queue.push_front((data, on_write));
                    break;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                // An important error seems to have accrued.
                Err(e) => (on_write)(handle, index, Result::Err(e.into())),
            };
        }

        // Unregister write interest if the write_queue is empty.
        if tcp_wrap.write_queue.is_empty() {
            let token = Token(tcp_wrap.id as usize);
            self.network_events
                .reregister(&mut tcp_wrap.socket, token, Interest::READABLE)
                .unwrap();
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

        let mut data = vec![];
        let mut data_buf = [0; 4096];

        // This will help us catch errors and FIN packets.
        let mut read_error: Option<io::Error> = None;
        let mut connection_closed = false;

        // We can (maybe) read from the connection.
        loop {
            match tcp_wrap.socket.read(&mut data_buf) {
                // Reading 0 bytes means the other side has closed the
                // connection or is done writing.
                Ok(0) => {
                    connection_closed = true;
                    break;
                }
                Ok(n) => data.extend_from_slice(&data_buf[..n]),
                // Would block "errors" are the OS's way of saying that the
                // connection is not actually ready to perform this I/O operation.
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                // Other errors we'll be considered fatal.
                Err(e) => read_error = Some(e),
            }
        }

        // Note: If a FIN packet received without us listening on the TCP stream, it means
        // that the other side closed the connection so we'll schedule the resource
        // for removal.

        let on_read = match tcp_wrap.on_read.as_mut() {
            Some(on_read) => on_read,
            None if !connection_closed => return,
            None => {
                // Deregister any interest on that socket.
                self.network_events
                    .deregister(&mut tcp_wrap.socket)
                    .unwrap();
                // Schedule resource clean-up.
                self.close_queue.push((index, None));
                return;
            }
        };

        // Check if we had any errors while reading.
        if let Some(e) = read_error {
            // Run on_read callback.
            (on_read)(handle, index, Result::Err(e.into()));
            return;
        }

        match data.len() {
            // FIN packet.
            0 => (on_read)(handle, index, Result::Ok(data)),
            // We read some bytes.
            _ if !connection_closed => (on_read)(handle, index, Result::Ok(data)),
            // FIN packet is included to the bytes we read.
            _ => {
                (on_read)(handle.clone(), index, Result::Ok(data));
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
        let mut new_resources = vec![];

        loop {
            // Create a new handle.
            let handle = handle.clone();

            // Received an event for the TCP server socket, which indicates we can accept a connections.
            let (socket, _) = match tcp_wrap.socket.accept() {
                Ok(sock) => sock,
                // If we get a `WouldBlock` error we know our
                // listener has no more incoming connections queued,
                // so we can return to polling and wait for some
                // more.
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    (on_connection)(handle, index, Result::Err(e.into()));
                    break;
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
                    id,
                    host: stream.socket.local_addr().unwrap(),
                    remote: stream.socket.peer_addr().unwrap(),
                }),
            );

            // Initialize socket with a READABLE event.
            self.network_events
                .register(&mut stream.socket, Token(id as usize), Interest::READABLE)
                .unwrap();

            new_resources.push((id, Box::new(stream)));
        }

        // Register the new TCP streams to the event-loop.
        for (id, stream) in new_resources.drain(..) {
            self.resources.insert(id, stream);
        }
    }

    /// Runs callback referring to specific fs event.
    fn fs_event(&mut self, index: Index, event: FsEvent) {
        // Try to get a reference to the resource.
        let handle = self.handle();
        let resource = match self.resources.get_mut(&index) {
            Some(resource) => resource,
            None => return,
        };

        // Get a mut reference to the callback.
        let on_event = match resource.downcast_mut::<FsWatcherWrap>() {
            Some(w_wrap) => w_wrap.on_event.as_mut().unwrap(),
            None => return,
        };

        // Run watcher's cb.
        (on_event)(handle, event);
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
    fn timer_remove_req(&mut self, index: Index) {
        self.resources.remove(&index);
        self.timer_queue.retain(|_, v| *v != index);
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

        self.thread_pool.spawn({
            let waker = self.waker.clone();
            move || {
                let result = (task)();
                let notifier = notifier.lock().unwrap();

                notifier.send(Event::ThreadPool(index, result)).unwrap();
                waker.wake().unwrap();
            }
        });

        self.thread_pool_tasks += 1;
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

        let interest = Interest::READABLE.add(Interest::WRITABLE);

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
            _ => Interest::READABLE.add(Interest::WRITABLE),
        };

        self.network_events
            .reregister(&mut tcp_wrap.socket, token, interest)
            .unwrap();
    }

    /// Schedules a TCP socket shutdown.
    fn tcp_close_req(&mut self, index: Index, on_close: Box<dyn FnOnce(LoopHandle) + 'static>) {
        // Schedule resource for graceful shutdown and removal.
        self.close_queue.push((index, Some(on_close)));
    }

    /// Closes the write side of the stream.
    fn tcp_shutdown_req(
        &mut self,
        index: Index,
        on_shutdown: Box<dyn FnOnce(LoopHandle) + 'static>,
    ) {
        // Get resource by it's ID.
        let resource = match self.resources.get_mut(&index) {
            Some(resource) => resource,
            None => return,
        };

        // Cast resource to TcpStreamWrap.
        resource.downcast_mut::<TcpStreamWrap>().unwrap().close();
        on_shutdown(self.handle())
    }

    /// Schedules a new check callback.
    fn check_req(&mut self, index: Index, check_wrap: CheckWrap) {
        // Add the check_wrap to the event loop.
        self.resources.insert(index, Box::new(check_wrap));
        self.check_queue.push(index);
    }

    /// Removes a check callback from the event-loop.
    fn check_remove_req(&mut self, index: Index) {
        self.resources.remove(&index);
        self.check_queue.retain(|v| *v != index);
    }

    /// Subscribes a new fs watcher to the event-loop.
    fn fs_event_start_req(&mut self, index: Index, mut wrap: FsWatcherWrap) {
        // Create an appropriate watcher for the current system.
        let mut watcher = RecommendedWatcher::new(
            FsEventHandler {
                waker: self.waker.clone(),
                sender: self.event_dispatcher.clone(),
                id: index,
            },
            notify::Config::default(),
        )
        .unwrap();

        let recursive_mode = match wrap.recursive {
            true => RecursiveMode::Recursive,
            _ => RecursiveMode::NonRecursive,
        };

        // Start watching requested path(s).
        watcher.watch(&wrap.path, recursive_mode).unwrap();

        wrap.inner = Some(watcher);
        self.resources.insert(index, Box::new(wrap));
    }

    /// Stops an fs watcher and removes it from the event-loop.
    fn fs_event_stop_req(&mut self, index: Index) {
        self.resources.remove(&index);
    }

    /// Subscribes a new signal listener to the event-loop.
    fn signal_start_req(&mut self, signum: SignalNum, signal_wrap: SignalWrap) {
        // Check if the specific signal is already being tracked.
        let signum = signum.to_i32();
        if let Some(handlers) = self.signals.handlers.get_mut(&signum) {
            handlers.push(signal_wrap);
            return;
        }

        #[cfg(target_family = "unix")]
        {
            self.signals.sources.add_signal(signum).unwrap();
        }
        self.signals.handlers.insert(signum, vec![signal_wrap]);
    }

    /// Removes the signal listener from the event-loop.
    fn signal_stop_req(&mut self, index: Index) {
        self.signals.remove_handler(index)
    }
}

impl Default for EventLoop {
    fn default() -> Self {
        let default_pool_size = unsafe { NonZeroUsize::new_unchecked(4) };
        let num_cores = thread::available_parallelism().unwrap_or(default_pool_size);

        Self::new(num_cores.into())
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
        self.actions.send(Action::TimerRemoveReq(*index)).unwrap();
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
    pub fn tcp_shutdown<F>(&self, index: Index, on_shutdown: F)
    where
        F: FnOnce(LoopHandle) + 'static,
    {
        self.actions
            .send(Action::TcpShutdownReq(index, Box::new(on_shutdown)))
            .unwrap();

        self.actions_queue_empty.set(false);
    }

    /// Schedules a new check callback.
    pub fn check<F>(&self, on_check: F) -> Index
    where
        F: FnOnce(LoopHandle) + 'static,
    {
        let index = self.index();
        let on_check = Box::new(on_check);

        self.actions
            .send(Action::CheckReq(index, CheckWrap { cb: Some(on_check) }))
            .unwrap();

        self.actions_queue_empty.set(false);

        index
    }

    /// Removes a check callback from the event-loop.
    pub fn remove_check(&self, index: &Index) {
        self.actions.send(Action::CheckRemoveReq(*index)).unwrap();
        self.actions_queue_empty.set(false);
    }

    /// Creates a watcher that will watch the specified path for changes.
    pub fn fs_event_start<F, P>(&self, path: P, recursive: bool, on_event: F) -> Result<Index>
    where
        F: FnMut(LoopHandle, FsEvent) + 'static,
        P: AsRef<Path>,
    {
        let index = self.index();
        let on_event = Box::new(on_event);

        // Check if path exists.
        std::fs::metadata(path.as_ref())?;

        // Note: We don't have access to internal mpsc channels so will
        // create the watcher at a later stage.
        let watcher_wrap = FsWatcherWrap {
            inner: None,
            on_event: Some(on_event),
            path: path.as_ref().to_path_buf(),
            recursive,
        };

        self.actions
            .send(Action::FsEventStartReq(index, watcher_wrap))
            .unwrap();

        self.actions_queue_empty.set(false);

        Ok(index)
    }

    /// Stops watch handle, the callback will no longer be called.
    pub fn fs_event_stop(&self, index: &Index) {
        self.actions.send(Action::FsEventStopReq(*index)).unwrap();
        self.actions_queue_empty.set(false);
    }

    /// Generic function to subscribe interest in an OS signal.
    fn signal_init<F>(&self, signum: i32, oneshot: bool, on_signal: F) -> Result<Index>
    where
        F: FnMut(LoopHandle, i32) + 'static,
    {
        // Parse signal number provided.
        let signum = SignalNum::parse(signum)?;

        let index = self.index();
        let on_signal = Box::new(on_signal);

        let signal_wrap = SignalWrap {
            id: index,
            oneshot,
            on_signal,
        };

        self.actions
            .send(Action::SignalStartReq(signum, signal_wrap))
            .unwrap();

        self.actions_queue_empty.set(false);

        Ok(index)
    }

    /// Start the handle with the given callback, watching for the given signal.
    pub fn signal_start<F>(&self, signum: i32, on_signal: F) -> Result<Index>
    where
        F: FnMut(LoopHandle, i32) + 'static,
    {
        self.signal_init(signum, false, on_signal)
    }

    /// Same functionality but the signal handler is reset the moment the signal is received.
    pub fn signal_start_oneshot<F>(&self, signum: i32, on_signal: F) -> Result<Index>
    where
        F: FnMut(LoopHandle, i32) + 'static,
    {
        self.signal_init(signum, true, on_signal)
    }

    /// Stop the handle, the callback will no longer be called.
    pub fn signal_stop(&self, index: &Index) {
        self.actions.send(Action::SignalStopReq(*index)).unwrap();
        self.actions_queue_empty.set(false);
    }
}

#[derive(Clone)]
pub struct LoopInterruptHandle {
    waker: Arc<Waker>,
}

impl LoopInterruptHandle {
    // Interrupts the poll phase of the event-loop.
    pub fn interrupt(&self) {
        self.waker.wake().unwrap();
    }
}
