use crate::event_loop::LoopHandle;
use crate::resource::Resource;
use crate::resource::ResourceId;
use crate::resource::Shared;

pub type Callback = Box<dyn FnMut(CheckHandle) + 'static>;

/// Check resources will run the given callback once per loop iteration.
/// https://docs.libuv.org/en/v1.x/check.html
pub(crate) struct Check {
    pub id: Shared<ResourceId>,
    pub callback: Callback,
}

impl Check {
    /// Returns a handle to the check resource.
    pub fn handle(&self, handle: LoopHandle) -> CheckHandle {
        CheckHandle {
            id: self.id.clone(),
            handle,
        }
    }

    /// Runs the callback of the timer.
    pub fn run_callback(&mut self, handle: LoopHandle) {
        let handle = self.handle(handle);
        (self.callback)(handle);
    }
}

impl Resource for Check {}

/// A reference to the check resource.
#[derive(Debug, Clone)]
pub struct CheckHandle {
    /// A shared pointer to the resource ID of the resource.
    id: Shared<ResourceId>,
    /// A handle to the event-loop.
    handle: LoopHandle,
}

impl CheckHandle {
    /// The check callback will no longer be called.
    pub fn remove(&self) {
        self.handle.check_remove(self.id.clone());
    }

    /// Returns a handle to the event-loop.
    pub fn get_loop(&self) -> LoopHandle {
        self.handle.clone()
    }
}
