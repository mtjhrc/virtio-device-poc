use crate::port_io::PortOutput;
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
    Stop,
    ProcessQueue,
    IO,
}

pub(crate) fn process_tx<M, Q>(
    mem: M,
    device_queue: Q,
    output: Arc<dyn PortOutput<M> + Send>,
    stopfd: EventFd,
) where
    M: GuestAddressSpace,
    Q: DeviceQueue,
    BitmapOfAS<M>: BitmapSlice,
{
    let event_manager: EventManager<WorkerToken> = EventManager::new().unwrap();
    event_manager.add(&stopfd, WorkerToken::Stop).unwrap();
    event_manager
        .add(&device_queue.kick_event(), WorkerToken::ProcessQueue)
        .unwrap();
    event_manager
        .add(&output.ready_event(), WorkerToken::IO)
        .unwrap();

    let mut signal_used_queue = false;
    'event_loop: loop {
        if signal_used_queue {
            device_queue.signal_used_queue().unwrap();
            signal_used_queue = false;
        }

        let event = event_manager.wait_one().unwrap();
        log::trace!("TX process event: {:?}", event);
        match event.token() {
            WorkerToken::Stop => return,
            WorkerToken::ProcessQueue | WorkerToken::IO => {
                if !device_queue.read_kick().unwrap() {
                    log::trace!("Queue disabled");
                    continue;
                }
            }
        }
        let mem = mem.memory();
        let Some(mut queue) = device_queue.try_lock_queue() else {
            log::trace!("TX queue not ready");
            continue;
        };

        while let Some(chain) = queue.pop_descriptor_chain(&*mem) {
            let head_index = chain.head_index();
            log::trace!("TX Processing chain {:?}", head_index);
            let mut bytes_written = 0;
            for desc in chain.into_iter().readable() {
                let desc_len = desc.len() as usize;
                match write_desc_to_output(&*mem, desc, &*output) {
                    Ok(0) => {
                        break;
                    }
                    Ok(n) => {
                        log::trace!("Wrote {n} bytes");
                        assert_eq!(n, desc_len);
                        bytes_written += n;
                        signal_used_queue = true;
                    }
                    Err(e) => {
                        log::error!("Failed to write output: {e}");
                    }
                }
            }

            if bytes_written == 0 {
                log::trace!("Tx go_to_previous_position");
                queue.go_to_previous_position();
                continue 'event_loop;
            } else {
                log::trace!("Tx add used {bytes_written}");
                if let Err(e) = queue.add_used(&*mem, head_index, bytes_written as u32) {
                    log::error!("Failed to add used elements to the queue: {e:?}");
                }
            }
        }
    }
}

fn write_desc_to_output<M>(
    mem: &M::M,
    desc: Descriptor,
    output: &(dyn PortOutput<M> + Send),
) -> Result<usize, GuestMemoryError>
where
    M: GuestAddressSpace,
    BitmapOfAS<M>: BitmapSlice,
{
    mem.memory()
        .try_access(desc.len() as usize, desc.addr(), |_, len, addr, region| {
            let src = region.get_slice(addr, len).unwrap();
            loop {
                log::trace!("Tx {:?}, write_volatile {len} bytes", src);
                match output.write_volatile(&src) {
                    // try_access seem to handle partial write for us (we will be invoked again with an offset)
                    Ok(n) => break Ok(n),
                    // We can't return an error otherwise we would not know how many bytes were processed before WouldBlock
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        log::trace!("Tx wait for output (would block)");
                        // TODO: why do this? Can't the Port Output just be blocking?
                        output.wait_until_writable();
                    }
                    Err(e) => break Err(GuestMemoryError::IOError(e)),
                }
            }
        })
}
