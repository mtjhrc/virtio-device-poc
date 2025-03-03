use event_manager::EventSource;
use libc::{fcntl, F_GETFL, F_SETFL, O_NONBLOCK, STDERR_FILENO, STDIN_FILENO, STDOUT_FILENO};
use nix::errno::Errno;
use nix::libc;
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use nix::unistd::dup;
use std::io::{self, ErrorKind};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use virtio::BitmapOfAS;
use vm_memory::bitmap::{Bitmap, BitmapSlice};
use vm_memory::{GuestAddressSpace, VolatileMemoryError, VolatileSlice, WriteVolatile};
use vmm_sys_util::epoll::{ControlOperation, Epoll, EpollEvent, EventSet};

pub struct PortEvent<'a> {
    fd: BorrowedFd<'a>,
    events: EventSet,
}

impl<'a> PortEvent<'a> {
    fn new(fd: BorrowedFd<'a>, events: EventSet) -> PortEvent<'a> {
        Self { fd, events }
    }
}

impl EventSource for PortEvent<'_> {
    fn register_self(&self, epoll: &Arc<Epoll>, token: u64) -> io::Result<()> {
        epoll.ctl(
            ControlOperation::Add,
            self.fd.as_raw_fd(),
            EpollEvent::new(self.events, token),
        )
    }

    fn unregister_self(&self, epoll: &Arc<Epoll>) -> io::Result<()> {
        epoll.ctl(
            ControlOperation::Delete,
            self.fd.as_raw_fd(),
            EpollEvent::new(self.events, 0),
        )
    }
}

pub trait PortInput<M>
where
    M: GuestAddressSpace,
    BitmapOfAS<M>: BitmapSlice,
{
    fn read_volatile<'a>(
        &'a self,
        buf: &'a mut VolatileSlice<'a, BitmapOfAS<M>>,
    ) -> Result<usize, io::Error>;

    fn ready_event(&self) -> PortEvent<'_>;
}

pub trait PortOutput<M>
where
    M: GuestAddressSpace,
    BitmapOfAS<M>: BitmapSlice,
{
    fn write_volatile<'a>(
        &'a self,
        buf: &'a VolatileSlice<'a, BitmapOfAS<M>>,
    ) -> Result<usize, io::Error>;

    fn ready_event(&self) -> PortEvent<'_>;

    fn wait_until_writable(&self);
}

pub fn stdin<M>() -> Result<Arc<dyn PortInput<M> + Send + Sync>, nix::Error>
where
    M: GuestAddressSpace + 'static,
    BitmapOfAS<M>: BitmapSlice,
{
    let fd = dup_raw_fd_into_owned(STDIN_FILENO)?;
    make_non_blocking(&fd)?;
    Ok(Arc::new(PortInputFd(fd)))
}

pub fn stdout<M>() -> Result<Arc<dyn PortOutput<M> + Send + Sync>, nix::Error>
where
    M: GuestAddressSpace + 'static,
    BitmapOfAS<M>: BitmapSlice,
{
    output_to_raw_fd_dup(STDOUT_FILENO)
}

pub fn stderr<M>() -> Result<Arc<dyn PortOutput<M> + Send + Sync>, nix::Error>
where
    M: GuestAddressSpace + 'static,
    BitmapOfAS<M>: BitmapSlice,
{
    output_to_raw_fd_dup::<M>(STDERR_FILENO)
}

pub fn output_to_raw_fd_dup<M>(
    fd: RawFd,
) -> Result<Arc<dyn PortOutput<M> + Send + Sync>, nix::Error>
where
    M: GuestAddressSpace + 'static,
    BitmapOfAS<M>: BitmapSlice,
{
    let fd = dup_raw_fd_into_owned(fd)?;
    make_non_blocking(&fd)?;
    Ok(Arc::new(PortOutputFd(fd)))
}

pub struct PortInputFd(OwnedFd);

impl AsRawFd for PortInputFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

