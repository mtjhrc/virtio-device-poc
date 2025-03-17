use crate::device;
use crate::device::Error;
use crate::protocol::GpuResponse::{ErrInvalidParameter, ErrUnspec};
use crate::protocol::{
    virtio_gpu_ctrl_hdr, virtio_gpu_ctx_create, virtio_gpu_get_edid, virtio_gpu_resource_create_2d,
    virtio_gpu_resource_create_3d, virtio_gpu_transfer_host_3d, virtio_gpu_transfer_to_host_2d,
    virtio_gpu_update_cursor, GpuCommand, GpuResponse,
    VirtioGpuResult, VIRTIO_GPU_FLAG_FENCE, VIRTIO_GPU_FLAG_INFO_RING_IDX,
};
use crate::virtio_gpu::{VirtioGpu, VirtioGpuRing};
use anyhow::Context;
use event_manager::{EventManager, EventToken};
use log::{debug, trace, warn};
use rutabaga_gfx::{
    ResourceCreate3D, RutabagaFence, RutabagaIovec, Transfer3D, RUTABAGA_PIPE_BIND_RENDER_TARGET,
    RUTABAGA_PIPE_TEXTURE_2D,
};
use std::ffi::c_void;
use vhost::vhost_user::gpu_message::{VhostUserGpuCursorPos, VhostUserGpuEdidRequest};
use virtio::{BitmapOfAS, DeviceQueue};
use virtio_queue::{QueueOwnedT, QueueT, Reader, Writer};
use vm_memory::bitmap::BitmapSlice;
use vm_memory::{GuestAddress, GuestAddressSpace, GuestMemory, Le32};
use vmm_sys_util::eventfd::EventFd;

#[derive(EventToken, Debug)]
enum WorkerToken {
    Quit,
    ControlQ,
    CursorQ,
    Poll,
}

pub struct GpuWorker<G, M, Q>
where
    G: VirtioGpu,
    M: GuestAddressSpace + 'static,
    Q: DeviceQueue + 'static,
    BitmapOfAS<M>: BitmapSlice,
{
    event_manager: EventManager<WorkerToken>,
    gpu: G,
    control_queue: Q::SharedVariant,
    cursor_queue: Q,
    mem: M,
}

