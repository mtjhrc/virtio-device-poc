// vhost device Gpu
//
// Copyright 2024 RedHat
//
// SPDX-License-Identifier: Apache-2.0 or BSD-3-Clause

use crate::protocol::GpuCommandDecodeError;
use crate::worker::GpuWorker;
use crate::{
    protocol::{
        GpuResponseEncodeError, VirtioGpuConfig, CONTROL_QUEUE, CURSOR_QUEUE,
        VIRTIO_GPU_MAX_SCANOUTS,
    },
    virtio_gpu::RutabagaVirtioGpu,
    GpuConfig,
};
use anyhow::{bail, Context};
use log::{error, info, warn};
use std::collections::BTreeMap;
use std::thread::JoinHandle;
use std::{
    io::{self},
    sync::{self},
    thread,
};
use thiserror::Error as ThisError;
use vhost::vhost_user::GpuBackend;
use virtio::{BitmapOfAS, DeviceQueue, QueueArgs, VirtioDevice};
use virtio_bindings::{
    bindings::{
        virtio_config::{VIRTIO_F_NOTIFY_ON_EMPTY, VIRTIO_F_RING_RESET, VIRTIO_F_VERSION_1},
        virtio_ring::{VIRTIO_RING_F_EVENT_IDX, VIRTIO_RING_F_INDIRECT_DESC},
    },
    virtio_gpu::{
        VIRTIO_GPU_F_CONTEXT_INIT, VIRTIO_GPU_F_EDID, VIRTIO_GPU_F_RESOURCE_BLOB,
        VIRTIO_GPU_F_VIRGL,
    },
};
use vm_memory::bitmap::BitmapSlice;
use vm_memory::{ByteValued, GuestAddressSpace, Le32};
use vmm_sys_util::eventfd::{EventFd, EFD_NONBLOCK};

pub(crate) type Result<T> = std::result::Result<T, Error>;
#[derive(Debug, ThisError)]
pub enum Error {
    #[error("Failed to handle event, didn't match EPOLLIN")]
    HandleEventNotEpollIn,
    #[error("Failed to handle unknown event")]
    HandleEventUnknown,
    #[error("Descriptor read failed")]
    DescriptorReadFailed,
    #[error("Descriptor write failed")]
    DescriptorWriteFailed,
    #[error("Invalid command type {0}")]
    InvalidCommandType(u32),
    #[error("Failed to send used queue notification: {0}")]
    NotificationFailed(io::Error),
    #[error("Failed to create new EventFd")]
    EventFdFailed,
    #[error("Failed to create an iterator over a descriptor chain: {0}")]
    CreateIteratorDescChain(virtio_queue::Error),
    #[error("Failed to create descriptor chain Reader: {0}")]
    CreateReader(virtio_queue::Error),
    #[error("Failed to create descriptor chain Writer: {0}")]
    CreateWriter(virtio_queue::Error),
    #[error("Failed to decode gpu command: {0}")]
    GpuCommandDecode(GpuCommandDecodeError),
    #[error("Failed to encode gpu response: {0}")]
    GpuResponseEncode(GpuResponseEncodeError),
    #[error("Failed add used chain to queue: {0}")]
    QueueAddUsed(virtio_queue::Error),
    #[error("Epoll handler not available: {0}")]
    EpollHandler(String),
    #[error("Failed register epoll listener: {0}")]
    RegisterEpollListener(io::Error),
}

pub struct GpuDevice {
    virtio_cfg: VirtioGpuConfig,
    acked_features: u64,
    worker_thread: Option<JoinHandle<()>>,
    exit_event: EventFd,
    gpu_backend: Option<GpuBackend>,
    gpu_config: GpuConfig,
}

impl GpuDevice {
    pub fn new(gpu_config: GpuConfig) -> Result<Self> {
        info!(
            "GpuBackend using mode {} (capsets: '{}'), flags: {:?}",
            gpu_config.gpu_mode(),
            gpu_config.capsets(),
            gpu_config.flags()
        );

        let exit_event = EventFd::new(EFD_NONBLOCK).map_err(|_| Error::EventFdFailed)?;

        Ok(Self {
            virtio_cfg: VirtioGpuConfig {
                events_read: 0.into(),
                events_clear: 0.into(),
                num_scanouts: Le32::from(VIRTIO_GPU_MAX_SCANOUTS),
                num_capsets: Le32::from(gpu_config.capsets().num_capsets()),
            },
            acked_features: 0,
            worker_thread: None,
            exit_event,
            gpu_backend: None,
            gpu_config,
        })
    }

    pub fn set_gpu_socket(&mut self, gpu_backend: GpuBackend) {
        self.gpu_backend = Some(gpu_backend);
    }
}

