use crate::DeviceQueue;
use event_manager::EventSource;
use std::io;
use std::marker::PhantomData;
use std::sync::{Arc, MutexGuard};
use virtio_queue::Queue;
use vmm_sys_util::epoll::Epoll;

#[derive(Clone)]
pub struct MockDeviceQueue();

pub struct MockKickEvent<'a> {
    phantom: PhantomData<&'a MockDeviceQueue>,
}

impl EventSource for MockKickEvent<'_> {
    fn register_self(&self, _epoll: &Arc<Epoll>, _token: u64) -> io::Result<()> {
        todo!()
    }

    fn unregister_self(&self, _epoll: &Arc<Epoll>) -> io::Result<()> {
        todo!()
    }
}



impl DeviceQueue for MockDeviceQueue {
    type Queue = Queue;
    type QueueGuard<'a> = MutexGuard<'a, Queue>;
    type KickEvent<'a> = MockKickEvent<'a>;

    type SharedVariant = MockDeviceQueue;

    fn upgrade_to_shared(self) -> Self::SharedVariant {
        todo!()
    }

    fn try_lock_queue<'a>(&'a self) -> Option<Self::QueueGuard<'a>> {
        todo!()
    }

    fn signal_used_queue(&self) -> io::Result<()> {
        todo!()
    }

    fn kick_event<'a>(&'a self) -> Self::KickEvent<'a> {
        todo!()
    }

    fn read_kick(&self) -> io::Result<bool> {
        todo!()
    }
}
