// Copyright 2019 Intel Corporation. All Rights Reserved.
// Copyright 2019-2021 Alibaba Cloud. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;
use vhost::vhost_user::message::VhostUserProtocolFeatures;
use vhost::vhost_user::{GpuBackend, VhostUserVirtioFeatures};
use vm_memory::bitmap::{Bitmap, BitmapSlice};

use crate::Vring;
use virtio::{QueueArgs, VirtioDevice};
use vm_memory::{GuestMemoryAtomic, GuestMemoryMmap};

pub trait VhostUserDevice: Send {
    type Bitmap: Bitmap + 'static + Send + Sync + Clone;

    /// Get available vhost-user protocol features.
    fn protocol_features(&self) -> VhostUserProtocolFeatures;

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

    /// Activate the device. Upon activation the device can spawn worker thread(s) to handle
    // TODO: change BTreeMap<usize, Q> to [Option<Q>] ? This might be more efficient? Or let the device
    // itself do this?
    fn activate(
        &mut self,
        mem: GuestMemoryAtomic<GuestMemoryMmap<Self::Bitmap>>,
        queues: BTreeMap<usize, Vring>,
    ) -> anyhow::Result<()>;

    /// Stop device's worker threads.
    ///
    /// This may block to make sure the device can flush important data (e.g. disk writes).
    fn stop(&mut self);

    /// Stop device's worker threads and reset the device state.
    ///
    /// Implementation of this method is optional. Returns "true" if device reset is supported
    /// meaning the device can be activated again.
    fn reset(&mut self) -> anyhow::Result<bool>;

    fn set_gpu_socket(&mut self, gpu_backend: GpuBackend);
}

/// Utility to delegate the VhostUserDevice methods to a VirtioDevice, but also allow selectively
/// overriding any method.
pub trait VhostUserDeviceImplementer: AsRef<Self::Device> + AsMut<Self::Device> + Send {
    type Device: VirtioDevice<GuestMemoryAtomic<GuestMemoryMmap<Self::Bitmap>>, Vring>;
    type Bitmap: Bitmap + BitmapSlice + Send + Sync + 'static + Clone;

    fn protocol_features(&self) -> VhostUserProtocolFeatures;

    fn avail_features(&self) -> u64 {
        self.as_ref().avail_features() | VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits()
    }

    fn acked_features(&self) -> u64 {
        self.as_ref().acked_features() | VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits()
    }

    fn set_acked_features(&mut self, acked_features: u64) {
        // TODO: propagate the error instead?
        assert_eq!(
            acked_features & VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits(),
            VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits(),
            "The backend does not support vhost protocol features"
        );
        self.as_mut()
            .set_acked_features(acked_features ^ VhostUserVirtioFeatures::PROTOCOL_FEATURES.bits())
    }

    fn queues(&self) -> &[QueueArgs] {
        self.as_ref().queues()
    }

    fn read_config(&self, offset: usize, data: &mut [u8]) -> anyhow::Result<()> {
        self.as_ref().read_config(offset, data)
    }

    fn write_config(&mut self, offset: usize, data: &[u8]) -> anyhow::Result<()> {
        self.as_mut().write_config(offset, data)
    }

    fn activate(
        &mut self,
        mem: GuestMemoryAtomic<GuestMemoryMmap<Self::Bitmap>>,
        queues: BTreeMap<usize, Vring>,
    ) -> anyhow::Result<()> {
        self.as_mut().activate(mem, queues)
    }

    fn stop(&mut self) {
        self.as_mut().stop()
    }

    fn reset(&mut self) -> anyhow::Result<bool> {
        self.as_mut().reset()
    }

    fn set_gpu_socket(&mut self, _gpu_backend: GpuBackend) {
        // Generic virtio device doesn't have a gpu socket, let the user hook this up if necessary
    }
}

impl<I> VhostUserDevice for I
where
    //D: VirtioDevice<GuestMemoryAtomic<GuestMemoryMmap<Self::Bitmap>>, Vring>,
    I: VhostUserDeviceImplementer,
{
    type Bitmap = I::Bitmap;

    fn protocol_features(&self) -> VhostUserProtocolFeatures {
        I::protocol_features(self)
    }

    fn avail_features(&self) -> u64 {
        I::avail_features(self)
    }

    fn acked_features(&self) -> u64 {
        I::acked_features(self)
    }

    fn set_acked_features(&mut self, acked_features: u64) {
        I::set_acked_features(self, acked_features)
    }

    fn queues(&self) -> &[QueueArgs] {
        I::queues(self)
    }

    fn read_config(&self, offset: usize, data: &mut [u8]) -> anyhow::Result<()> {
        I::read_config(self, offset, data)
    }

    fn write_config(&mut self, offset: usize, data: &[u8]) -> anyhow::Result<()> {
        I::write_config(self, offset, data)
    }

    fn activate(
        &mut self,
        mem: GuestMemoryAtomic<GuestMemoryMmap<Self::Bitmap>>,
        queues: BTreeMap<usize, Vring>,
    ) -> anyhow::Result<()> {
        I::activate(self, mem, queues)
    }

    fn stop(&mut self) {
        I::stop(self)
    }

    fn reset(&mut self) -> anyhow::Result<bool> {
        I::reset(self)
    }

    fn set_gpu_socket(&mut self, gpu_backend: GpuBackend) {
        I::set_gpu_socket(self, gpu_backend)
    }
}
