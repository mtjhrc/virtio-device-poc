// Copyright 2019 Intel Corporation. All Rights Reserved.
// Copyright 2019-2021 Alibaba Cloud. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0

use std::error;
use std::fs::File;
use std::io;
use std::ops::Deref;
#[cfg(feature = "postcopy")]
use std::os::fd::AsFd;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::bitmap::{BitmapReplace, MemRegionBitmap, MmapLogReg};
#[cfg(feature = "postcopy")]
use userfaultfd::{Uffd, UffdBuilder};
use vhost::vhost_user::message::{
    VhostTransferStateDirection, VhostTransferStatePhase, VhostUserConfigFlags, VhostUserLog,
    VhostUserMemoryRegion, VhostUserProtocolFeatures, VhostUserSingleMemoryRegion,
    VhostUserVirtioFeatures, VhostUserVringAddrFlags, VhostUserVringState,
};
use vhost::vhost_user::GpuBackend;
use vhost::vhost_user::{
    Backend, Error as VhostUserError, Result as VhostUserResult, VhostUserBackendReqHandlerMut,
};

use super::backend::VhostUserDevice;
use super::vring::Vring;
use super::GM;
use std::os::fd::AsFd;
use virtio::QueueArgs;
use virtio_bindings::bindings::virtio_ring::VIRTIO_RING_F_EVENT_IDX;
use virtio_queue::{Error as VirtQueError, QueueT};
use vm_memory::mmap::NewBitmap;
use vm_memory::{
    GuestAddress, GuestAddressSpace, GuestMemory, GuestMemoryMmap, GuestMemoryRegion,
    GuestRegionMmap,
};

// vhost in the kernel usually supports 509 mem slots.
// The 509 used to be the KVM limit, it supported 512, but 3 were used
// for internal purposes (nowadays, it supports more than that).
const MAX_MEM_SLOTS: u64 = 509;

#[derive(Debug)]
/// Errors related to vhost-user handler.
pub enum VhostUserHandlerError {
    /// Failed to create a `Vring`.
    CreateVring(VirtQueError),
    /// Could not find the mapping from memory regions.
    MissingMemoryMapping,
}

impl std::fmt::Display for VhostUserHandlerError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            VhostUserHandlerError::CreateVring(e) => {
                write!(f, "failed to create vring: {}", e)
            }
            VhostUserHandlerError::MissingMemoryMapping => write!(f, "Missing memory mapping"),
        }
    }
}

impl error::Error for VhostUserHandlerError {}

/// Result of vhost-user handler operations.
pub type VhostUserHandlerResult<T> = std::result::Result<T, VhostUserHandlerError>;

#[derive(Debug)]
struct AddrMapping {
    #[cfg(feature = "postcopy")]
    local_addr: u64,
    vmm_addr: u64,
    size: u64,
    gpa_base: u64,
}

pub struct VhostUserHandler<T: VhostUserDevice> {
    device: T,
    device_activated: bool,
    memory_updated: bool,
    requested_queues: Vec<QueueArgs>,
    owned: bool,
    features_acked: bool,
    acked_features: u64,
    acked_protocol_features: u64,
    mappings: Vec<AddrMapping>,
    atomic_mem: GM<T::Bitmap>,
    vrings: Vec<Vring>,
    #[cfg(feature = "postcopy")]
    uffd: Option<Uffd>,
}

impl<T> VhostUserHandler<T>
where
    T: VhostUserDevice + 'static,
    T::Bitmap: Clone + Send + 'static,
{
    pub(crate) fn new(device: T, atomic_mem: GM<T::Bitmap>) -> VhostUserHandlerResult<Self> {
        let requested_queues = device.queues().to_vec();

        let vrings = requested_queues
            .iter()
            .map(Vring::new)
            .collect::<Result<Vec<Vring>, VirtQueError>>()
            .map_err(VhostUserHandlerError::CreateVring)?;

        Ok(VhostUserHandler {
            device,
            device_activated: false,
            memory_updated: false,
            requested_queues,
            owned: false,
            features_acked: false,
            acked_features: 0,
            acked_protocol_features: 0,
            mappings: Vec::new(),
            atomic_mem,
            vrings,
            #[cfg(feature = "postcopy")]
            uffd: None,
        })
    }
}

impl<T: VhostUserDevice> VhostUserHandler<T> {
    #[allow(dead_code)]
    pub(crate) fn send_exit_event(&mut self) {
        self.device.stop()
    }

