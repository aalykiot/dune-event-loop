use crate::event_loop::BasicQueue;
use crate::event_loop::LoopHandle;
use crate::resource::Resource;
use crate::resource::ResourceId;
use anyhow::anyhow;
use anyhow::Result;
use mio::net::TcpStream as MioSocket;
use mio::Interest;
use mio::Registry;
use mio::Token;
use slotmap::Key;
use std::cell::Cell;
use std::collections::VecDeque;
use std::io;
use std::io::Read;
use std::io::Write;
use std::net::Shutdown;
use std::net::SocketAddr;
use std::rc::Rc;

pub type OnConnectionCallback = Box<dyn Fn(TcpStreamHandle, Result<SocketInfo>) + 'static>;
pub type OnReadCallback = Box<dyn Fn(TcpStreamHandle, Result<Vec<u8>>) + 'static>;
pub type OnWriteCallback = Box<dyn Fn(TcpStreamHandle, Result<usize>) + 'static>;
pub type OnCloseCallback = Box<dyn Fn(LoopHandle) + 'static>;

/// Information about the underlying tcp socket.
#[derive(Debug)]
pub struct SocketInfo {
    pub host: SocketAddr,
    pub remote: SocketAddr,
}

/// Indicates the kind of readiness in the socket.
#[derive(Debug)]
pub(crate) enum TcpEventKind {
    /// Socket is ready for reading.
    Read(Token),
    /// Socket is ready for writing.
    Write(Token),
}

/// The data required for a tcp connection resource.
pub(crate) struct TcpStream {
    pub id: Rc<Cell<ResourceId>>,
    pub socket: MioSocket,
    pub on_connection: Option<OnConnectionCallback>,
    pub on_read: Option<OnReadCallback>,
    pub on_close: Option<OnCloseCallback>,
    pub write_queue: VecDeque<(Vec<u8>, OnWriteCallback)>,
}

impl Resource for TcpStream {
    fn close(&mut self, handle: LoopHandle) {
        // Shutdown the write side of the stream.
        self.socket.shutdown(Shutdown::Write).unwrap();

        // Run any user defined close action.
        if let Some(callback) = self.on_close.take() {
            callback(handle);
        }
    }
}

impl TcpStream {
    /// Returns a handle to the tcp stream resource.
    pub fn handle(&self, handle: LoopHandle) -> TcpStreamHandle {
        TcpStreamHandle {
            id: self.id.clone(),
            handle,
        }
    }

    /// Adds data to the write queue for writing.
    pub fn enqueue(&mut self, data: Vec<u8>, callback: OnWriteCallback) {
        self.write_queue.push_back((data, callback));
    }

    /// Tries to read from a ready tcp socket. Ready means that
    /// the operation won't block the current thread.
    pub fn read_from_socket(
        &mut self,
        handle: LoopHandle,
        registry: &mut Registry,
        close_queue: &mut BasicQueue,
    ) {
        // Create buffers for reading data.
        let mut data = vec![];
        let mut data_buf = [0; 4096];

        // This will help us catch errors and FIN packets.
        let mut read_error: Option<io::Error> = None;
        let mut is_eof = false;

        // We can probably read from the socket connection.
        loop {
            match self.socket.read(&mut data_buf) {
                // Reading 0 bytes means the other side has closed the
                // connection or is done writing.
                Ok(0) => {
                    is_eof = true;
                    break;
                }
                Ok(n) => data.extend_from_slice(&data_buf[..n]),
                // Would block "errors" are the OS's way of saying that the connection
                // is not actually ready to perform this I/O operation.
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                // Other errors we'll be considered fatal.
                Err(e) => read_error = Some(e),
            }
        }

        // NOTE: If a FIN packet received without us listening on the tcp stream, it means that
        // the other side closed the connection so we'll schedule the resource for removal.
        let on_read = match self.on_read.as_mut() {
            Some(on_read) => on_read,
            None if !is_eof => return,
            None => {
                // Deregister any interest on the socket.
                registry.deregister(&mut self.socket).unwrap();
                close_queue.push(self.id.get());
                return;
            }
        };

        let tcp_handle = TcpStreamHandle {
            id: self.id.clone(),
            handle: handle.clone(),
        };

        // Check if we had any errors while reading.
        if let Some(e) = read_error {
            (on_read)(tcp_handle, Err(e.into()));
            return;
        }

        match data.len() {
            // FIN packet.
            0 => (on_read)(tcp_handle, Ok(data)),
            // We read some bytes.
            _ if !is_eof => (on_read)(tcp_handle, Ok(data)),
            // FIN packet is included to the bytes we read.
            _ => {
                (on_read)(tcp_handle.clone(), Ok(data));
                (on_read)(tcp_handle, Ok(vec![]));
            }
        };
    }

