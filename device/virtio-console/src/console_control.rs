use crate::device::control_event::{
    VIRTIO_CONSOLE_CONSOLE_PORT, VIRTIO_CONSOLE_PORT_ADD, VIRTIO_CONSOLE_PORT_NAME,
    VIRTIO_CONSOLE_PORT_OPEN, VIRTIO_CONSOLE_RESIZE,
};
use crate::device::{control_event, CONTROL_RXQ_INDEX, CONTROL_TXQ_INDEX};
use crate::port::Port;
use crate::port_queue_mapping::{port_id_to_queue_idx, QueueDirection};
use event_manager::{EventManager, EventToken};
use log::error;
use std::collections::{BTreeMap, VecDeque};
use std::io::Write;
use std::ops::Deref;
use std::sync::{Arc, Mutex};
use virtio::{BitmapOfAS, DeviceQueue};
use virtio_queue::{QueueOwnedT, QueueT, Reader, Writer};
use vm_memory::bitmap::BitmapSlice;
use vm_memory::{ByteValued, GuestAddressSpace};
use vmm_sys_util::eventfd::{EventFd, EFD_NONBLOCK};

#[derive(Copy, Clone, Debug, Default)]
#[repr(C, packed(4))]
pub struct VirtioConsoleControl {
    /// Port number
    pub id: u32,
    /// The kind of control event
    pub event: u16,
    /// Extra information for the event
    pub value: u16,
}

// Safe because it only has data and has no implicit padding.
// But NOTE that this relies on CPU being little endian, to have correct semantics
unsafe impl ByteValued for VirtioConsoleControl {}

#[derive(Copy, Clone, Debug, Default)]
#[repr(C, packed)]
pub struct VirtioConsoleResize {
    // NOTE: the order of these fields in the actual kernel implementation and in the spec are swapped,
    // we follow the order in the kernel to get it working correctly
    pub rows: u16,
    pub cols: u16,
}

// Safe because it only has data and has no implicit padding.
// but NOTE, that we rely on CPU being little endian, for the values to be correct
unsafe impl ByteValued for VirtioConsoleResize {}

#[derive(Debug)]
pub enum Payload {
    ConsoleControl(VirtioConsoleControl),
    Bytes(Vec<u8>),
}

impl Deref for Payload {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        match self {
            Payload::ConsoleControl(b) => b.as_slice(),
            Payload::Bytes(b) => b.as_slice(),
        }
    }
}

// Utility for sending commands into control rx queue
pub struct ConsoleControl {
    queue: Mutex<VecDeque<Payload>>,
    queue_evt: EventFd,
}

impl ConsoleControl {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            queue: Default::default(),
            queue_evt: EventFd::new(EFD_NONBLOCK).unwrap(),
        })
    }

    pub fn mark_console_port(&self, port_id: u32) {
        self.push_msg(VirtioConsoleControl {
            id: port_id,
            event: VIRTIO_CONSOLE_CONSOLE_PORT,
            value: 1,
        })
    }

    pub fn console_resize(&self, port_id: u32, new_size: VirtioConsoleResize) {
        let mut buf = Vec::new();
        buf.extend(
            VirtioConsoleControl {
                id: port_id,
                event: VIRTIO_CONSOLE_RESIZE,
                value: 0,
            }
            .as_slice(),
        );
        buf.extend(new_size.as_slice());
        self.push_vec(buf)
    }

    /// Adds another port with the specified port_id
    pub fn port_add(&self, port_id: u32) {
        self.push_msg(VirtioConsoleControl {
            id: port_id,
            event: VIRTIO_CONSOLE_PORT_ADD,
            value: 0,
        })
    }

    pub fn port_open(&self, port_id: u32, open: bool) {
        self.push_msg(VirtioConsoleControl {
            id: port_id,
            event: VIRTIO_CONSOLE_PORT_OPEN,
            value: open as u16,
        })
    }

    pub fn port_name(&self, port_id: u32, name: &str) {
        let mut buf: Vec<u8> = Vec::new();

        buf.extend_from_slice(
            VirtioConsoleControl {
                id: port_id,
                event: VIRTIO_CONSOLE_PORT_NAME,
                value: 1, // Unspecified/unused in the spec, lets use the same value as QEMU.
            }
            .as_slice(),
        );

        // The spec says the name shouldn't be NUL terminated.
        buf.extend(name.as_bytes());
        self.push_vec(buf)
    }

    pub fn queue_pop(&self) -> Option<Payload> {
        let mut queue = self.queue.lock().expect("Poisoned lock");
        queue.pop_front()
    }

    fn push_msg(&self, msg: VirtioConsoleControl) {
        let mut queue = self.queue.lock().expect("Poisoned lock");
        queue.push_back(Payload::ConsoleControl(msg));
        if let Err(e) = self.queue_evt.write(1) {
            log::trace!("ConsoleControl failed to write to notify {e}")
        }
    }

    fn push_vec(&self, buf: Vec<u8>) {
        let mut queue = self.queue.lock().expect("Poisoned lock");
        queue.push_back(Payload::Bytes(buf));
        if let Err(e) = self.queue_evt.write(1) {
            log::trace!("ConsoleControl failed to write to notify {e}")
        }
    }
}

