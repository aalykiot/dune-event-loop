use crate::event_loop::LoopHandle;
use crate::resource::Resource;
use crate::resource::ResourceId;
use crate::resource::Shared;
use anyhow::Result;
use crossterm::terminal;
use std::io;
use std::io::Write;
use std::rc::Rc;

pub type OnReadCallback = Box<dyn FnMut(TTYHandle, Result<Vec<u8>>) + 'static>;

/// Represents the terminal input mode.
pub enum Mode {
    /// Standard terminal mode where input is line-buffered and processed by
    /// the terminal driver before being delivered to the application.
    Normal,
    /// Raw terminal mode where input is delivered directly to the application
    /// without terminal driver processing.
    Raw,
}

/// The data required for a TTY resource.
pub(crate) struct TTY {
    pub id: Shared<ResourceId>,
    pub on_read: Option<OnReadCallback>,
}

impl TTY {
    /// Returns a handle to the tty resource.
    pub fn handle(&self, handle: LoopHandle) -> TTYHandle {
        TTYHandle {
            id: Rc::clone(&self.id),
            handle,
        }
    }
}

impl Resource for TTY {}

/// A reference like struct to tty.
#[derive(Debug, Clone)]
pub struct TTYHandle {
    /// A shared pointer to the resource ID of the tty.
    pub(crate) id: Shared<ResourceId>,
    /// A handle to the event-loop.
    handle: LoopHandle,
}

impl TTYHandle {
    /// Writes data to the stdout stream.
    fn write<D>(&self, data: D)
    where
        D: AsRef<[u8]>,
    {
        // Unlike reading from stdin, writing to stdout is not an asynchronous operation.
        // The bytes are written immediately when this function is called.
        let mut stdout = io::stdout().lock();

        stdout.write_all(data.as_ref()).unwrap();
        stdout.flush().unwrap();
    }

    /// Sets the TTY to raw mode.
    fn enable_raw_mode(&self) -> Result<()> {
        terminal::enable_raw_mode().map_err(Into::into)
    }

    /// Resets TTY settings to default values for the next process to take over.
    pub fn reset_mode(&self) -> Result<()> {
        terminal::disable_raw_mode().map_err(Into::into)
    }

    /// Set the TTY using the specified terminal mode.
    pub fn set_mode(&self, mode: Mode) -> Result<()> {
        match mode {
            Mode::Normal => self.reset_mode(),
            Mode::Raw => self.enable_raw_mode(),
        }
    }

    /// Gets the current Window size.
    pub fn get_winsize(&self) -> Result<terminal::WindowSize> {
        terminal::window_size().map_err(Into::into)
    }

    /// Returns a handle to the event-loop.
    pub fn get_loop(&self) -> LoopHandle {
        self.handle.clone()
    }
}