impl<M, Q> VirtioDevice<M, Q> for GpuDevice
where
    M: GuestAddressSpace + Send + Clone + Sync + 'static,
    Q: DeviceQueue + Send + 'static,
    BitmapOfAS<M>: BitmapSlice,
{
    fn device_type(&self) -> u32 {
        todo!()
    }

    fn avail_features(&self) -> u64 {
        1 << VIRTIO_F_VERSION_1
            | 1 << VIRTIO_F_RING_RESET
            | 1 << VIRTIO_F_NOTIFY_ON_EMPTY
            | 1 << VIRTIO_RING_F_INDIRECT_DESC
            | 1 << VIRTIO_RING_F_EVENT_IDX
            | 1 << VIRTIO_GPU_F_VIRGL
            | 1 << VIRTIO_GPU_F_EDID
            | 1 << VIRTIO_GPU_F_RESOURCE_BLOB
            | 1 << VIRTIO_GPU_F_CONTEXT_INIT
    }

    fn acked_features(&self) -> u64 {
        self.acked_features
    }

    fn set_acked_features(&mut self, acked_features: u64) {
        self.acked_features =
            acked_features & <GpuDevice as VirtioDevice<M, Q>>::avail_features(self)
    }

    fn queues(&self) -> &[QueueArgs] {
        const QUEUES: &[QueueArgs] = &[QueueArgs::new(256), QueueArgs::new(256)];
        QUEUES
    }

    fn read_config(&self, offset: usize, data: &mut [u8]) -> anyhow::Result<()> {
        let offset = offset;

        let cfg_slice = &self.virtio_cfg.as_slice()[offset..];
        if cfg_slice.len() != data.len() {
            bail!("Invalid length");
        }
        data.copy_from_slice(&cfg_slice);

        Ok(())
    }

    fn write_config(&mut self, _offset: usize, _data: &[u8]) -> anyhow::Result<()> {
        bail!("Not supported");
    }

    fn activate(&mut self, mem: M, mut queues: BTreeMap<usize, Q>) -> anyhow::Result<()> {
        // Because GpuWorker needs to be initialized in the worker thread, we need this
        // channel to report any initialization failures cleanly
        let (tx, rx) = sync::mpsc::channel();

        let exit_event = self.exit_event.try_clone().context("Clone exit event")?;

        let control_queue = queues
            .remove(&CONTROL_QUEUE)
            .context("Missing control queue")?
            .upgrade_to_shared();
        let cursor_queue = queues
            .remove(&CURSOR_QUEUE)
            .context("Missing control queue")?;
        if !queues.is_empty() {
            warn!("Device activated with too many queues ({})", queues.len());
        }

        let Some(gpu_backend) = self.gpu_backend.take() else {
            bail!("Device cannot be activated because GpuBackend is missing");
        };
        let gpu_config = self.gpu_config.clone();

        self.worker_thread = Some(thread::spawn(move || {
            let gpu = RutabagaVirtioGpu::new(
                control_queue.clone(),
                mem.clone(),
                &gpu_config,
                gpu_backend,
            );
            let mut worker = match GpuWorker::new(gpu, mem, control_queue, cursor_queue, exit_event)
            {
                Ok(worker) => worker,
                Err(e) => {
                    tx.send(Err(e)).unwrap();
                    return;
                }
            };
            tx.send(Ok(())).unwrap();
            drop(tx);
            worker.run().unwrap()
        }));

        // Wait for initialization to report error/sucess
        rx.recv()?
    }

    fn stop(&mut self) {
        if let Some(worker_thread) = self.worker_thread.take() {
            self.exit_event.write(1).unwrap();
            worker_thread.join().unwrap();
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::{
        fs::File,
        iter::zip,
        mem,
        os::{fd::FromRawFd, unix::net::UnixStream},
    };

    use virtio_bindings::virtio_ring::VRING_DESC_F_NEXT;
    use virtio_queue::{mock::MockSplitQueue, Descriptor, QueueT};
    use vm_memory::{GuestAddress, GuestMemory, GuestMemoryAtomic, GuestMemoryMmap};

    use super::*;
    use crate::{GpuCapset, GpuFlags, GpuMode};

    const MEM_SIZE: usize = 2 * 1024 * 1024; // 2MiB

    const CURSOR_QUEUE_ADDR: GuestAddress = GuestAddress(0x0);
    const CURSOR_QUEUE_DATA_ADDR: GuestAddress = GuestAddress(0x1_000);
    const CURSOR_QUEUE_SIZE: u16 = 16;
    const CONTROL_QUEUE_ADDR: GuestAddress = GuestAddress(0x2_000);
    const CONTROL_QUEUE_DATA_ADDR: GuestAddress = GuestAddress(0x10_000);
    const CONTROL_QUEUE_SIZE: u16 = 1024;

    pub(crate) fn create_test_mem() -> GuestMemoryAtomic<GuestMemoryMmap> {
        GuestMemoryAtomic::new(
            GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), MEM_SIZE)]).unwrap(),
        )
    }

    fn init() -> (GpuDevice, GuestMemoryAtomic<GuestMemoryMmap>) {
        let config = GpuConfig::new(
            GpuMode::VirglRenderer,
            Some(GpuCapset::VIRGL | GpuCapset::VIRGL2),
            GpuFlags::default(),
        )
        .unwrap();
        let mem = create_test_mem();
        let device = GpuDevice::new(config).unwrap();

        (device, mem)
    }

    /// Arguments to create a descriptor chain for testing
    struct TestingDescChainArgs<'a> {
        readable_desc_bufs: &'a [&'a [u8]],
        writable_desc_lengths: &'a [u32],
    }

    fn gpu_backend_pair() -> (UnixStream, GpuBackend) {
        let (frontend, backend) = UnixStream::pair().unwrap();
        let backend = GpuBackend::from_stream(backend);
        MockSplitQueue::build_multiple_desc_chains()(frontend, backend)
    }

    fn event_fd_into_file(event_fd: EventFd) -> File {
        // SAFETY: We ensure that the `event_fd` is properly handled such that its file
        // descriptor is not closed after `File` takes ownership of it.
        unsafe {
            let event_fd_raw = event_fd.as_raw_fd();
            mem::forget(event_fd);
            File::from_raw_fd(event_fd_raw)
        }
    }

    fn make_descriptors_into_a_chain(start_idx: u16, descriptors: &mut [Descriptor]) {
        let last_idx = start_idx + descriptors.len() as u16 - 1;

        for (idx, desc) in zip(start_idx.., descriptors.iter_mut()) {
            if idx == last_idx {
                desc.set_flags(desc.flags() & !VRING_DESC_F_NEXT as u16);
            } else {
                desc.set_flags(desc.flags() | VRING_DESC_F_NEXT as u16);
                desc.set_next(idx + 1);
            };
        }
    }
    /*
    // Creates a vring from the specified descriptor chains
    // For each created device-writable descriptor chain a Vec<(GuestAddress,
    // usize)> is returned representing the descriptors of that chain.
    fn create_vring(
        mem: &GuestMemoryAtomic<GuestMemoryMmap>,
        chains: &[TestingDescChainArgs],
        queue_addr_start: GuestAddress,
        data_addr_start: GuestAddress,
        queue_size: u16,
    ) -> (VringRwLock, Vec<Vec<GuestAddress>>, EventFd) {
        let mem_handle = mem.memory();
        mem.memory()
            .check_address(queue_addr_start)
            .expect("Invalid start adress");

        let mut output_bufs = Vec::new();
        let vq = MockSplitQueue::create(&*mem_handle, queue_addr_start, queue_size);
        // Address of the buffer associated with the descriptor
        let mut next_addr = data_addr_start.0;
        let mut chain_index_start = 0;
        let mut descriptors = Vec::new();

        for chain in chains {
            for buf in chain.readable_desc_bufs {
                mem.memory()
                    .check_address(GuestAddress(next_addr))
                    .expect("Readable descriptor's buffer address is not valid!");
                let desc = Descriptor::new(
                    next_addr,
                    buf.len()
                        .try_into()
                        .expect("Buffer too large to fit into descriptor"),
                    0,
                    0,
                );
                mem_handle.write(buf, desc.addr()).unwrap();
                descriptors.push(desc);
                next_addr += buf.len() as u64;
            }
            let mut writable_descriptor_adresses = Vec::new();
            for desc_len in chain.writable_desc_lengths.iter().copied() {
                mem.memory()
                    .check_address(GuestAddress(next_addr))
                    .expect("Writable descriptor's buffer address is not valid!");
                let desc = Descriptor::new(next_addr, desc_len, VRING_DESC_F_WRITE as u16, 0);
                writable_descriptor_adresses.push(desc.addr());
                descriptors.push(desc);
                next_addr += u64::from(desc_len);
            }
            output_bufs.push(writable_descriptor_adresses);
            make_descriptors_into_a_chain(
                chain_index_start as u16,
                &mut descriptors[chain_index_start..],
            );
            chain_index_start = descriptors.len();
        }

        assert!(descriptors.len() < queue_size as usize);
        if !descriptors.is_empty() {
            vq.build_multiple_desc_chains(&descriptors)
                .expect("Failed to build descriptor chain");
        }

        let queue: Queue = vq.create_queue().unwrap();
        let vring = VringRwLock::new(mem.clone(), queue_size).unwrap();
        let signal_used_queue_evt = EventFd::new(EFD_NONBLOCK).unwrap();
        let signal_used_queue_evt_clone = signal_used_queue_evt.try_clone().unwrap();
        vring
            .set_queue_info(queue.desc_table(), queue.avail_ring(), queue.used_ring())
            .unwrap();
        vring.set_call(Some(event_fd_into_file(signal_used_queue_evt_clone)));

        vring.set_enabled(true);
        vring.set_queue_ready(true);

        (vring, output_bufs, signal_used_queue_evt)
    }

    fn create_control_vring(
        mem: &GuestMemoryAtomic<GuestMemoryMmap>,
        chains: &[TestingDescChainArgs],
    ) -> (VringRwLock, Vec<Vec<GuestAddress>>, EventFd) {
        create_vring(
            mem,
            chains,
            CONTROL_QUEUE_ADDR,
            CONTROL_QUEUE_DATA_ADDR,
            CONTROL_QUEUE_SIZE,
        )
    }

    fn create_cursor_vring(
        mem: &GuestMemoryAtomic<GuestMemoryMmap>,
        chains: &[TestingDescChainArgs],
    ) -> (VringRwLock, Vec<Vec<GuestAddress>>, EventFd) {
        create_vring(
            mem,
            chains,
            CURSOR_QUEUE_ADDR,
            CURSOR_QUEUE_DATA_ADDR,
            CURSOR_QUEUE_SIZE,
        )
    }

    #[test]
    fn test_handle_event_executes_gpu_commands() {
        let (backend, mem) = init();
        backend.update_memory(mem.clone()).unwrap();
        let backend_inner = backend.inner.lock().unwrap();

        let hdr = virtio_gpu_ctrl_hdr {
            type_: VIRTIO_GPU_CMD_RESOURCE_CREATE_2D.into(),
            ..Default::default()
        };

        let cmd = virtio_gpu_resource_create_2d {
            resource_id: 1.into(),
            format: VIRTIO_GPU_FORMAT_R8G8B8A8_UNORM.into(),
            width: 1920.into(),
            height: 1080.into(),
        };

        let chain1 = TestingDescChainArgs {
            readable_desc_bufs: &[hdr.as_slice(), cmd.as_slice()],
            writable_desc_lengths: &[mem::size_of::<virtio_gpu_ctrl_hdr>() as u32],
        };

        let chain2 = TestingDescChainArgs {
            readable_desc_bufs: &[hdr.as_slice(), cmd.as_slice()],
            writable_desc_lengths: &[mem::size_of::<virtio_gpu_ctrl_hdr>() as u32],
        };

        let (control_vring, outputs, control_signal_used_queue_evt) =
            create_control_vring(&mem, &[chain1, chain2]);
        let (cursor_vring, _, cursor_signal_used_queue_evt) = create_cursor_vring(&mem, &[]);

        let mem = mem.memory().into_inner();

        let mut mock_gpu = MockVirtioGpu::new();
        let seq = &mut mockall::Sequence::new();

        mock_gpu
            .expect_force_ctx_0()
            .return_const(())
            .once()
            .in_sequence(seq);

        mock_gpu
            .expect_resource_create_3d()
            .with(predicate::eq(1), predicate::always())
            .returning(|_, _| Ok(OkNoData))
            .once()
            .in_sequence(seq);

        mock_gpu
            .expect_force_ctx_0()
            .return_const(())
            .once()
            .in_sequence(seq);

        mock_gpu
            .expect_resource_create_3d()
            .with(predicate::eq(1), predicate::always())
            .returning(|_, _| Err(ErrUnspec))
            .once()
            .in_sequence(seq);

        assert_eq!(
            cursor_signal_used_queue_evt.read().unwrap_err().kind(),
            ErrorKind::WouldBlock
        );

        backend_inner
            .handle_event(0, &mut mock_gpu, &[control_vring, cursor_vring])
            .unwrap();

        let expected_hdr1 = virtio_gpu_ctrl_hdr {
            type_: VIRTIO_GPU_RESP_OK_NODATA.into(),
            ..Default::default()
        };

        let expected_hdr2 = virtio_gpu_ctrl_hdr {
            type_: VIRTIO_GPU_RESP_ERR_UNSPEC.into(),
            ..Default::default()
        };
        control_signal_used_queue_evt
            .read()
            .expect("Expected device to signal used queue!");
        assert_eq!(
            cursor_signal_used_queue_evt.read().unwrap_err().kind(),
            ErrorKind::WouldBlock,
            "Unexpected signal_used_queue on cursor queue!"
        );

        let result_hdr1: virtio_gpu_ctrl_hdr = mem.memory().read_obj(outputs[0][0]).unwrap();
        assert_eq!(result_hdr1, expected_hdr1);

        let result_hdr2: virtio_gpu_ctrl_hdr = mem.memory().read_obj(outputs[1][0]).unwrap();
        assert_eq!(result_hdr2, expected_hdr2);
    }

    #[test]
    fn test_command_with_fence_ready_immediately() {
        const FENCE_ID: u64 = 123;

        let (backend, mem) = init();
        backend.update_memory(mem.clone()).unwrap();
        let backend_inner = backend.inner.lock().unwrap();

        let hdr = virtio_gpu_ctrl_hdr {
            type_: VIRTIO_GPU_CMD_TRANSFER_TO_HOST_3D.into(),
            flags: VIRTIO_GPU_FLAG_FENCE.into(),
            fence_id: FENCE_ID.into(),
            ctx_id: 0.into(),
            ring_idx: 0,
            padding: Default::default(),
        };

        let cmd = virtio_gpu_transfer_host_3d::default();

        let chain = TestingDescChainArgs {
            readable_desc_bufs: &[hdr.as_slice(), cmd.as_slice()],
            writable_desc_lengths: &[mem::size_of::<virtio_gpu_ctrl_hdr>() as u32],
        };

        let (control_vring, outputs, control_signal_used_queue_evt) =
            create_control_vring(&mem, &[chain]);
        let (cursor_vring, _, _) = create_cursor_vring(&mem, &[]);

        let mut mock_gpu = MockVirtioGpu::new();
        let seq = &mut mockall::Sequence::new();

        mock_gpu
            .expect_force_ctx_0()
            .return_const(())
            .once()
            .in_sequence(seq);

        mock_gpu
            .expect_transfer_write()
            .returning(|_, _, _| Ok(OkNoData))
            .once()
            .in_sequence(seq);

        mock_gpu
            .expect_create_fence()
            .withf(|fence| fence.fence_id == FENCE_ID)
            .returning(|_| Ok(OkNoData))
            .once()
            .in_sequence(seq);

        mock_gpu
            .expect_process_fence()
            .with(
                predicate::eq(VirtioGpuRing::Global),
                predicate::eq(FENCE_ID),
                predicate::eq(0),
                predicate::eq(mem::size_of_val(&hdr) as u32),
            )
            .return_const(true)
            .once()
            .in_sequence(seq);

        backend_inner
            .handle_event(0, &mut mock_gpu, &[control_vring, cursor_vring])
            .unwrap();

        let expected_hdr = virtio_gpu_ctrl_hdr {
            type_: VIRTIO_GPU_RESP_OK_NODATA.into(),
            flags: VIRTIO_GPU_FLAG_FENCE.into(),
            fence_id: FENCE_ID.into(),
            ctx_id: 0.into(),
            ring_idx: 0,
            padding: Default::default(),
        };

        control_signal_used_queue_evt
            .read()
            .expect("Expected device to call signal_used_queue!");

        let result_hdr1: virtio_gpu_ctrl_hdr = mem.memory().read_obj(outputs[0][0]).unwrap();
        assert_eq!(result_hdr1, expected_hdr);
    }

    #[test]
    fn test_command_with_fence_not_ready() {
        const FENCE_ID: u64 = 123;
        const CTX_ID: u32 = 1;
        const RING_IDX: u8 = 2;

        let (backend, mem) = init();
        backend.update_memory(mem.clone()).unwrap();
        let backend_inner = backend.inner.lock().unwrap();

        let hdr = virtio_gpu_ctrl_hdr {
            type_: VIRTIO_GPU_CMD_TRANSFER_FROM_HOST_3D.into(),
            flags: (VIRTIO_GPU_FLAG_FENCE | VIRTIO_GPU_FLAG_INFO_RING_IDX).into(),
            fence_id: FENCE_ID.into(),
            ctx_id: CTX_ID.into(),
            ring_idx: RING_IDX,
            padding: Default::default(),
        };

        let cmd = virtio_gpu_transfer_host_3d::default();

        let chain = TestingDescChainArgs {
            readable_desc_bufs: &[hdr.as_slice(), cmd.as_slice()],
            writable_desc_lengths: &[mem::size_of::<virtio_gpu_ctrl_hdr>() as u32],
        };

        let (control_vring, _, control_signal_used_queue_evt) =
            create_control_vring(&mem, &[chain]);
        let (cursor_vring, _, _) = create_cursor_vring(&mem, &[]);

        let mut mock_gpu = MockVirtioGpu::new();
        let seq = &mut mockall::Sequence::new();

        mock_gpu
            .expect_force_ctx_0()
            .return_const(())
            .once()
            .in_sequence(seq);

        mock_gpu
            .expect_transfer_read()
            .returning(|_, _, _, _| Ok(OkNoData))
            .once()
            .in_sequence(seq);

        mock_gpu
            .expect_create_fence()
            .withf(|fence| fence.fence_id == FENCE_ID)
            .returning(|_| Ok(OkNoData))
            .once()
            .in_sequence(seq);

        mock_gpu
            .expect_process_fence()
            .with(
                predicate::eq(VirtioGpuRing::ContextSpecific {
                    ctx_id: CTX_ID,
                    ring_idx: RING_IDX,
                }),
                predicate::eq(FENCE_ID),
                predicate::eq(0),
                predicate::eq(mem::size_of_val(&hdr) as u32),
            )
            .return_const(false)
            .once()
            .in_sequence(seq);

        backend_inner
            .handle_event(0, &mut mock_gpu, &[control_vring, cursor_vring])
            .unwrap();

        assert_eq!(
            control_signal_used_queue_evt.read().unwrap_err().kind(),
            ErrorKind::WouldBlock
        );
    }

    rusty_fork_test! {
        #[test]
        fn test_verify_backend() {
            let gpu_config = GpuConfig::new(GpuMode::VirglRenderer, None, GpuFlags::default()).unwrap();
            let backend = VhostUserGpuBackend::new(gpu_config).unwrap();

            assert_eq!(backend.num_queues(), NUM_QUEUES);
            assert_eq!(backend.max_queue_size(), QUEUE_SIZE);
            assert_eq!(backend.features(), 0x0101_7100_001B);
            assert_eq!(
                backend.protocol_features(),
                VhostUserProtocolFeatures::CONFIG | VhostUserProtocolFeatures::MQ
            );
            assert_eq!(backend.queues_per_thread(), vec![0xffff_ffff]);
            assert_eq!(backend.get_config(0, 0), Vec::<u8>::new());

            assert!(backend.inner.lock().unwrap().gpu_backend.is_none());
            backend.set_gpu_socket(gpu_backend_pair().1).unwrap();
            assert!(backend.inner.lock().unwrap().gpu_backend.is_some());

            backend.set_event_idx(true);
            assert!(backend.inner.lock().unwrap().event_idx_enabled);

            assert!(backend.exit_event(0).is_some());

            let mem = GuestMemoryAtomic::new(
                GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), 0x1000)]).unwrap(),
            );
            backend.update_memory(mem.clone()).unwrap();

            let vring = VringRwLock::new(mem, 0x1000).unwrap();
            vring.set_queue_info(0x100, 0x200, 0x300).unwrap();
            vring.set_queue_ready(true);

            assert_eq!(
                backend
                    .handle_event(0, EventSet::OUT, &[vring.clone()], 0)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::Other
            );

            assert_eq!(
                backend
                    .handle_event(1, EventSet::IN, &[vring.clone()], 0)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::Other
            );

            // Hit the loop part
            backend.set_event_idx(true);
            backend
                .handle_event(0, EventSet::IN, &[vring.clone()], 0)
                .unwrap();

            // Hit the non-loop part
            backend.set_event_idx(false);
            backend.handle_event(0, EventSet::IN, &[vring], 0).unwrap();
        }
    }

    mod test_image {
        use super::*;
        const GREEN_PIXEL: u32 = 0x00FF_00FF;
        const RED_PIXEL: u32 = 0x00FF_00FF;
        const BYTES_PER_PIXEL: usize = 4;

        pub fn write(mem: &GuestMemoryMmap, image_addr: GuestAddress, width: u32, height: u32) {
            let mut image_addr: u64 = image_addr.0;
            for i in 0..width * height {
                let pixel = if i % 2 == 0 { RED_PIXEL } else { GREEN_PIXEL };
                let pixel = pixel.to_be_bytes();

                mem.memory()
                    .write_slice(&pixel, GuestAddress(image_addr))
                    .unwrap();
                image_addr += BYTES_PER_PIXEL as u64;
            }
        }

        pub fn assert(data: &[u8], width: u32, height: u32) {
            assert_eq!(data.len(), (width * height) as usize * BYTES_PER_PIXEL);
            for (i, pixel) in data.chunks(BYTES_PER_PIXEL).enumerate() {
                let expected_pixel = if i % 2 == 0 { RED_PIXEL } else { GREEN_PIXEL };
                assert_eq!(
                    pixel,
                    expected_pixel.to_be_bytes(),
                    "Wrong pixel at index {i}"
                );
            }
        }
    }

    fn split_into_mem_entries(
        addr: GuestAddress,
        len: u32,
        chunk_size: u32,
    ) -> Vec<virtio_gpu_mem_entry> {
        let mut entries = Vec::new();
        let mut addr = addr.0;
        let mut remaining = len;

        while remaining >= chunk_size {
            entries.push(virtio_gpu_mem_entry {
                addr: addr.into(),
                length: chunk_size.into(),
                padding: Le32::default(),
            });
            addr += u64::from(chunk_size);
            remaining -= chunk_size;
        }

        if remaining != 0 {
            entries.push(virtio_gpu_mem_entry {
                addr: addr.into(),
                length: remaining.into(),
                padding: Le32::default(),
            });
        }

        entries
    }

    fn new_hdr(type_: u32) -> virtio_gpu_ctrl_hdr {
        virtio_gpu_ctrl_hdr {
            type_: type_.into(),
            ..Default::default()
        }
    }

    rusty_fork_test! {
        /// This test uses multiple gpu commands, it crates a resource, writes a test image into it and
        /// then present the display output.
        #[test]
        fn test_display_output() {
            const IMAGE_ADDR: GuestAddress = GuestAddress(0x30_000);
            const IMAGE_WIDTH: u32 = 640;
            const IMAGE_HEIGHT: u32 = 480;
            const RESP_SIZE: u32 = mem::size_of::<virtio_gpu_ctrl_hdr>() as u32;
            const EXPECTED_SCANOUT_REQUEST: VhostUserGpuScanout = VhostUserGpuScanout {
                scanout_id: 1,
                width: IMAGE_WIDTH,
                height: IMAGE_HEIGHT,
            };

            const EXPECTED_UPDATE_REQUEST: VhostUserGpuUpdate = VhostUserGpuUpdate {
                scanout_id: 1,
                x: 0,
                y: 0,
                width: IMAGE_WIDTH,
                height: IMAGE_HEIGHT,
            };

            let (backend, mem) = init();
            let (mut gpu_frontend, gpu_backend) = gpu_backend_pair();
            gpu_frontend
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            gpu_frontend
                .set_write_timeout(Some(Duration::from_secs(10)))
                .unwrap();

            backend.set_gpu_socket(gpu_backend).unwrap();

            // Unfortunately there is no way to crate a VringEpollHandler directly (the ::new is not public)
            // So we create a daemon to create the epoll handler for us here
            let daemon = VhostUserDaemon::new(
                "vhost-gpu-backend".to_string(),
                backend.clone(),
                mem.clone(),
            )
            .expect("Could not create daemon");
            let epoll_handlers = daemon.get_epoll_handlers();
            backend.set_epoll_handler(&epoll_handlers);
            mem::drop(daemon);

            let image_rect = virtio_gpu_rect {
                x: 0.into(),
                y: 0.into(),
                width: IMAGE_WIDTH.into(),
                height: IMAGE_HEIGHT.into(),
            };

            // Construct a command to create a resource
            let hdr = new_hdr(VIRTIO_GPU_CMD_RESOURCE_CREATE_2D);
            let cmd = virtio_gpu_resource_create_2d {
                resource_id: 1.into(),
                format: VIRTIO_GPU_FORMAT_R8G8B8A8_UNORM.into(), // RGBA8888
                width: IMAGE_WIDTH.into(),
                height: IMAGE_HEIGHT.into(),
            };
            let create_resource_cmd = TestingDescChainArgs {
                readable_desc_bufs: &[hdr.as_slice(), cmd.as_slice()],
                writable_desc_lengths: &[RESP_SIZE],
            };

            // Construct a command to attach backing memory location(s) to the resource
            let hdr = new_hdr(VIRTIO_GPU_CMD_RESOURCE_ATTACH_BACKING);
            let mem_entries = split_into_mem_entries(IMAGE_ADDR, IMAGE_WIDTH * IMAGE_HEIGHT * 4, 4096);
            let cmd = virtio_gpu_resource_attach_backing {
                resource_id: 1.into(),
                nr_entries: (mem_entries.len() as u32).into(),
            };
            let mut readable_desc_bufs = vec![hdr.as_slice(), cmd.as_slice()];
            readable_desc_bufs.extend(mem_entries.iter().map(ByteValued::as_slice));
            let attach_backing_cmd = TestingDescChainArgs {
                readable_desc_bufs: &readable_desc_bufs,
                writable_desc_lengths: &[RESP_SIZE],
            };

            // Construct a command to detach backing memory location(s) from the resource
            let hdr = new_hdr(VIRTIO_GPU_CMD_RESOURCE_DETACH_BACKING);
            let cmd = virtio_gpu_resource_detach_backing {
                resource_id: 1.into(),
                padding: Le32::default(),
            };
            let detach_backing_cmd = TestingDescChainArgs {
                readable_desc_bufs: &[hdr.as_slice(), cmd.as_slice()],
                writable_desc_lengths: &[RESP_SIZE],
            };

            // Construct a command to transfer the resource data from the attached memory to gpu
            let hdr = new_hdr(VIRTIO_GPU_CMD_TRANSFER_TO_HOST_2D);
            let cmd = virtio_gpu_transfer_to_host_2d {
                r: image_rect,
                offset: 0.into(),
                resource_id: 1.into(),
                padding: Le32::default(),
            };
            let transfer_to_host_cmd = TestingDescChainArgs {
                readable_desc_bufs: &[hdr.as_slice(), cmd.as_slice()],
                writable_desc_lengths: &[RESP_SIZE],
            };

            // Construct a command to transfer the resource data from the host gpu to the attached memory
            let hdr = new_hdr(VIRTIO_GPU_CMD_TRANSFER_FROM_HOST_3D);
            let cmd = virtio_gpu_transfer_host_3d::default();
            let transfer_from_host_cmd = TestingDescChainArgs {
                readable_desc_bufs: &[hdr.as_slice(), cmd.as_slice()],
                writable_desc_lengths: &[RESP_SIZE],
            };

            // Construct a command to create a context for the given ctx_id in the hdr
            let hdr = new_hdr(VIRTIO_GPU_CMD_CTX_CREATE);
            let cmd = virtio_gpu_ctx_create::default();
            let ctx_create_cmd = TestingDescChainArgs {
                readable_desc_bufs: &[hdr.as_slice(), cmd.as_slice()],
                writable_desc_lengths: &[RESP_SIZE],
            };

            // Construct a command to destroy a context for the given ctx_id in the hdr
            let hdr = new_hdr(VIRTIO_GPU_CMD_CTX_DESTROY);
            let cmd = virtio_gpu_ctx_destroy::default();
            let ctx_destroy_cmd = TestingDescChainArgs {
                readable_desc_bufs: &[hdr.as_slice(), cmd.as_slice()],
                writable_desc_lengths: &[RESP_SIZE],
            };

            // Construct a command to attach a context for the given ctx_id in the hdr
            let hdr = new_hdr(VIRTIO_GPU_CMD_CTX_ATTACH_RESOURCE);
            let cmd = virtio_gpu_ctx_resource::default();
            let ctx_attach_cmd = TestingDescChainArgs {
                readable_desc_bufs: &[hdr.as_slice(), cmd.as_slice()],
                writable_desc_lengths: &[RESP_SIZE],
            };

            // Construct a command to detach a context for the given ctx_id in the hdr
            let hdr = new_hdr(VIRTIO_GPU_CMD_CTX_DETACH_RESOURCE);
            let cmd = virtio_gpu_ctx_resource::default();
            let ctx_detach_cmd = TestingDescChainArgs {
                readable_desc_bufs: &[hdr.as_slice(), cmd.as_slice()],
                writable_desc_lengths: &[RESP_SIZE],
            };

            // Construct a command to set the scanout (display) output
            let hdr = new_hdr(VIRTIO_GPU_CMD_SET_SCANOUT);
            let cmd = virtio_gpu_set_scanout {
                r: image_rect,
                resource_id: 1.into(),
                scanout_id: 1.into(),
            };
            let set_scanout_cmd = TestingDescChainArgs {
                readable_desc_bufs: &[hdr.as_slice(), cmd.as_slice()],
                writable_desc_lengths: &[RESP_SIZE],
            };

            // Construct a command to flush the resource
            let hdr = new_hdr(VIRTIO_GPU_CMD_RESOURCE_FLUSH);
            let cmd = virtio_gpu_resource_flush {
                r: image_rect,
                resource_id: 1.into(),
                padding: Le32::default(),
            };
            let flush_resource_cmd = TestingDescChainArgs {
                readable_desc_bufs: &[hdr.as_slice(), cmd.as_slice()],
                writable_desc_lengths: &[RESP_SIZE],
            };

            // Create a control queue with all the commands defined above
            let commands = [
                create_resource_cmd,
                attach_backing_cmd,
                transfer_to_host_cmd,
                transfer_from_host_cmd,
                set_scanout_cmd,
                flush_resource_cmd,
                detach_backing_cmd,
                ctx_create_cmd,
                ctx_attach_cmd,
                ctx_detach_cmd,
                ctx_destroy_cmd,
            ];
            let (control_vring, _, _) = create_control_vring(&mem, &commands);

            // Create an empty cursor queue with no commands
            let (cursor_vring, _, _) = create_cursor_vring(&mem, &[]);

            // Write the test image in guest memory
            test_image::write(&mem.memory(), IMAGE_ADDR, IMAGE_WIDTH, IMAGE_HEIGHT);

            // This simulates the frontend vmm. Here we check the issued frontend requests and if the
            // output matches the test image.
            let frontend_thread = thread::spawn(move || {
                let mut scanout_request_hdr = [0; 12];
                let mut scanout_request = VhostUserGpuScanout::default();
                let mut update_request_hdr = [0; 12];
                let mut update_request = VhostUserGpuUpdate::default();
                let mut result_img = vec![0xdd; (IMAGE_WIDTH * IMAGE_HEIGHT * 4) as usize];

                gpu_frontend.read_exact(&mut scanout_request_hdr).unwrap();
                gpu_frontend
                    .read_exact(scanout_request.as_mut_slice())
                    .unwrap();
                gpu_frontend.read_exact(&mut update_request_hdr).unwrap();
                gpu_frontend
                    .read_exact(update_request.as_mut_slice())
                    .unwrap();
                gpu_frontend.read_exact(&mut result_img).unwrap();

                assert_eq!(scanout_request, EXPECTED_SCANOUT_REQUEST);
                assert_eq!(update_request, EXPECTED_UPDATE_REQUEST);
                test_image::assert(&result_img, IMAGE_WIDTH, IMAGE_HEIGHT);
            });

            backend
                .handle_event(0, EventSet::IN, &[control_vring, cursor_vring], 0)
                .unwrap();

            frontend_thread.join().unwrap();
        }
    }*/
}
