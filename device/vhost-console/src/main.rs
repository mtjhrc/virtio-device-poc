use derive_more::{AsMut, AsRef};
use nix::sys::termios::{tcgetattr, tcsetattr, LocalFlags, SetArg};
use std::io::stdin;
use std::os::fd::{AsFd, BorrowedFd};
use vhost::vhost_user::VhostUserProtocolFeatures;
use vhost_user::{VhostUserDaemon, VhostUserDeviceImplementer, Vring};
use virtio_console::{Console, PortDescription};
use vm_memory::{GuestMemoryAtomic, GuestMemoryMmap};

type ConsoleDevice = Console<GuestMemoryAtomic<GuestMemoryMmap>, Vring>;

#[derive(AsRef, AsMut)]
struct VhostUserConsole(ConsoleDevice);

impl VhostUserDeviceImplementer for VhostUserConsole {
    type Device = ConsoleDevice;
    type Bitmap = ();

    fn protocol_features(&self) -> VhostUserProtocolFeatures {
        VhostUserProtocolFeatures::MQ
            | VhostUserProtocolFeatures::CONFIG
            | VhostUserProtocolFeatures::REPLY_ACK
    }
}

fn set_raw_mode(term: BorrowedFd<'_>) -> Result<(), nix::Error> {
    let mut termios = tcgetattr(term)?;
    termios.local_flags &= !(LocalFlags::ECHO | LocalFlags::ICANON);
    tcsetattr(term, SetArg::TCSANOW, &termios)?;
    Ok(())
}

fn main() {
    env_logger::init();

    let mem = GuestMemoryAtomic::new(GuestMemoryMmap::<()>::new());

    set_raw_mode(stdin().as_fd()).unwrap();

    let console = ConsoleDevice::new(vec![PortDescription::Console {
        input: Some(virtio_console::stdin().unwrap()),
        output: Some(virtio_console::stdout().unwrap()),
    }])
    .unwrap();

    let vhost_console = VhostUserConsole(console);

    let mut backend =
        VhostUserDaemon::new("vhost-user-console".into(), vhost_console, mem.clone()).unwrap();

    backend.serve("/tmp/console").unwrap();
}
