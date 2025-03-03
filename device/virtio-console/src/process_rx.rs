use crate::console_control::ConsoleControl;
use crate::port_io::PortInput;
use event_manager::{EventManager, EventToken};
use std::io;
use std::sync::Arc;
use virtio::{BitmapOfAS, DeviceQueue};
use virtio_queue::{Descriptor, QueueOwnedT, QueueT};
use vm_memory::bitmap::BitmapSlice;
use vm_memory::{GuestAddressSpace, GuestMemory, GuestMemoryError, GuestMemoryRegion};
use vmm_sys_util::eventfd::EventFd;

#[derive(Debug, EventToken)]
enum WorkerToken {
    ProcessQueue,
    IO,
    Stop,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn process_rx<M: GuestAddressSpace, Q: DeviceQueue>(
    mem: M,
    device_queue: Q,
    input: Arc<dyn PortInput<M> + Send>,
    control: Arc<ConsoleControl>,
    port_id: u32,
    stopfd: EventFd,
) where
    BitmapOfAS<M>: BitmapSlice,
{
    let mut eof = false;
    let event_manager: EventManager<WorkerToken> = EventManager::new().unwrap();
    event_manager.add(&stopfd, WorkerToken::Stop).unwrap();
    event_manager
        .add(&device_queue.kick_event(), WorkerToken::ProcessQueue)
        .unwrap();
    event_manager
        .add(&input.ready_event(), WorkerToken::IO)
        .unwrap();

    let mut signal_used_queue = false;
    'event_loop: loop {
        if signal_used_queue {
            device_queue.signal_used_queue().unwrap();
            signal_used_queue = false;
        }
        let event = event_manager.wait_one().unwrap();
        log::trace!("RX process event: {:?}", event);
        match event.token() {
            WorkerToken::ProcessQueue | WorkerToken::IO => {
                if !device_queue.read_kick().unwrap() {
                    log::trace!("Queue disabled!");
                    continue;
                }
            }
            WorkerToken::Stop => return,
        }

        let mem = mem.memory();
        log::trace!("RX about to grab lock:");
        let Some(mut queue) = device_queue.try_lock_queue() else {
            log::trace!("RX queue not ready");
            continue 'event_loop;
        };
        //log::trace!("RX have lock, queue state:{:?}", queue.state());

        while let Some(chain) = queue.pop_descriptor_chain(&*mem) {
            let head_index = chain.head_index();
            let mut bytes_read = 0;
            for desc in chain.into_iter().writable() {
                match read_to_desc(&*mem, desc, input.as_ref(), &mut eof) {
                    Ok(0) => {
                        break;
                    }
                    Ok(len) => {
                        bytes_read += len;
                        signal_used_queue = true;
                    }
                    Err(e) => {
                        log::error!("Failed to read: {e:?}")
                    }
                }
            }

            log::trace!("RX Processing chain {:?}, {}bytes", head_index, bytes_read);

            if bytes_read == 0 {
                log::trace!("Rx got to previous positions");
                queue.go_to_previous_position();
                continue 'event_loop;
            } else {
                log::trace!("Rx add_used {bytes_read} bytes queue len{}", queue.size());
                if let Err(e) = queue.add_used(&*mem, head_index, bytes_read as u32) {
                    log::error!("failed to add used elements to the queue: {:?}", e);
                }
            }

            if eof {
                log::trace!("signaling EOF on port {port_id}");
                control.port_open(port_id, false);
                break 'event_loop;
            }
        }

        if signal_used_queue {
            device_queue.signal_used_queue().unwrap();
        }
    }
}

fn read_to_desc<M: GuestAddressSpace>(
    mem: &M::M,
    desc: Descriptor,
    input: &(dyn PortInput<M> + Send),
    eof: &mut bool,
) -> Result<usize, GuestMemoryError>
where
    BitmapOfAS<M>: BitmapSlice,
{
    mem.memory()
        .try_access(desc.len() as usize, desc.addr(), |_, len, addr, region| {
            let mut target = region.get_slice(addr, len).unwrap();
            match input.read_volatile(&mut target) {
                Ok(n) => {
                    if n == 0 {
                        *eof = true
                    }
                    Ok(n)
                }
                // We can't return an error otherwise we would not know how many bytes were processed before WouldBlock
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(0),
                Err(e) => Err(GuestMemoryError::IOError(e)),
            }
        })
}
