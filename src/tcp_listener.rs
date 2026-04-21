use crate::event_loop::LoopHandle;
use crate::resource::Resource;
use crate::resource::ResourceId;
use crate::tcp_stream::SocketInfo;
use crate::tcp_stream::TcpStream;
use anyhow::Result;
use mio::net::TcpListener as MioListener;
use slotmap::DefaultKey;
use slotmap::Key;
use std::cell::Cell;
use std::collections::VecDeque;
use std::io;
use std::rc::Rc;

type OnConnectionCallback = Box<dyn Fn(TcpListenerHandle, Result<SocketInfo>) + 'static>;

/// The data required for a tcp listener resource.
pub(crate) struct TcpListener {
    pub id: Rc<Cell<ResourceId>>,
    pub listener: MioListener,
    pub on_connection: OnConnectionCallback,
}

impl TcpListener {
    /// Returns a handle to the tcp listener resource.
    pub fn handle(&self, handle: LoopHandle) -> TcpListenerHandle {
        TcpListenerHandle {
            id: self.id.clone(),
            handle,
        }
    }

    /// Tries to accept a new available clients.
    pub fn accept(&mut self, loop_handle: LoopHandle) -> Vec<TcpStream> {
        // Try to accept as many new connections as possible.
        let mut clients = vec![];

        let handle = self.handle(loop_handle);
        let callback = self.on_connection.as_mut();

        loop {
            // Received an event for the tcp listener, which indicates
            // we can accept a new connection.
            let handle = handle.clone();
            let (socket, _) = match self.listener.accept() {
                Ok(socket) => socket,
                // If we get a "WouldBlock" error we know our listener has no more incoming
                // connections queued, so we can return to polling and wait for some more.
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    callback(handle, Result::Err(e.into()));
                    break;
                }
            };

            // Since the resource is not yet scheduled in the event-loop, we create a
            // null ID. The event-loop will update this value with a real ID later.
            let id = Rc::new(Cell::new(DefaultKey::null()));
            let host = socket.local_addr().unwrap();
            let remote = socket.peer_addr().unwrap();

            let stream = TcpStream {
                id,
                socket,
                on_connection: None,
                on_read: None,
                on_close: None,
                write_queue: VecDeque::new(),
            };

            callback(handle, Ok(SocketInfo { host, remote }));
            clients.push(stream);
        }

        clients
    }
}

impl Resource for TcpListener {}

/// A reference like struct to an active tcp listener.
#[derive(Debug, Clone)]
pub struct TcpListenerHandle {
    /// A shared pointer to the resource ID of the listener.
    pub(crate) id: Rc<Cell<ResourceId>>,
    /// A handle to the event-loop.
    handle: LoopHandle,
}