impl<G, M, Q> GpuWorker<G, M, Q>
where
    G: VirtioGpu,
    M: GuestAddressSpace + 'static,
    Q: DeviceQueue + 'static,
    BitmapOfAS<M>: BitmapSlice,
{
    pub fn new(
        gpu: G,
        mem: M,
        control_queue: Q::SharedVariant,
        cursor_queue: Q,
        exit_evt: EventFd,
    ) -> anyhow::Result<Self> {
        let event_manager = EventManager::new().context("Create event manager")?;
        event_manager.add(&exit_evt, WorkerToken::Quit)?;
        event_manager.add(&control_queue.kick_event(), WorkerToken::ControlQ)?;
        event_manager.add(&cursor_queue.kick_event(), WorkerToken::CursorQ)?;

        if let Some(poll_event) = gpu.get_event_poll_fd() {
            event_manager.add(&poll_event, WorkerToken::Poll)?;
        }

        Ok(Self {
            event_manager,
            gpu,
            control_queue,
            cursor_queue,
            mem,
        })
    }

    pub fn run(&mut self) -> anyhow::Result<()> {
        loop {
            for event in self.event_manager.wait().context("wait for events")? {
                match event.token() {
                    WorkerToken::Quit => return Ok(()),
                    WorkerToken::ControlQ => {
                        let mem = self.mem.memory();
                        Self::handle_queue_event(&mut self.gpu, &*mem, &self.control_queue)?;
                    }
                    WorkerToken::CursorQ => {
                        let mem = self.mem.memory();
                        Self::handle_queue_event(&mut self.gpu, &*mem, &self.cursor_queue)?;
                    }
                    WorkerToken::Poll => {
                        self.gpu.event_poll();
                    }
                };
            }
        }
    }

    fn handle_queue_event(
        gpu: &mut G,
        mem: &M::M,
        device_queue: &impl DeviceQueue,
    ) -> anyhow::Result<()> {
        if !device_queue.read_kick()? {
            return Ok(());
        }

        let Some(mut queue) = device_queue.try_lock_queue() else {
            return Ok(());
        };

        if queue.event_idx_enabled() {
            // vm-virtio's Queue implementation only checks avail_index
            // once, so to properly support EVENT_IDX we need to keep
            // calling process_queue() until it stops finding new
            // requests on the queue.
            loop {
                queue.disable_notification(mem).unwrap();
                let notify = Self::process_queue(gpu, mem, &mut *queue)?;
                if notify {
                    device_queue.signal_used_queue().unwrap();
                }
                if !queue.enable_notification(mem).unwrap() {
                    break;
                }
            }
        } else {
            // Without EVENT_IDX, a single call is enough.
            Self::process_queue(gpu, mem, &mut *queue)?;
        };

        Ok(())
    }

    /// Process the requests in the vring and dispatch replies
    fn process_queue(gpu: &mut G, mem: &M::M, queue: &mut impl QueueOwnedT) -> device::Result<bool> {
        let mut signal_used_queue = false;

        while let Some(desc_chain) = queue.pop_descriptor_chain(mem) {
            let head_index = desc_chain.head_index();
            let mut reader = desc_chain
                .clone()
                .reader(&mem)
                .map_err(Error::CreateReader)?;
            let mut writer = desc_chain.writer(&mem).map_err(Error::CreateWriter)?;

            let used_queue =
                Self::process_queue_chain(gpu, mem, queue, head_index, &mut reader, &mut writer)?;

            signal_used_queue |= used_queue;
        }

        debug!("Processing control queue finished");

        Ok(signal_used_queue)
    }

    fn process_queue_chain(
        gpu: &mut G,
        mem: &M::M,
        queue: &mut impl QueueOwnedT,
        head_index: u16,
        reader: &mut Reader<BitmapOfAS<M>>,
        writer: &mut Writer<BitmapOfAS<M>>,
    ) -> device::Result<bool> {
        let mut response = ErrUnspec;

        let ctrl_hdr = match GpuCommand::decode(reader) {
            Ok((ctrl_hdr, gpu_cmd)) => {
                let cmd_name = gpu_cmd.command_name();
                let response_result = Self::process_gpu_command(gpu, &mem, ctrl_hdr, gpu_cmd);
                // Unwrap the response from inside Result and log information
                response = match response_result {
                    Ok(response) => response,
                    Err(response) => {
                        debug!("GpuCommand {cmd_name} failed: {response:?}");
                        response
                    }
                };
                Some(ctrl_hdr)
            }
            Err(e) => {
                warn!("Failed to decode GpuCommand: {e}");
                None
            }
        };

        if writer.available_bytes() == 0 {
            debug!("Command does not have descriptors for a response");
            queue
                .add_used(mem, head_index, 0)
                .map_err(Error::QueueAddUsed)?;
            return Ok(true);
        }

        let mut fence_id = 0;
        let mut ctx_id = 0;
        let mut flags = 0;
        let mut ring_idx = 0;

        if let Some(ctrl_hdr) = ctrl_hdr {
            if <Le32 as Into<u32>>::into(ctrl_hdr.flags) & VIRTIO_GPU_FLAG_FENCE != 0 {
                flags = ctrl_hdr.flags.into();
                fence_id = ctrl_hdr.fence_id.into();
                ctx_id = ctrl_hdr.ctx_id.into();
                ring_idx = ctrl_hdr.ring_idx;

                let fence = RutabagaFence {
                    flags,
                    fence_id,
                    ctx_id,
                    ring_idx,
                };
                if let Err(fence_response) = gpu.create_fence(fence) {
                    warn!(
                        "Failed to create fence: fence_id: {fence_id} fence_response: \
                         {fence_response}"
                    );
                    response = fence_response;
                }
            }
        }

        // Prepare the response now, even if it is going to wait until
        // fence is complete.
        let response_len = response
            .encode(flags, fence_id, ctx_id, ring_idx, writer)
            .map_err(Error::GpuResponseEncode)?;

        let add_to_queue = if flags & VIRTIO_GPU_FLAG_FENCE != 0 {
            let ring = match flags & VIRTIO_GPU_FLAG_INFO_RING_IDX {
                0 => VirtioGpuRing::Global,
                _ => VirtioGpuRing::ContextSpecific { ctx_id, ring_idx },
            };
            debug!("Trying to process_fence for the command");
            gpu.process_fence(ring, fence_id, head_index, response_len)
        } else {
            true
        };

        if add_to_queue {
            queue
                .add_used(mem, head_index, response_len)
                .map_err(Error::QueueAddUsed)?;
            trace!("add_used {} bytes", response_len);
        }
        Ok(add_to_queue)
    }

    fn process_gpu_command(
        virtio_gpu: &mut G,
        mem: &M::M,
        hdr: virtio_gpu_ctrl_hdr,
        cmd: GpuCommand,
    ) -> VirtioGpuResult {
        virtio_gpu.force_ctx_0();
        debug!("process_gpu_command: {cmd:?}");
        match cmd {
            GpuCommand::GetDisplayInfo => virtio_gpu.display_info(),
            GpuCommand::GetEdid(req) => Self::handle_get_edid(virtio_gpu, req),
            GpuCommand::ResourceCreate2d(req) => Self::handle_resource_create_2d(virtio_gpu, req),
            GpuCommand::ResourceUnref(req) => virtio_gpu.unref_resource(req.resource_id.into()),
            GpuCommand::SetScanout(req) => {
                virtio_gpu.set_scanout(req.scanout_id.into(), req.resource_id.into(), req.r.into())
            }
            GpuCommand::ResourceFlush(req) => {
                virtio_gpu.flush_resource(req.resource_id.into(), req.r.into())
            }
            GpuCommand::TransferToHost2d(req) => Self::handle_transfer_to_host_2d(virtio_gpu, req),
            GpuCommand::ResourceAttachBacking(req, iovecs) => {
                let iovecs = Self::rutabaga_iovecs_from_guest_mem(&iovecs, mem)?;
                virtio_gpu.attach_backing(req.resource_id.into(), iovecs)
            }
            GpuCommand::ResourceDetachBacking(req) => {
                virtio_gpu.detach_backing(req.resource_id.into())
            }
            GpuCommand::UpdateCursor(req) => Self::handle_update_cursor(virtio_gpu, req),
            GpuCommand::MoveCursor(req) => Self::handle_move_cursor(virtio_gpu, req),
            GpuCommand::ResourceAssignUuid(_) => {
                panic!("virtio_gpu: GpuCommand::ResourceAssignUuid unimplemented")
            }
            GpuCommand::GetCapsetInfo(req) => virtio_gpu.get_capset_info(req.capset_index.into()),
            GpuCommand::GetCapset(req) => {
                virtio_gpu.get_capset(req.capset_id.into(), req.capset_version.into())
            }
            GpuCommand::CtxCreate(req) => Self::handle_ctx_create(virtio_gpu, hdr, req),
            GpuCommand::CtxDestroy(_) => virtio_gpu.destroy_context(hdr.ctx_id.into()),
            GpuCommand::CtxAttachResource(req) => {
                virtio_gpu.context_attach_resource(hdr.ctx_id.into(), req.resource_id.into())
            }
            GpuCommand::CtxDetachResource(req) => {
                virtio_gpu.context_detach_resource(hdr.ctx_id.into(), req.resource_id.into())
            }
            GpuCommand::ResourceCreate3d(req) => Self::handle_resource_create_3d(virtio_gpu, req),
            GpuCommand::TransferToHost3d(req) => {
                Self::handle_transfer_to_host_3d(virtio_gpu, hdr.ctx_id.into(), req)
            }
            GpuCommand::TransferFromHost3d(req) => {
                Self::handle_transfer_from_host_3d(virtio_gpu, hdr.ctx_id.into(), req)
            }
            GpuCommand::CmdSubmit3d {
                fence_ids,
                mut cmd_data,
            } => virtio_gpu.submit_command(hdr.ctx_id.into(), &mut cmd_data, &fence_ids),
            GpuCommand::ResourceCreateBlob(_) => {
                panic!("virtio_gpu: GpuCommand::ResourceCreateBlob unimplemented")
            }
            GpuCommand::SetScanoutBlob(_) => {
                panic!("virtio_gpu: GpuCommand::SetScanoutBlob unimplemented")
            }
            GpuCommand::ResourceMapBlob(_) => {
                panic!("virtio_gpu: GpuCommand::ResourceMapBlob unimplemented")
            }
            GpuCommand::ResourceUnmapBlob(_) => {
                panic!("virtio_gpu: GpuCommand::ResourceUnmapBlob unimplemented")
            }
        }
    }

    fn handle_get_edid(virtio_gpu: &impl VirtioGpu, req: virtio_gpu_get_edid) -> VirtioGpuResult {
        let edid_req = VhostUserGpuEdidRequest {
            scanout_id: req.scanout.into(),
        };
        virtio_gpu.get_edid(edid_req)
    }

    fn handle_resource_create_2d(
        virtio_gpu: &mut impl VirtioGpu,
        req: virtio_gpu_resource_create_2d,
    ) -> VirtioGpuResult {
        let resource_create_3d = ResourceCreate3D {
            target: RUTABAGA_PIPE_TEXTURE_2D,
            format: req.format.into(),
            bind: RUTABAGA_PIPE_BIND_RENDER_TARGET,
            width: req.width.into(),
            height: req.height.into(),
            depth: 1,
            array_size: 1,
            last_level: 0,
            nr_samples: 0,
            flags: 0,
        };
        virtio_gpu.resource_create_3d(req.resource_id.into(), resource_create_3d)
    }

    fn handle_transfer_to_host_2d(
        virtio_gpu: &mut impl VirtioGpu,
        req: virtio_gpu_transfer_to_host_2d,
    ) -> VirtioGpuResult {
        let transfer = Transfer3D::new_2d(
            req.r.x.into(),
            req.r.y.into(),
            req.r.width.into(),
            req.r.height.into(),
            req.offset.into(),
        );
        virtio_gpu.transfer_write(0, req.resource_id.into(), transfer)
    }

    fn handle_update_cursor(
        virtio_gpu: &mut impl VirtioGpu,
        req: virtio_gpu_update_cursor,
    ) -> VirtioGpuResult {
        let cursor_pos = VhostUserGpuCursorPos {
            scanout_id: req.pos.scanout_id.into(),
            x: req.pos.x.into(),
            y: req.pos.y.into(),
        };
        virtio_gpu.update_cursor(
            req.resource_id.into(),
            cursor_pos,
            req.hot_x.into(),
            req.hot_y.into(),
        )
    }

    fn handle_move_cursor(
        virtio_gpu: &mut impl VirtioGpu,
        req: virtio_gpu_update_cursor,
    ) -> VirtioGpuResult {
        let cursor = VhostUserGpuCursorPos {
            scanout_id: req.pos.scanout_id.into(),
            x: req.pos.x.into(),
            y: req.pos.y.into(),
        };
        virtio_gpu.move_cursor(req.resource_id.into(), cursor)
    }

    fn handle_ctx_create(
        virtio_gpu: &mut impl VirtioGpu,
        hdr: virtio_gpu_ctrl_hdr,
        req: virtio_gpu_ctx_create,
    ) -> VirtioGpuResult {
        let context_name: Option<String> = Some(req.get_debug_name());
        virtio_gpu.create_context(
            hdr.ctx_id.into(),
            req.context_init.into(),
            context_name.as_deref(),
        )
    }

    fn handle_resource_create_3d(
        virtio_gpu: &mut impl VirtioGpu,
        req: virtio_gpu_resource_create_3d,
    ) -> VirtioGpuResult {
        let resource_create_3d = ResourceCreate3D {
            target: req.target.into(),
            format: req.format.into(),
            bind: req.bind.into(),
            width: req.width.into(),
            height: req.height.into(),
            depth: req.depth.into(),
            array_size: req.array_size.into(),
            last_level: req.last_level.into(),
            nr_samples: req.nr_samples.into(),
            flags: req.flags.into(),
        };
        virtio_gpu.resource_create_3d(req.resource_id.into(), resource_create_3d)
    }

    fn handle_transfer_to_host_3d(
        virtio_gpu: &mut impl VirtioGpu,
        ctx_id: u32,
        req: virtio_gpu_transfer_host_3d,
    ) -> VirtioGpuResult {
        let transfer = Transfer3D {
            x: req.box_.x.into(),
            y: req.box_.y.into(),
            z: req.box_.z.into(),
            w: req.box_.w.into(),
            h: req.box_.h.into(),
            d: req.box_.d.into(),
            level: req.level.into(),
            stride: req.stride.into(),
            layer_stride: req.layer_stride.into(),
            offset: req.offset.into(),
        };
        virtio_gpu.transfer_write(ctx_id, req.resource_id.into(), transfer)
    }

    fn handle_transfer_from_host_3d(
        virtio_gpu: &mut impl VirtioGpu,
        ctx_id: u32,
        req: virtio_gpu_transfer_host_3d,
    ) -> VirtioGpuResult {
        let transfer = Transfer3D {
            x: req.box_.x.into(),
            y: req.box_.y.into(),
            z: req.box_.z.into(),
            w: req.box_.w.into(),
            h: req.box_.h.into(),
            d: req.box_.d.into(),
            level: req.level.into(),
            stride: req.stride.into(),
            layer_stride: req.layer_stride.into(),
            offset: req.offset.into(),
        };
        virtio_gpu.transfer_read(ctx_id, req.resource_id.into(), transfer, None)
    }

    fn rutabaga_iovecs_from_guest_mem(
        vecs: &[(GuestAddress, usize)],
        mem: &M::M,
    ) -> Result<Vec<RutabagaIovec>, GpuResponse> {
        if vecs
            .iter()
            .any(|&(addr, len)| mem.get_slice(addr, len).is_err())
        {
            return Err(ErrInvalidParameter);
        }

        let mut rutabaga_iovecs: Vec<RutabagaIovec> = Vec::new();
        for &(addr, len) in vecs {
            let slice = mem.get_slice(addr, len).unwrap();
            rutabaga_iovecs.push(RutabagaIovec {
                base: slice.ptr_guard_mut().as_ptr().cast::<c_void>(),
                len,
            });
        }
        Ok(rutabaga_iovecs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::protocol::GpuResponse::{OkCapsetInfo, OkDisplayInfo, OkEdid, OkNoData};
    use crate::protocol::{
        virtio_gpu_ctrl_hdr, virtio_gpu_ctx_create, virtio_gpu_ctx_destroy,
        virtio_gpu_ctx_resource, virtio_gpu_get_capset_info, virtio_gpu_get_edid,
        virtio_gpu_resource_attach_backing, virtio_gpu_resource_create_2d,
        virtio_gpu_resource_create_3d, virtio_gpu_resource_detach_backing,
        virtio_gpu_resource_flush, virtio_gpu_resource_unref, virtio_gpu_set_scanout,
        virtio_gpu_transfer_host_3d, virtio_gpu_transfer_to_host_2d, virtio_gpu_update_cursor,
        GpuCommand,
    };
    use crate::virtio_gpu::MockVirtioGpu;
    use crate::worker::GpuWorker;
    use assert_matches::assert_matches;
    use vm_memory::{GuestAddressSpace, GuestMemoryAtomic};
    use virtio::mock::MockDeviceQueue;

    #[test]
    fn test_process_gpu_command() {
        let mem = device::tests::create_test_mem();
        let hdr = virtio_gpu_ctrl_hdr::default();

        let test_cmd = |cmd: GpuCommand, setup: fn(&mut MockVirtioGpu)| {
            let mut mock_gpu = MockVirtioGpu::new();
            mock_gpu.expect_force_ctx_0().return_once(|| ());
            setup(&mut mock_gpu);
            GpuWorker::<MockVirtioGpu, GuestMemoryAtomic<GuestMemoryMmap>, MockDeviceQueue>::process_gpu_command(&mut mock_gpu, &mem.memory(), hdr, cmd)
        };

        let cmd = GpuCommand::GetDisplayInfo;
        let result = test_cmd(cmd, |g| {
            g.expect_display_info()
                .return_once(|| Ok(OkDisplayInfo(vec![(1280, 720, true)])));
        });
        assert_matches!(result, Ok(OkDisplayInfo(_)));

        let cmd = GpuCommand::GetEdid(virtio_gpu_get_edid::default());
        let result = test_cmd(cmd, |g| {
            g.expect_get_edid().return_once(|_| {
                Ok(OkEdid {
                    blob: Box::new([0xff; 512]),
                })
            });
        });
        assert_matches!(result, Ok(OkEdid { .. }));

        let cmd = GpuCommand::ResourceCreate2d(virtio_gpu_resource_create_2d::default());
        let result = test_cmd(cmd, |g| {
            g.expect_resource_create_3d()
                .return_once(|_, _| Ok(OkNoData));
        });
        assert_matches!(result, Ok(OkNoData));

        let cmd = GpuCommand::ResourceUnref(virtio_gpu_resource_unref::default());
        let result = test_cmd(cmd, |g| {
            g.expect_unref_resource().return_once(|_| Ok(OkNoData));
        });
        assert_matches!(result, Ok(OkNoData));

        let cmd = GpuCommand::SetScanout(virtio_gpu_set_scanout::default());
        let result = test_cmd(cmd, |g| {
            g.expect_set_scanout().return_once(|_, _, _| Ok(OkNoData));
        });
        assert_matches!(result, Ok(OkNoData));

        let cmd = GpuCommand::ResourceFlush(virtio_gpu_resource_flush::default());
        let result = test_cmd(cmd, |g| {
            g.expect_flush_resource().return_once(|_, _| Ok(OkNoData));
        });
        assert_matches!(result, Ok(OkNoData));

        let cmd = GpuCommand::TransferToHost2d(virtio_gpu_transfer_to_host_2d::default());
        let result = test_cmd(cmd, |g| {
            g.expect_transfer_write()
                .return_once(|_, _, _| Ok(OkNoData));
        });
        assert_matches!(result, Ok(OkNoData));

        let cmd = GpuCommand::ResourceAttachBacking(
            virtio_gpu_resource_attach_backing::default(),
            Vec::default(),
        );
        let result = test_cmd(cmd, |g| {
            g.expect_attach_backing()
                .return_once(|_, _| Ok(OkNoData));
        });
        assert_matches!(result, Ok(OkNoData));

        let cmd = GpuCommand::ResourceDetachBacking(virtio_gpu_resource_detach_backing::default());
        let result = test_cmd(cmd, |g| {
            g.expect_detach_backing().return_once(|_| Ok(OkNoData));
        });
        assert_matches!(result, Ok(OkNoData));

        let cmd = GpuCommand::GetCapsetInfo(virtio_gpu_get_capset_info::default());
        let result = test_cmd(cmd, |g| {
            g.expect_get_capset_info().return_once(|_| {
                Ok(OkCapsetInfo {
                    capset_id: 1,
                    version: 2,
                    size: 32,
                })
            });
        });
        assert_matches!(
            result,
            Ok(OkCapsetInfo {
                capset_id: 1,
                version: 2,
                size: 32
            })
        );

        let cmd = GpuCommand::CtxCreate(virtio_gpu_ctx_create::default());
        let result = test_cmd(cmd, |g| {
            g.expect_create_context()
                .return_once(|_, _, _| Ok(OkNoData));
        });
        assert_matches!(result, Ok(OkNoData));

        let cmd = GpuCommand::CtxDestroy(virtio_gpu_ctx_destroy::default());
        let result = test_cmd(cmd, |g| {
            g.expect_destroy_context().return_once(|_| Ok(OkNoData));
        });
        assert_matches!(result, Ok(OkNoData));

        let cmd = GpuCommand::CtxAttachResource(virtio_gpu_ctx_resource::default());
        let result = test_cmd(cmd, |g| {
            g.expect_context_attach_resource()
                .return_once(|_, _| Ok(OkNoData));
        });
        assert_matches!(result, Ok(OkNoData));

        let cmd = GpuCommand::CtxDetachResource(virtio_gpu_ctx_resource::default());
        let result = test_cmd(cmd, |g| {
            g.expect_context_detach_resource()
                .return_once(|_, _| Ok(OkNoData));
        });
        assert_matches!(result, Ok(OkNoData));

        let cmd = GpuCommand::ResourceCreate3d(virtio_gpu_resource_create_3d::default());
        let result = test_cmd(cmd, |g| {
            g.expect_resource_create_3d()
                .return_once(|_, _| Ok(OkNoData));
        });
        assert_matches!(result, Ok(OkNoData));

        let cmd = GpuCommand::TransferToHost3d(virtio_gpu_transfer_host_3d::default());
        let result = test_cmd(cmd, |g| {
            g.expect_transfer_write()
                .return_once(|_, _, _| Ok(OkNoData));
        });
        assert_matches!(result, Ok(OkNoData));

        let cmd = GpuCommand::TransferFromHost3d(virtio_gpu_transfer_host_3d::default());
        let result = test_cmd(cmd, |g| {
            g.expect_transfer_read()
                .return_once(|_, _, _, _| Ok(OkNoData));
        });
        assert_matches!(result, Ok(OkNoData));

        let cmd = GpuCommand::CmdSubmit3d {
            cmd_data: vec![0xff; 512],
            fence_ids: vec![],
        };
        let result = test_cmd(cmd, |g| {
            g.expect_submit_command()
                .return_once(|_, _, _| Ok(OkNoData));
        });
        assert_matches!(result, Ok(OkNoData));

        let cmd = GpuCommand::UpdateCursor(virtio_gpu_update_cursor::default());
        let result = test_cmd(cmd, |g| {
            g.expect_update_cursor()
                .return_once(|_, _, _, _| Ok(OkNoData));
        });
        assert_matches!(result, Ok(OkNoData));

        let cmd = GpuCommand::MoveCursor(virtio_gpu_update_cursor::default());
        let result = test_cmd(cmd, |g| {
            g.expect_move_cursor().return_once(|_, _| Ok(OkNoData));
        });
        assert_matches!(result, Ok(OkNoData));

        let cmd = GpuCommand::MoveCursor(virtio_gpu_update_cursor::default());
        let result = test_cmd(cmd, |g| {
            g.expect_move_cursor().return_once(|_, _| Ok(OkNoData));
        });
        assert_matches!(result, Ok(OkNoData));
    }
}