impl AsFd for PortInputFd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        unsafe { BorrowedFd::borrow_raw(self.as_raw_fd()) }
    }
}

impl<M> PortInput<M> for PortInputFd
where
    M: GuestAddressSpace,
    BitmapOfAS<M>: BitmapSlice,
{
    fn read_volatile(&self, buf: &mut VolatileSlice<BitmapOfAS<M>>) -> io::Result<usize> {
        // This source code is copied from vm-memory, except it fixes an issue, where
        // the original code would does not handle handle EWOULDBLOCK

        let fd = self.as_raw_fd();
        let guard = buf.ptr_guard_mut();

        let dst = guard.as_ptr().cast::<libc::c_void>();

        // SAFETY: We got a valid file descriptor from `AsRawFd`. The memory pointed to by `dst` is
        // valid for writes of length `buf.len() by the invariants upheld by the constructor
        // of `VolatileSlice`.
        let bytes_read = unsafe { libc::read(fd, dst, buf.len()) };

        if bytes_read < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() != ErrorKind::WouldBlock {
                // We don't know if a partial read might have happened, so mark everything as dirty
                buf.bitmap().mark_dirty(0, buf.len());
            }

            Err(err)
        } else {
            let bytes_read = bytes_read.try_into().unwrap();
            buf.bitmap().mark_dirty(0, bytes_read);
            Ok(bytes_read)
        }
    }

    fn ready_event(&self) -> PortEvent<'_> {
        PortEvent::new(self.as_fd(), EventSet::IN | EventSet::EDGE_TRIGGERED)
    }
}

pub struct PortOutputFd(OwnedFd);

impl AsRawFd for PortOutputFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

impl AsFd for PortOutputFd {
    fn as_fd(&self) -> BorrowedFd<'_> {
        unsafe { BorrowedFd::borrow_raw(self.as_raw_fd()) }
    }
}

impl<M> PortOutput<M> for PortOutputFd
where
    M: GuestAddressSpace,
    BitmapOfAS<M>: BitmapSlice,
{
    fn write_volatile(&self, buf: &VolatileSlice<BitmapOfAS<M>>) -> Result<usize, io::Error> {
        // FIXME: (in vm-memory), write_volatile shouldn't require &mut reference
        let mut fd = self.0.as_fd();

        let mut dbg_buf = vec![0u8; buf.len()];
        buf.copy_to(&mut dbg_buf[..]);
        log::trace!(
            "PortOutputFd to fd: {}, writing {:?}",
            fd.as_raw_fd(),
            dbg_buf
        );
        fd.write_volatile(buf).map_err(|e| match e {
            VolatileMemoryError::IOError(e) => e,
            e => {
                log::error!("Unsuported error from write_volatile: {e:?}");
                io::Error::new(ErrorKind::Other, e)
            }
        })
    }
    fn ready_event(&self) -> PortEvent<'_> {
        PortEvent::new(self.as_fd(), EventSet::OUT | EventSet::EDGE_TRIGGERED)
    }

    fn wait_until_writable(&self) {
        log::trace!("PortOutputFd wait_until_writable");
        let mut poll_fds = [PollFd::new(self.0.as_fd(), PollFlags::POLLOUT)];
        poll(&mut poll_fds, PollTimeout::NONE).expect("Failed to poll");
    }
}

fn dup_raw_fd_into_owned(raw_fd: RawFd) -> Result<OwnedFd, nix::Error> {
    let fd = dup(raw_fd)?;
    // SAFETY: the fd is valid because dup succeeded
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn make_non_blocking(as_rw_fd: &impl AsRawFd) -> Result<(), nix::Error> {
    let fd = as_rw_fd.as_raw_fd();
    unsafe {
        let flags = fcntl(fd, F_GETFL, 0);
        if flags < 0 {
            return Err(Errno::last());
        }

        if fcntl(fd, F_SETFL, flags | O_NONBLOCK) < 0 {
            return Err(Errno::last());
        }
    }
    Ok(())
}