    fn vmm_va_to_gpa(&self, vmm_va: u64) -> VhostUserHandlerResult<u64> {
        for mapping in self.mappings.iter() {
            if vmm_va >= mapping.vmm_addr && vmm_va < mapping.vmm_addr + mapping.size {
                return Ok(vmm_va - mapping.vmm_addr + mapping.gpa_base);
            }
        }

        Err(VhostUserHandlerError::MissingMemoryMapping)
    }
}

impl<T> VhostUserHandler<T>
where
    T: VhostUserDevice,
{
    fn initialize_vring(&mut self, index: u8) -> VhostUserResult<()> {
        // If the vring wasn't initialized and we already have an EventFd for
        // VRING_KICK, initialize it now.
        log::trace!("initialize_vring");
        //let mut queue = self.vrings[index as usize].queue_mut();
        //dbg!(!queue.ready());

        if !self.vrings[index as usize].queue_mut().ready()
            && self.vrings[index as usize].has_kick_event()
        {
            log::trace!("calling vrings[{}].start", index);
            self.vrings[index as usize].start().unwrap();

            if !self.device_activated && self.memory_updated {
                self.device_activated = true;
                if let Err(e) = self.device.activate(
                    self.atomic_mem.clone(),
                    self.vrings.iter().cloned().enumerate().collect(),
                ) {
                    error!("Failed to activate device {}", e);
                    return Err(VhostUserError::BackendInternalError);
                }
            }
            log::trace!("vrings[{}] started", index);
        }
        log::trace!("vrings[{}] initialize_vring end", index);

        Ok(())
    }

    /// Helper to check if VirtioFeature enabled
    fn check_feature(&self, feat: VhostUserVirtioFeatures) -> VhostUserResult<()> {
        if self.acked_features & feat.bits() != 0 {
            Ok(())
        } else {
            Err(VhostUserError::InactiveFeature(feat))
        }
    }
}