// Main worker thread, taking care of starting worker threads for ports
pub struct MainWorker<M, Q>
where
    M: GuestAddressSpace + Clone + Send + 'static,
    Q: DeviceQueue + Send + 'static,
{
    mem: M,
    control_rxq: Q,
    control_txq: Q,
    control: Arc<ConsoleControl>,
    port_queues: BTreeMap<usize, Q>,
    ports: Vec<Port<M>>,
    stopfd: EventFd,
}

#[derive(EventToken, Debug, Copy, Clone)]
enum WorkerToken {
    ControlRxq,
    ControlTxq,
    Quit,
}

impl<M, Q> MainWorker<M, Q>
where
    M: GuestAddressSpace + Clone + Send + 'static,
    Q: DeviceQueue + Send + 'static,
    BitmapOfAS<M>: BitmapSlice,
{
    pub fn new(
        mem: M,
        mut queues: BTreeMap<usize, Q>,
        ports: Vec<Port<M>>,
        stopfd: EventFd,
    ) -> Self {
        let control_rxq = queues.remove(&CONTROL_RXQ_INDEX).unwrap();
        let control_txq = queues.remove(&CONTROL_TXQ_INDEX).unwrap();
        let control = ConsoleControl::new();
        let mut s = Self {
            mem,
            control_rxq,
            control_txq,
            control,
            port_queues: queues,
            ports,
            stopfd,
        };
        s.start_ports();
        s
    }

    fn start_ports(&mut self) {
        for port_id in 0..self.ports.len() {
            log::trace!("Starting port io for port {}", port_id);
            self.ports[port_id].start(
                self.mem.clone(),
                self.port_queues
                    .remove(&port_id_to_queue_idx(QueueDirection::Rx, port_id))
                    .expect("Broken invariant: queue already started"),
                self.port_queues
                    .remove(&port_id_to_queue_idx(QueueDirection::Tx, port_id))
                    .expect("Broken invariant: queue already started"),
                self.control.clone(),
                self.stopfd.try_clone().unwrap(), //TODO
            );
        }
    }

    pub fn run(mut self) {
        let events_manager: EventManager<WorkerToken> = EventManager::new().unwrap();
        //thread::sleep(Duration::from_secs(5));
        events_manager
            .add(&self.control_rxq.kick_event(), WorkerToken::ControlRxq)
            .unwrap();
        events_manager
            .add(&self.control_txq.kick_event(), WorkerToken::ControlTxq)
            .unwrap();
        events_manager
            .add(&self.control.queue_evt, WorkerToken::ControlRxq)
            .unwrap();
        events_manager.add(&self.stopfd, WorkerToken::Quit).unwrap();
        log::trace!("I have added my epools!");
        loop {
            log::trace!("vents_manager.wait()");
            for event in events_manager.wait().unwrap() {
                self.control.queue_evt.write(1).unwrap();
                match event.token() {
                    WorkerToken::ControlRxq => self.process_rx(),
                    WorkerToken::ControlTxq => self.process_tx(),
                    WorkerToken::Quit => break,
                }
            }
        }
    }

    fn process_rx(&mut self) {
        if !self.control_rxq.read_kick().unwrap() {
            log::trace!("process_rx: no kick");
            return;
        }
        let _ = self.control.queue_evt.read();
        log::trace!("process_rx");

        let mut raise_irq = false;
        let mem = self.mem.memory();
        let Some(mut queue) = self.control_rxq.try_lock_queue() else {
            return;
        };

        while let Some(head) = queue.pop_descriptor_chain(&*mem) {
            if let Some(buf) = self.control.queue_pop() {
                log::trace!("process_rx, writing: {buf:?}");

                let head_index = head.head_index();
                let mut writer = Writer::new(&*mem, head).unwrap();
                log::trace!("process_tx popped have writer");

                match writer.write(&buf) {
                    Ok(n) if n != buf.len() => {
                        log::error!("process_control_rx: partial write");
                    }
                    Ok(n) => {
                        raise_irq = true;
                        if let Err(e) = queue.add_used(&*self.mem.memory(), head_index, n as u32) {
                            error!("failed to add used elements to the queue: {:?}", e);
                        }
                    }
                    Err(e) => {
                        log::error!("process_control_rx failed to write: {e}");
                    }
                }
            } else {
                queue.go_to_previous_position();
                break;
            }
        }

        if raise_irq {
            self.control_rxq.signal_used_queue().unwrap();
        }
    }

    fn process_tx(&mut self) {
        if !self.control_txq.read_kick().unwrap() {
            return;
        }

        let mut raise_irq = false;

        let mut ports_to_start = Vec::new();

        let mem = self.mem.memory();
        let Some(mut queue) = self.control_txq.try_lock_queue() else {
            return;
        };

        while let Some(head) = queue.pop_descriptor_chain(&*mem) {
            let mut reader = Reader::new(&*mem, head).unwrap();
            log::trace!("process_tx popped have reader");

            raise_irq = true;
            let head_index = 0;

            let cmd: VirtioConsoleControl = match reader.read_obj() {
                Ok(cmd) => cmd,
                Err(e) => {
                    log::error!("Failed to read VirtioConsoleControl: {e}");
                    continue;
                }
            };

            log::trace!("process_tx about to add_used");
            if let Err(e) =
                queue.add_used(&*self.mem.memory(), head_index, size_of_val(&cmd) as u32)
            {
                error!("Failed to add used elements to the queue: {e:?}");
            }
            log::trace!("process_tx add_used end");

            log::trace!("VirtioConsoleControl cmd: {cmd:?}");
            match cmd.event {
                control_event::VIRTIO_CONSOLE_DEVICE_READY => {
                    log::debug!(
                        "Device is ready: initialization {}",
                        if cmd.value == 1 { "ok" } else { "failed" }
                    );
                    log::trace!("Have {} ports", self.ports.len());
                    for port_id in 0..self.ports.len() {
                        log::trace!("Adding port {}", port_id);
                        self.control.port_add(port_id as u32);
                    }

                    //FIXME:? this is just for debugging purposes currently
                    for port_id in 0..self.ports.len() {
                        ports_to_start.push(port_id);
                    }
                }
                control_event::VIRTIO_CONSOLE_PORT_READY => {
                    if cmd.value != 1 {
                        log::error!("Port initialization failed: {:?}", cmd);
                        continue;
                    }

                    if self.ports[cmd.id as usize].is_console() {
                        self.control.mark_console_port(cmd.id);
                        self.control.port_open(cmd.id, true);
                        let (cols, rows) = get_win_size();
                        self.control
                            .console_resize(cmd.id, VirtioConsoleResize { cols, rows });
                    } else {
                        // We start with all ports open, this makes sense for now,
                        // Underlying file descriptors STDIN, STDOUT, STDERR are always open too
                        self.control.port_open(cmd.id, true)
                    }

                    let name = self.ports[cmd.id as usize].name();
                    log::trace!("Port ready {id}: {name}", id = cmd.id);
                    if !name.is_empty() {
                        self.control.port_name(cmd.id, name)
                    }
                }
                VIRTIO_CONSOLE_PORT_OPEN => {
                    let opened = match cmd.value {
                        0 => false,
                        1 => true,
                        _ => {
                            log::error!(
                                "Invalid value ({}) for VIRTIO_CONSOLE_PORT_OPEN on port {}",
                                cmd.value,
                                cmd.id
                            );
                            continue;
                        }
                    };

                    if !opened {
                        log::debug!("Guest closed port {}", cmd.id);
                        continue;
                    }
                }
                _ => log::warn!("Unknown console control event {:x}", cmd.event),
            }
        }

        if raise_irq {
            self.control_txq.signal_used_queue().unwrap();
        }
    }
}

pub(crate) fn get_win_size() -> (u16, u16) {
    //let mut ws: WS = crate::device::WS::default();
    /*
    let ret = unsafe { tiocgwinsz(0, &mut ws) };

    if let Err(err) = ret {
        error!("Couldn't get terminal dimensions: {}", err);
        (0, 0)
    } else {
        (ws.cols, ws.rows)
    }*/
    (80, 24) //FIXME
}
