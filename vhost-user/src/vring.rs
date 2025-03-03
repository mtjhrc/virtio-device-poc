// Copyright 2019 Intel Corporation. All Rights Reserved.
// Copyright 2021 Alibaba Cloud Computing. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0

//! Struct to maintain state information and manipulate vhost-user-user queues.

use arc_swap::ArcSwapOption;
use event_manager::EventSource;
use std::fs::File;
use std::io;
use std::io::ErrorKind;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::io::{FromRawFd, IntoRawFd};
use std::result::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError, Weak};
use virtio::{DeviceQueue, QueueArgs};
use virtio_queue::{Error as VirtQueError, Queue, QueueT};
use vm_memory::GuestAddress;
use vmm_sys_util::epoll::{ControlOperation, Epoll, EpollEvent, EventSet};
use vmm_sys_util::eventfd::EventFd;

struct VringState {
    enabled: AtomicBool,
    kick: ArcSwapOption<EventFd>,
    call: ArcSwapOption<EventFd>,
    err: ArcSwapOption<EventFd>,
    // Epoll which monitor are monitoring the kick EventFd. These need to be updated when kick
    // changes.
    epolls: Mutex<Vec<(Weak<Epoll>, u64)>>,
}

/// Struct to maintain raw state information for a vhost-user-user queue.
///
/// This struct maintains all information of a virito queue, and could be used as an `VringT`
/// object for single-threaded context.
#[derive(Clone)]
pub struct Vring {
    // We map the `queue.ready() == false` state to a stopped Vring state,
    // and `ready() == true` to a started Vring
    queue: Arc<Mutex<Queue>>,
    // Condvar for queue.ready()
    //queue_ready_cond: Arc<Condvar>,
    state: Arc<VringState>,
}

fn register_kick(epoll: &Epoll, kick_fd: RawFd, token: u64) -> io::Result<()> {
    log::trace!(
        "Registering token={token} fd={kick_fd} into epoll {}",
        epoll.as_raw_fd()
    );
    match epoll.ctl(
        ControlOperation::Add,
        kick_fd.as_raw_fd(),
        EpollEvent::new(EventSet::IN, token),
    ) {
        Ok(()) => Ok(()),
        Err(e) => Err(e),
    }
}

fn unregister_kick(epoll: &Epoll, kick_fd: RawFd) -> io::Result<()> {
    log::trace!(
        "Unregistering fd={kick_fd} into epoll {}",
        epoll.as_raw_fd()
    );
    match epoll.ctl(
        ControlOperation::Delete,
        kick_fd.as_raw_fd(),
        EpollEvent::new(EventSet::IN, 0),
    ) {
        Ok(()) => Ok(()),
        Err(e) => Err(e),
    }
}

impl Vring {
    pub(crate) fn queue_mut(&self) -> MutexGuard<Queue> {
        self.queue.lock().unwrap()
    }

    pub fn new(queue_args: &QueueArgs) -> Result<Vring, VirtQueError> {
        let queue = Queue::new(queue_args.max_size())?;
        Ok(Self {
            queue: Arc::new(Mutex::new(queue)),
            //queue_ready_cond: Arc::new(Condvar::new()),
            state: Arc::new(VringState {
                enabled: Default::default(),
                kick: None.into(),
                call: None.into(),
                err: None.into(),
                epolls: Mutex::new(vec![]),
            }),
        })
    }

    pub fn set_queue_info(
        &mut self,
        desc_table: u64,
        avail_ring: u64,
        used_ring: u64,
    ) -> Result<(), VirtQueError> {
        let mut queue = self.queue_mut();
        queue.try_set_desc_table_address(GuestAddress(desc_table))?;
        queue.try_set_avail_ring_address(GuestAddress(avail_ring))?;
        queue.try_set_used_ring_address(GuestAddress(used_ring))
    }

    pub fn is_enabled(&self) -> bool {
        self.state.enabled.load(Ordering::SeqCst)
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.state.enabled.store(enabled, Ordering::SeqCst)
    }

    /// Stop processing the vring, we make sure the device cannot process it!
    pub fn stop(&self) -> io::Result<()> {
        // Note that we take a lock of the queue after we remove ourselves from epolls,
        // This order should better guarantee that once Vring::stop returns no thread is processing
        // the vring
        if let Some(kick_efd) = self.state.kick.load().as_ref() {
            self.update_epolls(Some(kick_efd.as_raw_fd()), None)?;
        }
        self.queue_mut().set_ready(false);
        Ok(())
    }

    /// Start processing the vring,
    pub fn start(&self) -> io::Result<()> {
        self.queue_mut().set_ready(true);
        self.update_epolls(
            None,
            self.state.kick.load().as_ref().map(|fd| fd.as_raw_fd()),
        )
    }