impl<T: VhostUserDevice> VhostUserBackendReqHandlerMut for VhostUserHandler<T>
where
    T::Bitmap: BitmapReplace + NewBitmap + Clone,
{
    fn set_owner(&mut self) -> VhostUserResult<()> {
        log::trace!("set_owner");
        if self.owned {
            return Err(VhostUserError::InvalidOperation("already claimed"));
        }
        self.owned = true;
        Ok(())
    }

    fn reset_owner(&mut self) -> VhostUserResult<()> {
        log::trace!("reset_owner");
        self.owned = false;
        self.features_acked = false;
        self.acked_features = 0;
        self.acked_protocol_features = 0;
        Ok(())
    }

    fn reset_device(&mut self) -> VhostUserResult<()> {
        log::trace!("reset_device");
        // Disable all vrings
        for vring in self.vrings.iter_mut() {
            vring.set_enabled(false);
            vring.stop().unwrap();
        }

        // Reset device state, retain protocol state
        self.features_acked = false;
        self.acked_features = 0;
        self.device.reset().unwrap();
        Ok(())
    }

    fn get_features(&mut self) -> VhostUserResult<u64> {
        log::trace!("get_features");
        Ok(self.device.avail_features())
    }

    fn set_features(&mut self, features: u64) -> VhostUserResult<()> {
        log::trace!("set_features");

        if (features & !self.device.avail_features()) != 0 {
            log::error!("set_features: VhostUserError::InvalidParam");
            return Err(VhostUserError::InvalidParam);
        }

        self.acked_features = features;
        self.features_acked = true;

        // Upon receiving a `VHOST_USER_SET_FEATURES` message from the front-end without
        // `VHOST_USER_F_PROTOCOL_FEATURES` set, the back-end must enable all rings immediately.
        // While processing the rings (whether they are enabled or not), the back-end must support
        // changing some configuration aspects on the fly.
        // (see https://qemu-project.gitlab.io/qemu/interop/vhost-user.html#ring-states)
        //
        // Note: If `VHOST_USER_F_PROTOCOL_FEATURES` has been negotiated we must leave
        // the vrings in their current state.
        if self.acked_features & VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits() == 0 {
            for vring in self.vrings.iter_mut() {
                vring.set_enabled(true);
            }
        }

        let event_idx: bool = (self.acked_features & (1 << VIRTIO_RING_F_EVENT_IDX)) != 0;
        for vring in self.vrings.iter_mut() {
            vring.queue_mut().set_event_idx(event_idx);
        }
        self.device.set_acked_features(self.acked_features);

        Ok(())
    }

    fn set_mem_table(
        &mut self,
        ctx: &[VhostUserMemoryRegion],
        files: Vec<File>,
    ) -> VhostUserResult<()> {
        log::trace!("set_mem_table");
        // We need to create tuple of ranges from the list of VhostUserMemoryRegion
        // that we get from the caller.
        let mut regions = Vec::new();
        let mut mappings: Vec<AddrMapping> = Vec::new();

        for (region, file) in ctx.iter().zip(files) {
            let guest_region = GuestRegionMmap::new(
                region.mmap_region(file)?,
                GuestAddress(region.guest_phys_addr),
            )
            .map_err(|e| {
                VhostUserError::ReqHandlerError(io::Error::new(io::ErrorKind::Other, e))
            })?;
            mappings.push(AddrMapping {
                #[cfg(feature = "postcopy")]
                local_addr: guest_region.as_ptr() as u64,
                vmm_addr: region.user_addr,
                size: region.memory_size,
                gpa_base: region.guest_phys_addr,
            });
            regions.push(guest_region);
        }

        log::trace!("set_mem_table: got regions");

        let mem = GuestMemoryMmap::from_regions(regions).map_err(|e| {
            VhostUserError::ReqHandlerError(io::Error::new(io::ErrorKind::Other, e))
        })?;

        // Updating the inner GuestMemory object here will cause all our vrings to
        // see the new one the next time they call to `atomic_mem.memory()`.
        self.atomic_mem.lock().unwrap().replace(mem);
        /*
        self.device
            .update_memory(self.atomic_mem.clone())
            .map_err(|e| {
                VhostUserError::ReqHandlerError(io::Error::new(io::ErrorKind::Other, e))
            })?;
        */
        self.memory_updated = true;
        self.mappings = mappings;
        log::trace!("set_mem_table: ok");
        Ok(())
    }

    fn set_vring_num(&mut self, index: u32, num: u32) -> VhostUserResult<()> {
        log::trace!("set_vring_num");
        let vring = self
            .vrings
            .get_mut(index as usize)
            .ok_or(VhostUserError::InvalidParam)?;

        if num == 0 || num > self.requested_queues[index as usize].max_size() as u32 {
            log::error!(
                "{num} > {}",
                self.requested_queues[index as usize].max_size()
            );
            return Err(VhostUserError::InvalidParam);
        }

        vring.queue_mut().set_size(num as u16);
        log::trace!("set_vring_num: {index}, {num}");
        Ok(())
    }

    fn set_vring_addr(
        &mut self,
        index: u32,
        _flags: VhostUserVringAddrFlags,
        descriptor: u64,
        used: u64,
        available: u64,
        _log: u64,
    ) -> VhostUserResult<()> {
        log::trace!("set_vring_addr");
        if !self.mappings.is_empty() {
            let desc_table = self.vmm_va_to_gpa(descriptor).map_err(|e| {
                VhostUserError::ReqHandlerError(io::Error::new(io::ErrorKind::Other, e))
            })?;
            let avail_ring = self.vmm_va_to_gpa(available).map_err(|e| {
                VhostUserError::ReqHandlerError(io::Error::new(io::ErrorKind::Other, e))
            })?;
            let used_ring = self.vmm_va_to_gpa(used).map_err(|e| {
                VhostUserError::ReqHandlerError(io::Error::new(io::ErrorKind::Other, e))
            })?;

            let vring = self
                .vrings
                .get_mut(index as usize)
                .ok_or(VhostUserError::InvalidParam)?;

            vring
                .set_queue_info(desc_table, avail_ring, used_ring)
                .map_err(|_| VhostUserError::InvalidParam)?;

            // SET_VRING_BASE will only restore the 'avail' index, however, after the guest driver
            // changes, for instance, after reboot, the 'used' index should be reset to 0.
            //
            // So let's fetch the used index from the vring as set by the guest here to keep
            // compatibility with the QEMU's vhost-user library just in case, any implementation
            // expects the 'used' index to be set when receiving a SET_VRING_ADDR message.
            //
            // Note: I'm not sure why QEMU's vhost-user library sets the 'user' index here,
            // _probably_ to make sure that the VQ is already configured. A better solution would
            // be to receive the 'used' index in SET_VRING_BASE, as is done when using packed VQs.
            let idx = vring
                .queue_mut()
                .used_idx(self.atomic_mem.memory().deref(), Ordering::Relaxed)
                .map_err(|_| VhostUserError::BackendInternalError)?;
            vring.queue_mut().set_next_used(idx.0);

            Ok(())
        } else {
            Err(VhostUserError::InvalidParam)
        }
    }

    fn set_vring_base(&mut self, index: u32, base: u32) -> VhostUserResult<()> {
        log::trace!("set_vring_base");
        let vring = self
            .vrings
            .get_mut(index as usize)
            .ok_or(VhostUserError::InvalidParam)?;

        vring.queue_mut().set_next_avail(base as u16);

        Ok(())
    }

    fn get_vring_base(&mut self, index: u32) -> VhostUserResult<VhostUserVringState> {
        log::trace!("get_vring_base");
        let vring = self
            .vrings
            .get_mut(index as usize)
            .ok_or(VhostUserError::InvalidParam)?;

        // Quote from vhost-user specification:
        // Client must start ring upon receiving a kick (that is, detecting
        // that file descriptor is readable) on the descriptor specified by
        // VHOST_USER_SET_VRING_KICK, and stop ring upon receiving
        // VHOST_USER_GET_VRING_BASE.
        vring.stop().unwrap();

        let next_avail = vring.queue_mut().next_avail();

        vring.set_kick(None).unwrap();
        vring.set_call(None);

        Ok(VhostUserVringState::new(index, u32::from(next_avail)))
    }

    fn set_vring_kick(&mut self, index: u8, file: Option<File>) -> VhostUserResult<()> {
        log::trace!("set_vring_kick");
        let vring = self
            .vrings
            .get(index as usize)
            .ok_or(VhostUserError::InvalidParam)?;

        // SAFETY: EventFd requires that it has sole ownership of its fd. So
        // does File, so this is safe.
        // Ideally, we'd have a generic way to refer to a uniquely-owned fd,
        // such as that proposed by Rust RFC #3128.
        vring.set_kick(file).unwrap();

        self.initialize_vring(index)?;

        Ok(())
    }

    fn set_vring_call(&mut self, index: u8, file: Option<File>) -> VhostUserResult<()> {
        log::trace!("set_vring_call");
        let vring = self
            .vrings
            .get(index as usize)
            .ok_or(VhostUserError::InvalidParam)?;

        vring.set_call(file);
        self.initialize_vring(index)?;

        Ok(())
    }

    fn set_vring_err(&mut self, index: u8, file: Option<File>) -> VhostUserResult<()> {
        log::trace!("set_vring_err");
        let vring = self
            .vrings
            .get(index as usize)
            .ok_or(VhostUserError::InvalidParam)?;

        vring.set_err(file);

        Ok(())
    }

    fn get_protocol_features(&mut self) -> VhostUserResult<VhostUserProtocolFeatures> {
        log::trace!("get_protocol_features");
        Ok(self.device.protocol_features())
    }

    fn set_protocol_features(&mut self, features: u64) -> VhostUserResult<()> {
        log::trace!("set_protocol_features");
        // Note: backend that reported VHOST_USER_F_PROTOCOL_FEATURES must
        // support this message even before VHOST_USER_SET_FEATURES was
        // called.
        self.acked_protocol_features = features;
        Ok(())
    }

    fn get_queue_num(&mut self) -> VhostUserResult<u64> {
        log::trace!("get_queue_num");
        Ok(self.requested_queues.len() as u64)
    }

    fn set_vring_enable(&mut self, index: u32, enable: bool) -> VhostUserResult<()> {
        log::trace!("set_vring_enable");
        // This request should be handled only when VHOST_USER_F_PROTOCOL_FEATURES
        // has been negotiated.
        self.check_feature(VhostUserVirtioFeatures::PROTOCOL_FEATURES)?;

        let vring = self
            .vrings
            .get(index as usize)
            .ok_or(VhostUserError::InvalidParam)?;

        // Backend must not pass data to/from the backend until ring is
        // enabled by VHOST_USER_SET_VRING_ENABLE with parameter 1,
        // or after it has been disabled by VHOST_USER_SET_VRING_ENABLE
        // with parameter 0.
        vring.set_enabled(enable);

        Ok(())
    }

    fn get_config(
        &mut self,
        offset: u32,
        size: u32,
        _flags: VhostUserConfigFlags,
    ) -> VhostUserResult<Vec<u8>> {
        log::trace!("get_config");
        let mut data = vec![0u8; size as usize];
        self.device
            .read_config(offset as usize, &mut data)
            .map_err(|_| VhostUserError::BackendInternalError)?; // FIXME: wrong error?
        Ok(data)
    }

    fn set_config(
        &mut self,
        offset: u32,
        buf: &[u8],
        _flags: VhostUserConfigFlags,
    ) -> VhostUserResult<()> {
        log::trace!("set_config");
        self.device
            .write_config(offset as usize, buf)
            .map_err(|_| VhostUserError::BackendInternalError)?; // FIXME: wrong error?
        Ok(())
    }

    fn set_backend_req_fd(&mut self, backend: Backend) {
        log::trace!("set_backend_req_fd");
        if self.acked_protocol_features & VhostUserProtocolFeatures::REPLY_ACK.bits() != 0 {
            backend.set_reply_ack_flag(true);
        }
        if self.acked_protocol_features & VhostUserProtocolFeatures::SHARED_OBJECT.bits() != 0 {
            backend.set_shared_object_flag(true);
        }
        /*
        self.device.set_backend_req_fd(backend);
        */
    }

    fn set_gpu_socket(&mut self, gpu_backend: GpuBackend) -> VhostUserResult<()> {
        self.device.set_gpu_socket(gpu_backend);
        Ok(())
    }

    fn get_inflight_fd(
        &mut self,
        _inflight: &vhost::vhost_user::message::VhostUserInflight,
    ) -> VhostUserResult<(vhost::vhost_user::message::VhostUserInflight, File)> {
        // Assume the backend hasn't negotiated the inflight feature; it
        // wouldn't be correct for the backend to do so, as we don't (yet)
        // provide a way for it to handle such requests.
        Err(VhostUserError::InvalidOperation("not supported"))
    }

    fn set_inflight_fd(
        &mut self,
        _inflight: &vhost::vhost_user::message::VhostUserInflight,
        _file: File,
    ) -> VhostUserResult<()> {
        Err(VhostUserError::InvalidOperation("not supported"))
    }

    fn get_max_mem_slots(&mut self) -> VhostUserResult<u64> {
        Ok(MAX_MEM_SLOTS)
    }

    fn add_mem_region(
        &mut self,
        region: &VhostUserSingleMemoryRegion,
        file: File,
    ) -> VhostUserResult<()> {
        let guest_region = Arc::new(
            GuestRegionMmap::new(
                region.mmap_region(file)?,
                GuestAddress(region.guest_phys_addr),
            )
            .map_err(|e| {
                VhostUserError::ReqHandlerError(io::Error::new(io::ErrorKind::Other, e))
            })?,
        );

        let addr_mapping = AddrMapping {
            #[cfg(feature = "postcopy")]
            local_addr: guest_region.as_ptr() as u64,
            vmm_addr: region.user_addr,
            size: region.memory_size,
            gpa_base: region.guest_phys_addr,
        };

        let mem = self
            .atomic_mem
            .memory()
            .insert_region(guest_region)
            .map_err(|e| {
                VhostUserError::ReqHandlerError(io::Error::new(io::ErrorKind::Other, e))
            })?;

        self.atomic_mem.lock().unwrap().replace(mem);
        /*
        self.device
            .update_memory(self.atomic_mem.clone())
            .map_err(|e| {
                VhostUserError::ReqHandlerError(io::Error::new(io::ErrorKind::Other, e))
            })?;
        */
        self.mappings.push(addr_mapping);

        Ok(())
    }

    fn remove_mem_region(&mut self, region: &VhostUserSingleMemoryRegion) -> VhostUserResult<()> {
        let (mem, _) = self
            .atomic_mem
            .memory()
            .remove_region(GuestAddress(region.guest_phys_addr), region.memory_size)
            .map_err(|e| {
                VhostUserError::ReqHandlerError(io::Error::new(io::ErrorKind::Other, e))
            })?;

        self.atomic_mem.lock().unwrap().replace(mem);
        /*
        self.device
            .update_memory(self.atomic_mem.clone())
            .map_err(|e| {
                VhostUserError::ReqHandlerError(io::Error::new(io::ErrorKind::Other, e))
            })?;
        */
        self.mappings
            .retain(|mapping| mapping.gpa_base != region.guest_phys_addr);

        Ok(())
    }

    fn set_device_state_fd(
        &mut self,
        _direction: VhostTransferStateDirection,
        _phase: VhostTransferStatePhase,
        _file: File,
    ) -> VhostUserResult<Option<File>> {
        todo!()
        /*self.device
        .set_device_state_fd(direction, phase, file)
        .map_err(VhostUserError::ReqHandlerError)*/
    }

    fn check_device_state(&mut self) -> VhostUserResult<()> {
        todo!()
        /*self.device
        .check_device_state()
        .map_err(VhostUserError::ReqHandlerError)*/
    }

    #[cfg(feature = "postcopy")]
    fn postcopy_advice(&mut self) -> VhostUserResult<File> {
        let mut uffd_builder = UffdBuilder::new();

        let uffd = uffd_builder
            .close_on_exec(true)
            .non_blocking(true)
            .user_mode_only(false)
            .create()
            .map_err(|e| {
                VhostUserError::ReqHandlerError(io::Error::new(io::ErrorKind::Other, e))
            })?;

        // We need to duplicate the uffd fd because we need both
        // to return File with fd and store fd inside uffd.
        //
        // SAFETY:
        // We know that uffd is correctly created.
        // This means fd inside uffd is also a valid fd.
        // Duplicating a valid fd is safe.
        let uffd_dup = unsafe { libc::dup(uffd.as_raw_fd()) };
        if uffd_dup < 0 {
            return Err(VhostUserError::ReqHandlerError(io::Error::last_os_error()));
        }

        // SAFETY:
        // We know that uffd_dup is a valid fd.
        let uffd_file = unsafe { File::from_raw_fd(uffd_dup) };

        self.uffd = Some(uffd);

        Ok(uffd_file)
    }

    #[cfg(feature = "postcopy")]
    fn postcopy_listen(&mut self) -> VhostUserResult<()> {
        let Some(ref uffd) = self.uffd else {
            return Err(VhostUserError::ReqHandlerError(io::Error::new(
                io::ErrorKind::Other,
                "No registered UFFD handler",
            )));
        };

        for mapping in self.mappings.iter() {
            uffd.register(
                mapping.local_addr as *mut libc::c_void,
                mapping.size as usize,
            )
            .map_err(|e| {
                VhostUserError::ReqHandlerError(io::Error::new(io::ErrorKind::Other, e))
            })?;
        }

        Ok(())
    }

    #[cfg(feature = "postcopy")]
    fn postcopy_end(&mut self) -> VhostUserResult<()> {
        self.uffd = None;
        Ok(())
    }

    // Sets logging (i.e., bitmap) shared memory space.
    //
    // During live migration, the front-end may need to track the modifications the back-end
    // makes to the memory mapped regions. The front-end should mark the dirty pages in a log.
    // Once it complies to this logging, it may declare the `VHOST_F_LOG_ALL` vhost feature.
    //
    // If the backend has the `VHOST_USER_PROTOCOL_F_LOG_SHMFD` protocol feature it may receive
    // the `VHOST_USER_SET_LOG_BASE` message. The log memory file descriptor is provided in `file`,
    // the size and offset of shared memory area are provided in the `VhostUserLog` message.
    //
    // See https://qemu-project.gitlab.io/qemu/interop/vhost-user.html#migration.
    // TODO: We ignore the `LOG_ALL` flag on `SET_FEATURES`, so we will continue marking pages as
    // dirty even if the migration fails. We need to disable the logging after receiving  a
    // `SET_FEATURE` without the `LOG_ALL` flag.
    fn set_log_base(&mut self, log: &VhostUserLog, file: File) -> VhostUserResult<()> {
        let mem = self.atomic_mem.memory();

        let logmem = Arc::new(
            MmapLogReg::from_file(file.as_fd(), log.mmap_offset, log.mmap_size)
                .map_err(VhostUserError::ReqHandlerError)?,
        );

        // Let's create all bitmaps first before replacing them, in case any of them fails
        let mut bitmaps = Vec::new();
        for region in mem.iter() {
            let bitmap = <<T as VhostUserDevice>::Bitmap as BitmapReplace>::InnerBitmap::new(
                region,
                Arc::clone(&logmem),
            )
            .map_err(VhostUserError::ReqHandlerError)?;

            bitmaps.push((region, bitmap));
        }

        for (region, bitmap) in bitmaps {
            region.bitmap().replace(bitmap);
        }

        Ok(())
    }
}

impl<T: VhostUserDevice> Drop for VhostUserHandler<T> {
    fn drop(&mut self) {
        self.device.stop();
    }
}
