use crate::event_loop::LoopHandle;
use crate::resource::Resource;
use crate::resource::ResourceId;
use crate::resource::Shared;
use std::collections::BTreeMap;
use std::time::Duration;
use std::time::Instant;

#[derive(Default)]
pub(crate) struct TimersCollection {
    /// A sorted tree of timers.
    tree: BTreeMap<Instant, Vec<ResourceId>>,
}

impl TimersCollection {
    /// Creates an empty collection to track timers.
    pub fn new() -> Self {
        TimersCollection::default()
    }

    /// Sets the timer into the collection.
    pub fn insert(&mut self, timestamp: Instant, id: ResourceId) {
        // Update an exisitng list of timers at the provided instant,
        // or create a new one if it doesn't exists.
        self.tree.entry(timestamp).or_default().push(id);
    }

    /// Returns the expired timers based on a timestamp.
    pub fn split_expired_timers(&mut self, timestamp: Instant) -> Vec<ResourceId> {
        // Split the tree into expired and pending timers.
        let future = self.tree.split_off(&timestamp);
        let expired = std::mem::replace(&mut self.tree, future);

        expired.into_values().flatten().collect()
    }

    /// Returns the next timer to expire in the queue.
    pub fn next(&self) -> Option<(&Instant, &Vec<ResourceId>)> {
        self.tree.iter().next()
    }
}

pub type TimerCallback = Box<dyn FnMut(LoopHandle)>;

// Defines the style of the timer.
pub enum TimerKind {
    Timeout,
    Interval,
}

/// The data required for a timer resource.
pub(crate) struct Timer {
    pub id: Shared<ResourceId>,
    pub delay: Duration,
    pub callback: TimerCallback,
    pub kind: TimerKind,
}

impl Timer {
    /// Runs the callback of the timer.
    pub fn run_callback(&mut self, handle: LoopHandle) {
        (self.callback)(handle);
    }

    /// Returns a handle to the timer resource.
    pub fn handle(&self, handle: LoopHandle) -> TimerHandle {
        TimerHandle {
            id: self.id.clone(),
            handle,
        }
    }
}

impl Resource for Timer {}

/// A reference like struct to an active timer.
#[derive(Debug, Clone)]
pub struct TimerHandle {
    /// A shared pointer to the resource ID of the timer.
    pub(crate) id: Shared<ResourceId>,
    /// A handle to the event-loop.
    handle: LoopHandle,
}

impl TimerHandle {
    /// Cancels the scheduled timer.
    pub fn cancel(self) {
        // Consume self and call the internal cancle_timer method.
        self.handle.cancel_timer(self.id.clone());
    }
}
