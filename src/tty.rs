use crate::event_loop::LoopHandle;
use crate::resource::Resource;
use crate::resource::ResourceId;
use crate::resource::Shared;
use anyhow::Result;
use crossterm::terminal;
use std::io;
use std::io::Write;
use std::rc::Rc;
use std::sync::mpsc;

pub type OnReadCallback = Box<dyn FnMut(TtyHandle, Result<Vec<u8>>) + 'static>;

/// Represents the terminal input mode.
pub enum Mode {
    /// Standard terminal mode where input is line-buffered and processed by
    /// the terminal driver before being delivered to the application.
    Normal,
    /// Raw terminal mode where input is delivered directly to the application
    /// without terminal driver processing.
    Raw,
}

/// The internal worker state for a TTY resource.
pub(crate) struct TtyReader {
    pub id: Shared<ResourceId>,
    pub on_read: OnReadCallback,
    pub stop_tx: mpsc::Sender<()>,
}

impl TtyReader {
    /// Returns a handle to the TTY resource.
    pub fn handle(&self, handle: LoopHandle) -> TtyHandle {
        TtyHandle {
            id: Rc::clone(&self.id),
            stop_tx: Some(self.stop_tx.clone()),
            handle,
        }
    }
}

impl Resource for TtyReader {}

/// A reference like struct to a TTY instance.
#[derive(Debug, Clone)]
pub struct TtyHandle {
    /// A shared pointer to the resource ID of the tty.
    pub(crate) id: Shared<ResourceId>,
    /// Sends a signal to stop the input-reading thread.
    pub(crate) stop_tx: Option<mpsc::Sender<()>>,
    /// A handle to the event-loop.
    pub(crate) handle: LoopHandle,
}

impl TtyHandle {
    /// Writes data to the stdout stream.
    pub fn write<D>(&self, data: D)
    where
        D: AsRef<[u8]>,
    {
        // Unlike reading from stdin, writing to stdout is not an asynchronous operation.
        // The bytes are written immediately when this function is called.
        let mut stdout = io::stdout().lock();

        stdout.write_all(data.as_ref()).unwrap();
        stdout.flush().unwrap();
    }

    /// Starts reading from the TTY.
    pub fn start_reading<F>(&self, callback: F)
    where
        F: FnMut(TtyHandle, Result<Vec<u8>>) + 'static,
    {
        // This channel is used to stop the worker thread reading from stdin.
        let (stop_tx, stop_rx) = mpsc::channel();

        let reader = TtyReader {
            id: Rc::clone(&self.id),
            on_read: Box::new(callback),
            stop_tx: stop_tx.clone(),
        };

        self.handle.tty_read_start(reader, stop_rx);
    }

    /// Stops reading from the TTY.
    pub fn stop_reading(&self) {
        // Send a stop signal to the thread that is reading from stdin.
        self.stop_tx.as_ref().unwrap().send(()).unwrap();
        self.handle.tty_close(Rc::clone(&self.id));
    }

    /// Sets the TTY to raw mode.
    fn enable_raw_mode(&self) -> Result<()> {
        terminal::enable_raw_mode().map_err(Into::into)
    }

    /// Disables raw mode, restoring the terminal to its original settings.
    pub fn disable_raw_mode(&self) -> Result<()> {
        terminal::disable_raw_mode().map_err(Into::into)
    }

    /// Set the TTY using the specified terminal mode.
    pub fn set_mode(&self, mode: Mode) -> Result<()> {
        match mode {
            Mode::Normal => self.disable_raw_mode(),
            Mode::Raw => self.enable_raw_mode(),
        }
    }

    /// Gets the current window size.
    pub fn window_size(&self) -> Result<terminal::WindowSize> {
        terminal::window_size().map_err(Into::into)
    }

    /// Returns a handle to the event-loop.
    pub fn loop_handle(&self) -> LoopHandle {
        self.handle.clone()
    }
}
