use crate::event_loop::LoopHandle;
use downcast_rs::impl_downcast;
use downcast_rs::Downcast;
use slotmap::DefaultKey;
use slotmap::SlotMap;

/// A type alias for resource identification.
pub type ResourceId = DefaultKey;

/// All objects that are tracked by the event-loop should implement the `Resource` trait.
pub trait Resource: Downcast + 'static {
    /// Implements any clean up actions.
    fn close(&mut self, _: LoopHandle) {}
}

impl_downcast!(Resource);

/// A struct that holds and manages resources.
#[derive(Default)]
pub(crate) struct ResourceMap {
    inner: SlotMap<ResourceId, Box<dyn Resource>>,
}

impl ResourceMap {
    /// Returns a reference to a downcasted resource type.
    #[allow(unused)]
    pub fn get_as<T: Resource>(&self, id: ResourceId) -> Option<&T> {
        let resource = self.inner.get(id);
        resource.and_then(|resource| resource.downcast_ref::<T>())
    }

    /// Returns a mutable reference to a downcasted resource type.
    pub fn get_mut_as<T: Resource>(&mut self, id: ResourceId) -> Option<&mut T> {
        let resource = self.inner.get_mut(id);
        resource.and_then(|resource| resource.downcast_mut::<T>())
    }
}

impl std::ops::Deref for ResourceMap {
    type Target = SlotMap<ResourceId, Box<dyn Resource>>;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl std::ops::DerefMut for ResourceMap {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}