    pub fn is_started(&self) -> bool {
        self.queue_mut().ready()
    }

    pub fn set_call(&self, file: Option<File>) {
        self.state
            .call
            .store(file.map(|f| Arc::new(unsafe { EventFd::from_raw_fd(f.into_raw_fd()) })));
    }

    pub fn update_epolls(
        &self,
        old_kick_efd: Option<RawFd>,
        new_kick_efd: Option<RawFd>,
    ) -> io::Result<()> {
        // Update known epoll instances waiting on our kick
        for (epoll, token) in self
            .state
            .epolls
            .lock()
            .unwrap()
            .iter()
            .flat_map(|(epoll, token)| epoll.upgrade().and_then(|epoll| Some((epoll, *token))))
        {
            let epoll = epoll.as_ref();
            if let Some(old_kick_efd) = old_kick_efd {
                unregister_kick(epoll, old_kick_efd)?;
            }
            if let Some(new_kick_efd) = new_kick_efd {
                register_kick(epoll, new_kick_efd, token)?;
            }
        }
        Ok(())
    }

    pub fn set_kick(&self, file: Option<File>) -> io::Result<()> {
        let new_kick_efd = file.map(|f| Arc::new(unsafe { EventFd::from_raw_fd(f.into_raw_fd()) }));
        let old_kick_efd = self.state.kick.swap(new_kick_efd.clone());
        if self.is_started() {
            self.update_epolls(
                old_kick_efd.map(|fd| fd.as_raw_fd()),
                new_kick_efd.map(|fd| fd.as_raw_fd()),
            )?;
        }
        Ok(())
    }

    pub fn set_err(&self, file: Option<File>) {
        self.state
            .err
            .store(file.map(|f| Arc::new(unsafe { EventFd::from_raw_fd(f.into_raw_fd()) })));
    }

    pub fn has_kick_event(&self) -> bool {
        self.state.kick.load().is_some()
    }

    fn add_epoll(&self, epoll: Weak<Epoll>, token: u64) {
        let mut epolls = self.state.epolls.lock().unwrap();
        // Since we already lock the mutex, let's use the opportunity to also prune dead epolls
        epolls.retain(|(weak_epoll, _token)| weak_epoll.strong_count() >= 1);
        epolls.push((epoll, token));
    }

    /// Remove the specified epoll from the list of epolls we need to update on kick change.
    /// NOTE: this also has a side effect of removing dead epoll Arc references
    fn remove_epoll(&self, epoll: &Epoll) {
        let mut epolls = self.state.epolls.lock().unwrap();
        epolls.retain(|(weak_epoll, _token)| {
            weak_epoll
                .upgrade()
                .is_some_and(|existing_epoll| existing_epoll.as_raw_fd() != epoll.as_raw_fd())
        });
    }
}

pub struct VhostKickEvent<'a> {
    vring: &'a Vring,
}

impl<'a> EventSource for VhostKickEvent<'a> {
    fn register_self(&self, epoll: &Arc<Epoll>, token: u64) -> io::Result<()> {
        if let Some(kick_efd) = self.vring.state.kick.load().as_ref() {
            register_kick(epoll.as_ref(), kick_efd.as_raw_fd(), token)?;
        }
        self.vring.add_epoll(Arc::downgrade(epoll), token);
        Ok(())
    }

    fn unregister_self(&self, epoll: &Arc<Epoll>) -> io::Result<()> {
        self.vring.remove_epoll(epoll.as_ref());
        if let Some(kick_efd) = self.vring.state.kick.load().as_ref() {
            unregister_kick(epoll.as_ref(), kick_efd.as_raw_fd())?;
        };
        Ok(())
    }
}

impl DeviceQueue for Vring {
    type Queue = Queue;
    
    type QueueGuard<'a> = MutexGuard<'a, Queue>;

    type KickEvent<'a> = VhostKickEvent<'a>;

    // We are always shared between the vhost-user-backend thread and device worker thread
    type SharedVariant = Self;

