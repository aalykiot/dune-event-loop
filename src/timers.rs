use crate::event_loop::LoopHandle;
use crate::event_loop::Resource;
use crate::event_loop::ResourceId;
use std::cell::Cell;
use std::collections::BTreeMap;
use std::rc::Rc;
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

        expired.into_iter().flat_map(|(_, t)| t).collect()
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
    pub id: Rc<Cell<ResourceId>>,
    pub delay: Duration,
    pub callback: TimerCallback,
    pub kind: TimerKind,
}

impl Resource for Timer {}

impl Timer {
    /// Runs the callback of the timer.
    pub fn run_callback(&mut self, handle: LoopHandle) {
        (self.callback)(handle);
    }
}
