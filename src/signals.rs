use crate::event_loop::LoopHandle;
use mio::Interest;
use mio::Registry;
use mio::Token;
pub use signal_hook::consts::signal as SignalKind;
use signal_hook::low_level::emulate_default_handler;
use std::collections::HashMap;

#[cfg(target_family = "unix")]
use signal_hook_mio::v1_0::Signals;

#[cfg(target_family = "windows")]
use std::sync::mpsc;

#[derive(Debug, Clone, Copy)]
pub enum Lifetime {
    /// Invoked at most once.
    Oneshot,
    /// Stays active until explicitly removed.
    Persistent,
}

// Based on linux systems the max allowed signal number is 31.
// https://www-uxsup.csx.cam.ac.uk/courses/moved.Building/signals.pdf
const MAX_SIGNAL_VALUE: i32 = 31;

#[derive(Debug)]
pub(crate) struct SignalNum(i32);

impl TryFrom<i32> for SignalNum {
    type Error = String;

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        // Check if signal is within UNIX range.
        if value > MAX_SIGNAL_VALUE {
            return Err("Signal number out of range".to_string());
        }

        // Forbidden signals are not allowed to be registered.
        if signal_hook::consts::FORBIDDEN.contains(&value) {
            return Err("Forbidden signal provided".to_string());
        }

        Ok(SignalNum(value))
    }
}

impl From<SignalNum> for i32 {
    fn from(value: SignalNum) -> Self {
        value.0
    }
}

type SignalCallback = Box<dyn FnMut(SignalHandle, i32) + 'static>;

pub(crate) struct Signal {
    /// A unique referene.
    pub id: u64,
    /// Callback invoked when the signal is triggered.
    pub callback: SignalCallback,
    /// Controls how long a callback remains active.
    pub lifetime: Lifetime,
}

impl Signal {
    /// Returns a handle to the signal resource.
    pub fn handle(&self, handle: LoopHandle) -> SignalHandle {
        SignalHandle {
            id: self.id,
            handle,
        }
    }
}

/// A reference like struct to an active signal listener.
#[derive(Debug, Clone)]
pub struct SignalHandle {
    /// A unique referene.
    pub(crate) id: u64,
    /// A handle to the event-loop.
    handle: LoopHandle,
}

impl SignalHandle {
    /// The signal callback will no longer be called.
    pub fn stop(&self) {
        self.handle.signal_stop(self.id);
    }

    /// Returns a handle to the event-loop.
    pub fn get_loop(&self) -> LoopHandle {
        self.handle.clone()
    }
}

/// Abstracts signal handling across platforms.
pub(crate) struct OsSignals {
    #[cfg(target_family = "unix")]
    pub sources: Signals,
    pub handlers: HashMap<i32, Vec<Signal>>,
}

impl OsSignals {
    #[cfg(target_family = "unix")]
    pub fn new(registry: &Registry) -> Self {
        let mut sources = Signals::new::<[_; 0], i32>([]).unwrap();
        let _ = registry.register(&mut sources, Token(1), Interest::READABLE);

        OsSignals {
            sources,
            handlers: HashMap::new(),
        }
    }

    #[cfg(target_family = "windows")]
    fn new(notifier: mpsc::Sender<Event>, waker: Arc<Waker>) -> Self {
        // Spawn signal watching thread.
        let on_signal_handler = move || {
            notifier.send(Event::WinSigInt).unwrap();
            waker.wake().unwrap();
        };

        ctrlc::set_handler(on_signal_handler).unwrap();

        OsSignals {
            handlers: HashMap::new(),
        }
    }

    #[cfg(target_family = "unix")]
    pub fn run_pending(&mut self, handle: LoopHandle) {
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
                let handle = handler.handle(handle.clone());
                (handler.callback)(handle, signal);

                // Keep the listener if persistent.
                match handler.lifetime {
                    Lifetime::Oneshot => false,
                    Lifetime::Persistent => true,
                }
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
        let handlers = match self.handlers.get_mut(&SignalKind::SIGINT) {
            Some(handlers) if !handlers.is_empty() => handlers,
            _ => {
                emulate_default_handler(SignalKind::SIGINT).unwrap();
                return;
            }
        };

        handlers.retain_mut(|handler| {
            // Run handler's callback.
            let handle = handler.handle(handle.clone());
            (handler.callback)(handle, SignalKind::SIGINT);

            // Keep the listener if persistent.
            match handler.lifetime {
                Lifetime::Oneshot => false,
                Lifetime::Persistent => true,
            }
        });
    }

    pub fn remove_handler(&mut self, id: u64) {
        // Note: Given the structure of this struct we're following the simplest
        // approach to remove the element with O(n^2) complexity. Further
        // improvements can be made in the future if necessary.
        if let Some((_, list)) = self
            .handlers
            .iter_mut()
            .find(|(_, list)| list.iter().any(|handler| handler.id == id))
        {
            list.retain(|handler| handler.id != id);
        }
    }
}