    fn try_lock_queue<'a>(&'a self) -> Option<Self::QueueGuard<'a>> {
        if !self.is_enabled() {
            return None;
        }
        // FIXME: provide 2 variants of this method? locking and non-locking?
        let queue = self.queue.lock().unwrap();
        /*let queue = match self.queue.try_lock() {
            Ok(queue) => queue,
            //TODO maybe we can have an error for this?, in any case we cannot continue, and this is fatal
            Err(e @ TryLockError::Poisoned(_)) => panic!("{e}"),
            Err(TryLockError::WouldBlock) => {
                log::warn!("try_lock_queue WouldBlock");
                return None;
            }
        };*/

        if self.is_enabled() && queue.ready() {
            Some(queue)
        } else {
            None
        }
    }

    fn upgrade_to_shared(self) -> Self::SharedVariant {
        self
    }

    fn signal_used_queue(&self) -> io::Result<()> {
        if let Some(call) = self.state.call.load().as_ref() {
            call.write(1)
        } else {
            Ok(())
        }
    }

    fn kick_event(&self) -> VhostKickEvent {
        VhostKickEvent { vring: &self }
    }

    /// This method acknowledges the kick event has been received and return true if the device
    /// can proceed to process the queue. This call is non-blocking and can also be called without
    /// recieving a kick to see if the device can process the queue anyway.
    fn read_kick(&self) -> io::Result<bool> {
        let kick = self.state.kick.load();
        if let Some(kick) = kick.as_ref() {
            match kick.read() {
                Ok(n) => {
                    log::trace!("Read kick from fd: {}: {n}", kick.as_raw_fd())
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    log::trace!("Read kick from fd: {}, EAGAIN", kick.as_raw_fd())
                }
                Err(e) => return Err(e),
            }
        }

        Ok(self.is_enabled())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::io::AsRawFd;
    use vm_memory::bitmap::AtomicBitmap;
    use vmm_sys_util::eventfd::EventFd;

    #[test]
    fn test_new_vring() {
        let mem = GuestMemoryAtomic::new(
            GuestMemoryMmap::<AtomicBitmap>::from_ranges(&[(GuestAddress(0x100000), 0x10000)])
                .unwrap(),
        );
        let vring = VringMutex::new(mem, 0x1000).unwrap();

        assert!(vring.get_ref().get_kick().is_none());
        assert!(!vring.get_mut().enabled);
        assert!(!vring.lock().queue.ready());
        assert!(!vring.lock().queue.event_idx_enabled());

        vring.set_enabled(true);
        assert!(vring.get_ref().enabled);

        vring.set_queue_info(0x100100, 0x100200, 0x100300).unwrap();
        assert_eq!(vring.lock().get_queue().desc_table(), 0x100100);
        assert_eq!(vring.lock().get_queue().avail_ring(), 0x100200);
        assert_eq!(vring.lock().get_queue().used_ring(), 0x100300);

        assert_eq!(vring.queue_next_avail(), 0);
        vring.set_queue_next_avail(0x20);
        assert_eq!(vring.queue_next_avail(), 0x20);

        vring.set_queue_size(0x200);
        assert_eq!(vring.lock().queue.size(), 0x200);

        vring.set_queue_event_idx(true);
        assert!(vring.lock().queue.event_idx_enabled());

        vring.set_queue_ready(true);
        assert!(vring.lock().queue.ready());
    }

    #[test]
    fn test_vring_set_fd() {
        let mem = GuestMemoryAtomic::new(
            GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0x100000), 0x10000)]).unwrap(),
        );
        let vring = VringMutex::new(mem, 0x1000).unwrap();

        vring.set_enabled(true);
        assert!(vring.get_ref().enabled);

        let eventfd = EventFd::new(0).unwrap();
        // SAFETY: Safe because we panic before if eventfd is not valid.
        let file = unsafe { File::from_raw_fd(eventfd.as_raw_fd()) };
        assert!(vring.get_mut().kick.is_none());
        assert!(vring.read_kick().unwrap());
        vring.set_kick(Some(file));
        eventfd.write(1).unwrap();
        assert!(vring.read_kick().unwrap());
        assert!(vring.get_ref().kick.is_some());
        vring.set_kick(None);
        assert!(vring.get_ref().kick.is_none());
        std::mem::forget(eventfd);

        let eventfd = EventFd::new(0).unwrap();
        // SAFETY: Safe because we panic before if eventfd is not valid.
        let file = unsafe { File::from_raw_fd(eventfd.as_raw_fd()) };
        assert!(vring.get_ref().call.is_none());
        vring.set_call(Some(file));
        assert!(vring.get_ref().call.is_some());
        vring.set_call(None);
        assert!(vring.get_ref().call.is_none());
        std::mem::forget(eventfd);

        let eventfd = EventFd::new(0).unwrap();
        // SAFETY: Safe because we panic before if eventfd is not valid.
        let file = unsafe { File::from_raw_fd(eventfd.as_raw_fd()) };
        assert!(vring.get_ref().err.is_none());
        vring.set_err(Some(file));
        assert!(vring.get_ref().err.is_some());
        vring.set_err(None);
        assert!(vring.get_ref().err.is_none());
        std::mem::forget(eventfd);
    }
}
