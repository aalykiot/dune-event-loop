use crate::event_loop::LoopHandle;
use crate::resource::Resource;
use crate::resource::ResourceId;
use crate::resource::Shared;
use crate::tcp_stream::OnCloseCallback;
use crate::tcp_stream::TcpStream;
use crate::tcp_stream::TcpStreamHandle;
use crate::tcp_stream::READ_BUFFER_SIZE;
use anyhow::Result;
use mio::net::TcpListener as MioListener;
use mio::Token;
use slotmap::DefaultKey;
use slotmap::Key;
use std::cell::Cell;
use std::collections::VecDeque;
use std::io;
use std::rc::Rc;

type OnConnectionCallback = Box<dyn FnMut(TcpListenerHandle, Result<TcpStreamHandle>) + 'static>;

/// The data required for a tcp listener resource.
pub(crate) struct TcpListener {
    pub id: Shared<ResourceId>,
    pub socket: MioListener,
    pub on_connection: OnConnectionCallback,
    pub on_close: Option<OnCloseCallback>,
}

impl TcpListener {
    /// Returns a handle to the tcp listener resource.
    pub fn handle(&self, handle: LoopHandle) -> TcpListenerHandle {
        TcpListenerHandle {
            id: self.id.clone(),
            handle,
        }
    }

    /// Tries to accept new available connections.
    pub fn accept(&mut self, loop_handle: LoopHandle) -> Vec<TcpStream> {
        // Buffer to hold all new tcp streams.
        let mut clients = vec![];

        let handle = self.handle(loop_handle.clone());
        let callback = self.on_connection.as_mut();

        loop {
            // Received an event for the tcp listener, which indicates
            // we can accept a new connection.
            let handle = handle.clone();
            let (socket, _) = match self.socket.accept() {
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

            let stream = TcpStream {
                id,
                socket,
                read_buffer: [0; READ_BUFFER_SIZE],
                on_connection: None,
                on_read: None,
                on_close: None,
                write_queue: VecDeque::new(),
            };

            callback(handle, Ok(stream.handle(loop_handle.clone())));
            clients.push(stream);
        }

        clients
    }

    /// Returns a token linked to the underline socket.
    pub fn token(&self) -> Token {
        Token(self.id.get().data().as_ffi() as usize)
    }
}

impl Resource for TcpListener {}

/// A reference like struct to an active tcp listener.
#[derive(Debug, Clone)]
pub struct TcpListenerHandle {
    /// A shared pointer to the resource ID of the listener.
    pub(crate) id: Shared<ResourceId>,
    /// A handle to the event-loop.
    handle: LoopHandle,
}

impl TcpListenerHandle {
    /// Stops the server from accepting new tcp connections.
    pub fn shutdown<F>(self, callback: F)
    where
        F: Fn(LoopHandle) + 'static,
    {
        // Use the event-loop handle to shutdown the tcp listenr.
        self.handle.tcp_stop(self.id.clone(), callback);
    }

    /// Returns a handle to the event-loop.
    pub fn get_loop(&self) -> LoopHandle {
        self.handle.clone()
    }
}
