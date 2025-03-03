use crate::console_control::MainWorker;
use crate::port::{Port, PortDescription};
use crate::port_queue_mapping::num_queues;
use std::collections::BTreeMap;
use std::io::Write;
use std::iter::zip;
use std::marker::PhantomData;
use std::thread::JoinHandle;
use std::{cmp, mem, thread};
use virtio::{BitmapOfAS, DeviceQueue, QueueArgs, VirtioDevice};
use vm_memory::bitmap::BitmapSlice;
use vm_memory::{ByteValued, GuestAddressSpace};
use vmm_sys_util::eventfd::EventFd;

#[derive(Copy, Clone, Debug, Default)]
#[repr(C, packed)]
pub struct VirtioConsoleConfig {
    cols: u16,
    rows: u16,
    max_nr_ports: u32,
    emerg_wr: u32,
}

// Safe because it only has data and has no implicit padding.
unsafe impl ByteValued for VirtioConsoleConfig {}

impl VirtioConsoleConfig {
    pub fn new(cols: u16, rows: u16, max_nr_ports: u32) -> Self {
        VirtioConsoleConfig {
            cols,
            rows,
            max_nr_ports,
            emerg_wr: 0u32,
        }
    }
}

const AVAIL_FEATURES: u64 = 1 << uapi::VIRTIO_CONSOLE_F_SIZE as u64
    | 1 << uapi::VIRTIO_CONSOLE_F_MULTIPORT as u64
    | 1 << uapi::VIRTIO_F_VERSION_1 as u64;

const QUEUE_SIZE: u16 = 256;

pub(crate) const CONTROL_RXQ_INDEX: usize = 2;
pub(crate) const CONTROL_TXQ_INDEX: usize = 3;

pub mod uapi {
    /// The device conforms to the virtio spec version 1.0.
    pub const VIRTIO_CONSOLE_F_SIZE: u32 = 0;
    pub const VIRTIO_CONSOLE_F_MULTIPORT: u32 = 1;
    pub const VIRTIO_F_VERSION_1: u32 = 32;
    pub const VIRTIO_ID_CONSOLE: u32 = 3;
}

#[allow(dead_code)]
pub mod control_event {
    pub const VIRTIO_CONSOLE_DEVICE_READY: u16 = 0;
    // Also known as VIRTIO_CONSOLE_DEVICE_ADD in spec, but kernel uses this (more descriptive) name
    pub const VIRTIO_CONSOLE_PORT_ADD: u16 = 1;
    /// Also known as VIRTIO_CONSOLE_DEVICE_REMOVE in spec, but kernel uses this (more descriptive) name
    pub const VIRTIO_CONSOLE_PORT_REMOVE: u16 = 2;
    pub const VIRTIO_CONSOLE_PORT_READY: u16 = 3;
    pub const VIRTIO_CONSOLE_CONSOLE_PORT: u16 = 4;
    pub const VIRTIO_CONSOLE_RESIZE: u16 = 5;
    pub const VIRTIO_CONSOLE_PORT_OPEN: u16 = 6;
    pub const VIRTIO_CONSOLE_PORT_NAME: u16 = 7;
}

pub struct Console<M: GuestAddressSpace + 'static, Q: DeviceQueue> {
    ports: Vec<Port<M>>,
    queues: Vec<QueueArgs>,
    worker_thread: Option<JoinHandle<()>>,
    stopfd: EventFd,

    pub(crate) avail_features: u64,
    pub(crate) acked_features: u64,

    config: VirtioConsoleConfig,
    q: PhantomData<Q>,
}

impl<M, Q> Console<M, Q>
where
    M: GuestAddressSpace + Clone + Send,
    Q: DeviceQueue,
    BitmapOfAS<M>: BitmapSlice,
{
    pub fn new(ports: Vec<PortDescription<M>>) -> anyhow::Result<Console<M, Q>> {
        assert!(!ports.is_empty(), "Expected at least 1 port");
        assert!(
            matches!(ports[0], PortDescription::Console { .. }),
            "First port must be a console"
        );

        let num_queues = num_queues(ports.len());
        let queues = vec![QueueArgs::new(QUEUE_SIZE); num_queues];

        let ports: Vec<_> = zip(0u32.., ports)
            .map(|(port_id, description)| Port::new(port_id, description))
            .collect();

        let config = VirtioConsoleConfig::new(80, 24, ports.len() as u32);
        let stopfd = EventFd::new(0).unwrap();

        Ok(Console {
            ports,
            queues,
            worker_thread: None,
            stopfd,
            avail_features: AVAIL_FEATURES,
            acked_features: 0,
            config,
            q: PhantomData,
        })
    }
}

impl<M, Q> VirtioDevice<M, Q> for Console<M, Q>
where
    M: GuestAddressSpace + Send + Clone + 'static,
    Q: DeviceQueue + Send + 'static,
    BitmapOfAS<M>: BitmapSlice,
{
    fn avail_features(&self) -> u64 {
        self.avail_features
    }

    fn acked_features(&self) -> u64 {
        self.acked_features
    }

    fn set_acked_features(&mut self, acked_features: u64) {
        self.acked_features = AVAIL_FEATURES & acked_features;
    }

    fn device_type(&self) -> u32 {
        uapi::VIRTIO_ID_CONSOLE
    }

    fn queues(&self) -> &[QueueArgs] {
        log::trace!("num queues {}", self.queues.len());
        &self.queues
    }

    fn read_config(&self, offset: usize, mut data: &mut [u8]) -> anyhow::Result<()> {
        let config_slice = self.config.as_slice();
        let config_len = config_slice.len();
        if offset >= config_len {
            anyhow::bail!("Invalid offset");
        }
        if let Some(end) = offset.checked_add(data.len()) {
            // This write can't fail, offset and end are checked against config_len.
            data.write_all(&config_slice[offset..cmp::min(end, config_len)])
                .unwrap();
        }

        Ok(())
    }

    fn write_config(&mut self, _offset: usize, _data: &[u8]) -> anyhow::Result<()> {
        Err(anyhow::anyhow!("Config write unsupported"))
    }

    fn activate(&mut self, mem: M, queues: BTreeMap<usize, Q>) -> anyhow::Result<()> {
        // TODO: Make the device activatable again - currently we just steal the ports on the first run!
        log::trace!("Console::activate");
        assert!(!self.ports.is_empty());
        let ports = mem::take(&mut self.ports);
        let worker = MainWorker::new(mem, queues, ports, self.stopfd.try_clone()?);
        self.worker_thread = Some(thread::spawn(move || worker.run()));
        //thread::sleep(Duration::from_secs(4));
        log::trace!("Spawned control worker thread!");
        Ok(())
    }

    fn stop(&mut self) {
        self.stopfd.write(1).unwrap();
        if let Some(worker_thread) = self.worker_thread.take() {
            worker_thread.join().unwrap();
        }
    }

    fn reset(&mut self) -> anyhow::Result<bool> {
        // FIXME: Why the hell do I need this?
        Console::stop(self);
        Ok(true)
    }
}
