use event_manager::EventSource;
use std::collections::BTreeMap;
use std::io;
use std::ops::DerefMut;
use virtio_queue::QueueOwnedT;
use vm_memory::bitmap::BitmapSlice;
use vm_memory::{GuestAddressSpace, GuestMemory, GuestMemoryRegion};

#[cfg(feature = "mock")]
pub mod mock;

#[derive(Debug, Clone)]
pub struct QueueArgs {
    max_size: u16,
}

impl QueueArgs {
    pub const fn new(max_size: u16) -> QueueArgs {
        QueueArgs { max_size }
    }

    pub fn max_size(&self) -> u16 {
        self.max_size
    }
}

/// A Trait wrapping a virtio queue along with notification mechanisms, for receiving and sending
/// notifications related to that queue.
pub trait DeviceQueue: Send {
    type Queue: QueueOwnedT;

    type QueueGuard<'a>: DerefMut<Target = Self::Queue>
    where
        Self: 'a;

    #[cfg_attr(feature = "mock", mockall::concretize)]
    type KickEvent<'a>: EventSource + Send
    where
        Self: 'a;

    type SharedVariant: DeviceQueue + Sync + Clone;

    /// Upgrade the queue to a type that is also Sync + Clone
    ///
    /// The shared variant of the queue that can be shared and accessed from multiple threads.
    /// The Clone implementation has to be implemented in a shallow manner - cloned instances have
    /// to refer to the same DeviceQueue instance.
    fn upgrade_to_shared(self) -> Self::SharedVariant;

    // Try to lock the queue for processing. Returns None if the queue is not ready to be processed.
    // In general a device should wait on kick_event() to process the queue, but it can also wait
    // on other events (e.g. a file descriptor from which data is being copied), but in that case
    // the queue could not be ready when the fd is and queue_lock could return None, the device
    // has to then wait for a kick to continue processing.
    fn try_lock_queue<'a>(&'a self) -> Option<Self::QueueGuard<'a>>;

    /// Notify the driver that used descriptors have been put into the used queue.
    fn signal_used_queue(&self) -> io::Result<()>;

    /// Provides an EventSource for a kick. Upon a kick the device can continue/start processing the
    /// queue. The kick_event has to be disarmed by read_kick() otherwise the kick_event notification
    /// will keep being received.
    fn kick_event<'a>(&'a self) -> Self::KickEvent<'a>;

    /// Read the kick event, returns Ok(true) if queue is enabled and can be processed.
    fn read_kick(&self) -> io::Result<bool>;
}

// Utility to access the bitmap, TODO: move this somewhere else?
pub type BitmapOfMem<M> = <<M as GuestMemory>::R as GuestMemoryRegion>::B;
pub type BitmapOfAS<M> = <<<M as GuestAddressSpace>::M as GuestMemory>::R as GuestMemoryRegion>::B;

pub trait VirtioDevice<M, Q>: Send
where
    M: GuestAddressSpace + Send + 'static,
    Q: DeviceQueue + Send + 'static,
    BitmapOfAS<M>: BitmapSlice,
{
    /// The virtio device type.
    fn device_type(&self) -> u32;

    /// Get the available features offered by device.
    fn avail_features(&self) -> u64;

    /// Get acknowledged features of the driver.
    fn acked_features(&self) -> u64;

    /// Set acknowledged features of the driver.
    /// This function must maintain the following invariant:
    /// self.avail_features() & self.acked_features() = self.acked_features()
    fn set_acked_features(&mut self, acked_features: u64);

    /// Returns the queues the device wants to construct.
    fn queues(&self) -> &[QueueArgs];

    /// Reads this device configuration space at `offset`.
    /// TODO: specific error type?
    fn read_config(&self, offset: usize, data: &mut [u8]) -> anyhow::Result<()>;

    /// Writes to this device configuration space at `offset`.
    /// TODO specific error type?
    fn write_config(&mut self, offset: usize, data: &[u8]) -> anyhow::Result<()>;

    /// This is a callback performed when the device's memory changes during operation.
    /// An implementation may update the memory through holding another refernece to the memory in the
    /// background. In that case the call to this method, is just a "fence" when the memory has
    /// actually been updated.
    /// Notably the vhost-user-backend implementation does
    fn update_memory(&mut self, _mem: M) -> io::Result<()> {
        Ok(())
    }

    /// Activate the device. Upon activation the device can spawn worker thread(s) to handle
    // TODO: change BTreeMap<usize, Q> to [Option<Q>] ? This might be more efficient? Or let the device
    // itself do this?
    fn activate(&mut self, mem: M, queues: BTreeMap<usize, Q>) -> anyhow::Result<()>;

    /// Stop device's worker threads.
    ///
    /// This may block to make sure the device can flush important data (e.g. disk writes).
    fn stop(&mut self);

    /// Stop device's worker threads and reset the device state.
    ///
    /// Implementation of this method is optional. Returns "true" if device reset is supported
    /// meaning the device can be activated again.
    fn reset(&mut self) -> anyhow::Result<bool> {
        self.stop();
        Ok(false)
    }
}