    /// Tries to read from a ready tcp socket. Ready means that
    /// the operation won't block the current thread.
    pub fn write_to_socket(&mut self, handle: LoopHandle, registry: &mut Registry) {
        // Create a handle to the resource.
        let tcp_handle = TcpStreamHandle {
            id: self.id.clone(),
            handle: handle.clone(),
        };

        // Check if the socket is in error state.
        if let Ok(Some(e)) | Err(e) = self.socket.take_error() {
            // If `on_connection` is available it means the socket error happened
            // while trying to connect.
            if let Some(on_connection) = self.on_connection.take() {
                (on_connection)(tcp_handle, Err(e.into()));
                return;
            }
            // Otherwise the error happened while writing.
            if let Some((_, on_write)) = self.write_queue.pop_front() {
                (on_write)(tcp_handle, Err(e.into()));
                return;
            }
        }

        // If the on_connection callback is None it means that in some previous iteration
        // we made sure the tcp socket is well connected with the remote host.
        if let Some(on_connection) = self.on_connection.take() {
            // Run socket's on_connection callback.
            (on_connection)(
                tcp_handle.clone(),
                Ok(SocketInfo {
                    host: self.socket.local_addr().unwrap(),
                    remote: self.socket.peer_addr().unwrap(),
                }),
            );

            let token = Token(self.get_id());

            registry
                .reregister(&mut self.socket, token, Interest::READABLE)
                .unwrap();
        }

        loop {
            // Connection is okay, let's write some bytes.
            let tcp_handle = tcp_handle.clone();
            let (data, on_write) = match self.write_queue.pop_front() {
                Some(value) => value,
                None => break,
            };

            match self.socket.write(&data) {
                // We want to write the entire `data` buffer in a single go. If we
                // write less we'll return a short write error (same as
                // `io::Write::write_all` does).
                Ok(n) if n < data.len() => {
                    let err_message = io::ErrorKind::WriteZero.to_string();
                    (on_write)(tcp_handle, Err(anyhow!("{}", err_message)));
                }
                // All bytes were written to socket.
                Ok(n) => (on_write)(tcp_handle, Ok(n)),
                // Would block "errors" are the OS's way of saying that the
                // connection is not actually ready to perform this I/O operation.
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    // Since we couldn't send this data we need to put it
                    // back into the write_queue.
                    self.write_queue.push_front((data, on_write));
                    break;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                // An important error seems to have accrued.
                Err(e) => (on_write)(tcp_handle, Err(e.into())),
            };
        }

        // Unregister write interest if the write_queue is empty.
        if self.write_queue.is_empty() {
            let token = Token(self.get_id());
            registry
                .reregister(&mut self.socket, token, Interest::READABLE)
                .unwrap();
        }
    }

    /// Returns the resource id as a usize.
    pub fn get_id(&self) -> usize {
        self.id.get().data().as_ffi() as usize
    }
}

/// A reference like struct to an open tcp connection.
#[derive(Debug, Clone)]
pub struct TcpStreamHandle {
    /// A shared pointer to the resource ID of the connection.
    pub(crate) id: Rc<Cell<ResourceId>>,
    /// A handle to the event-loop.
    handle: LoopHandle,
}

impl TcpStreamHandle {
    /// Writes data to the tcp stream.
    pub fn write<F>(&self, data: Vec<u8>, callback: F)
    where
        F: Fn(TcpStreamHandle, Result<usize>) + 'static,
    {
        // Use the event-loop handle to write.
        self.handle.tcp_write(self.id.get(), data, callback);
    }

    /// Starts reading from a tcp stream.
    pub fn set_read_callback<F>(&self, callback: F)
    where
        F: Fn(TcpStreamHandle, Result<Vec<u8>>) + 'static,
    {
        // Use the event-loop handle to set a read callback for the stream.
        self.handle.tcp_read_start(self.id.get(), callback);
    }

    /// Closes the write side of the TCP stream.
    pub fn shutdown<F>(&self, callback: F)
    where
        F: Fn(LoopHandle) + 'static,
    {
        // Use the event-loop handle to shutdown the write side of the stream.
        self.handle.tcp_shutdown(self.id.get(), callback);
    }

    /// Completely closes the tcp stream.
    pub fn close<F>(&self, callback: F)
    where
        F: Fn(LoopHandle) + 'static,
    {
        // Use the event-loop handle to close the stream.
        self.handle.tcp_close(self.id.get(), callback);
    }

    /// Returns a handle to the event-loop.
    pub fn get_loop(&self) -> LoopHandle {
        self.handle.clone()
    }
}
