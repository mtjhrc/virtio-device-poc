use crate::console_control::ConsoleControl;
use crate::port_io::{PortInput, PortOutput};
use crate::process_rx::process_rx;
use crate::process_tx::process_tx;
use std::borrow::Cow;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::{mem, thread};
use virtio::{BitmapOfAS, DeviceQueue};
use vm_memory::bitmap::BitmapSlice;
use vm_memory::GuestAddressSpace;
use vmm_sys_util::eventfd::EventFd;

pub enum PortDescription<M: GuestAddressSpace> {
    Console {
        input: Option<Arc<dyn PortInput<M> + Send + Sync>>,
        output: Option<Arc<dyn PortOutput<M> + Send + Sync>>,
    },
    InputPipe {
        name: Cow<'static, str>,
        input: Arc<dyn PortInput<M> + Send + Sync>,
    },
    OutputPipe {
        name: Cow<'static, str>,
        output: Arc<dyn PortOutput<M> + Send + Sync>,
    },
}

enum PortState {
    Inactive,
    Active {
        stopfd: EventFd,
        rx_thread: Option<JoinHandle<()>>,
        tx_thread: Option<JoinHandle<()>>,
    },
}

pub(crate) struct Port<M: GuestAddressSpace + 'static> {
    port_id: u32,
    /// Empty if no name given
    name: Cow<'static, str>,
    represents_console: bool,
    state: PortState,
    input: Option<Arc<dyn PortInput<M> + Send + Sync>>,
    output: Option<Arc<dyn PortOutput<M> + Send + Sync>>,
}

impl<M> Port<M>
where
    M: GuestAddressSpace + Clone + Send,
    BitmapOfAS<M>: BitmapSlice,
{
    pub(crate) fn new(port_id: u32, description: PortDescription<M>) -> Self {
        match description {
            PortDescription::Console { input, output } => Self {
                port_id,
                name: "".into(),
                represents_console: true,
                state: PortState::Inactive,
                input: Some(input.unwrap()),
                output: Some(output.unwrap()),
            },
            PortDescription::InputPipe { name, input } => Self {
                port_id,
                name,
                represents_console: false,
                state: PortState::Inactive,
                input: Some(input),
                output: None,
            },
            PortDescription::OutputPipe { name, output } => Self {
                port_id,
                name,
                represents_console: false,
                state: PortState::Inactive,
                input: None,
                output: Some(output),
            },
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn is_console(&self) -> bool {
        self.represents_console
    }

    pub fn start<Q: DeviceQueue + Send + 'static>(
        &mut self,
        mem: M,
        rx_queue: Q,
        tx_queue: Q,
        control: Arc<ConsoleControl>,
        stopfd: EventFd,
    ) {
        if let PortState::Active { .. } = &mut self.state {
            self.shutdown();
        };

        let input: Option<Arc<dyn PortInput<M> + Send + Sync>> = self.input.as_ref().cloned();

        let output = self.output.as_ref().cloned();

        let rx_thread = input.map(|input| {
            let mem = mem.clone();
            let port_id = self.port_id;
            let stopfd = stopfd.try_clone().unwrap();
            thread::Builder::new()
                .name("console port".into())
                .spawn(move || process_rx(mem, rx_queue, input, control, port_id, stopfd))
                .unwrap()
        });

        let tx_thread = output.map(|output| {
            let stopfd = stopfd.try_clone().unwrap();
            thread::spawn(move || process_tx(mem, tx_queue, output, stopfd))
        });

        self.state = PortState::Active {
            stopfd,
            rx_thread,
            tx_thread,
        }
    }

    pub fn shutdown(&mut self) {
        if let PortState::Active {
            stopfd,
            tx_thread,
            rx_thread,
        } = &mut self.state
        {
            stopfd.write(1).unwrap();

            if let Some(tx_thread) = mem::take(tx_thread) {
                tx_thread.thread().unpark();
                if let Err(e) = tx_thread.join() {
                    log::error!(
                        "Failed to flush tx for port {port_id}, thread panicked: {e:?}",
                        port_id = self.port_id
                    )
                }
            }
            if let Some(rx_thread) = mem::take(rx_thread) {
                rx_thread.thread().unpark();
                if let Err(e) = rx_thread.join() {
                    log::error!(
                        "Failed to flush tx for port {port_id}, thread panicked: {e:?}",
                        port_id = self.port_id
                    )
                }
            }
        };
    }
}
