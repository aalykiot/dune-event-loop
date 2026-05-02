use crate::event_loop::LoopHandle;
use crate::resource::Resource;
use crate::resource::ResourceId;
use crate::resource::Shared;
use anyhow::Result;
use std::sync::mpsc;

pub type Output = Option<Result<Vec<u8>>>;

pub type WorkFn = Box<dyn FnOnce() -> Output + Send>;
pub type OnCompleteCallback = Box<dyn FnMut(LoopHandle, Output) + 'static>;

/// The data required for a task resource.
pub(crate) struct Task {
    pub id: Shared<ResourceId>,
    pub on_complete: Option<OnCompleteCallback>,
    pub cancel_tx: mpsc::Sender<()>,
}

impl Task {
    /// Runs the callback of the task.
    pub fn run_callback(&mut self, output: Output, handle: LoopHandle) {
        if let Some(mut callback) = self.on_complete.take() {
            callback(handle, output);
        }
    }

    /// Returns a handle to the task resource.
    pub fn handle(&mut self, handle: LoopHandle) -> TaskHandle {
        TaskHandle {
            id: self.id.clone(),
            cancelation: self.cancel_tx.clone(),
            handle,
        }
    }
}

impl Resource for Task {}

/// A reference like struct to a running or finished task.
#[derive(Debug, Clone)]
pub struct TaskHandle {
    /// A shared pointer to the resource ID of the spawned task.
    pub(crate) id: Shared<ResourceId>,
    /// A channel to submit cancelation notifications.
    cancelation: mpsc::Sender<()>,
    /// A clonable handle to the event-loop.
    handle: LoopHandle,
}

impl TaskHandle {
    /// Cancels the queued task. This will only succeed if no worker thread
    /// has started processing it yet. Once a worker has picked up the
    /// task for execution, it cannot be stopped.
    pub fn cancel(self) {
        // Notify for the cancelation and remove the resource.
        let handle = self.handle.clone();
        let _ = self.cancelation.send(());

        handle.cancel_task(self.id.clone());
    }
}
