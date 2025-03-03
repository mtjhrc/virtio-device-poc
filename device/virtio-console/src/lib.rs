mod console_control;
mod device;
mod port;
mod port_io;
mod port_queue_mapping;
mod process_rx;
mod process_tx;

pub use {
    device::Console,
    port::PortDescription,
    port_io::{
        output_to_raw_fd_dup, stderr, stdin, stdout, PortInput, PortInputFd, PortOutput,
        PortOutputFd,
    },
};
