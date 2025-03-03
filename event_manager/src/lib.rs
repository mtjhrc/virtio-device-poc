// Copyright 2020 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::io;
use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, RawFd};
use std::sync::Arc;
use std::time::Duration;
use vmm_sys_util::epoll::{ControlOperation, Epoll, EpollEvent, EventSet};

pub use event_token_derive::*;
use vmm_sys_util::eventfd::EventFd;

#[cfg(feature = "arbitrary-int")]
mod arbitrary_int_support;

/// Trait that can be used to associate events with arbitrary enums when using
/// `WaitContext`.
///
/// Simple enums that have no or primitive variant data data can use the `#[derive(EventToken)]`
/// custom derive to implement this trait. See
/// [event_token_derive::event_token](../event_token_derive/fn.event_token.html) for details.
pub trait EventToken {
    // The number of bits it takes to store this EventToken, used to check if type fits
    const USED_BITS: u32;

    /// Converts this token into a u64 that can be turned back into a token via `from_raw_token`.
    fn as_raw_token(&self) -> u64;

    /// Converts a raw token as returned from `as_raw_token` back into a token.
    ///
    /// It is invalid to give a raw token that was not returned via `as_raw_token` from the same
    /// `Self`. The implementation can expect that this will never happen as a result of its usage
    /// in `WaitContext`.
    fn from_raw_token(data: u64) -> Self;
}

const _: () = {
    if usize::BITS > u64::BITS {
        panic!("Unsupported platform: usize is bigger than 64 bits");
    }
};

impl EventToken for usize {
    const USED_BITS: u32 = Self::BITS;

    fn as_raw_token(&self) -> u64 {
        *self as u64
    }

    fn from_raw_token(data: u64) -> Self {
        data as Self
    }
}

impl EventToken for u64 {
    const USED_BITS: u32 = Self::BITS;

    fn as_raw_token(&self) -> u64 {
        *self
    }

    fn from_raw_token(data: u64) -> Self {
        data
    }
}

impl EventToken for u32 {
    const USED_BITS: u32 = Self::BITS;

    fn as_raw_token(&self) -> u64 {
        u64::from(*self)
    }

    fn from_raw_token(data: u64) -> Self {
        data as Self
    }
}

impl EventToken for u16 {
    const USED_BITS: u32 = Self::BITS;

    fn as_raw_token(&self) -> u64 {
        u64::from(*self)
    }

    fn from_raw_token(data: u64) -> Self {
        data as Self
    }
}

impl EventToken for u8 {
    const USED_BITS: u32 = Self::BITS;

    fn as_raw_token(&self) -> u64 {
        u64::from(*self)
    }

    fn from_raw_token(data: u64) -> Self {
        data as Self
    }
}

impl EventToken for () {
    const USED_BITS: u32 = 0;

    fn as_raw_token(&self) -> u64 {
        0
    }

    fn from_raw_token(_data: u64) -> Self {}
}

/// Represents an event that has been signaled and waited for via a wait function.
#[derive(Copy, Clone, Debug)]
pub struct TriggeredEvent<T: EventToken> {
    token: T,
    readable: bool,
    writable: bool,
    hungup: bool,
}

impl<T: EventToken> TriggeredEvent<T> {
    pub fn new(token: T, readable: bool, writable: bool, hungup: bool) -> Self {
        Self {
            token,
            readable,
            writable,
            hungup,
        }
    }

    pub fn token(&self) -> &T {
        &self.token
    }

    pub fn readable(&self) -> bool {
        self.readable
    }

    pub fn writable(&self) -> bool {
        self.writable
    }

    pub fn hungup(&self) -> bool {
        self.hungup
    }
}

/// Used to wait for multiple objects which are eligible for waiting.
///
pub struct EventManager<T: EventToken> {
    epoll: Arc<Epoll>,
    tokens: PhantomData<[T]>,
    // TODO: Enforce lifetime of fd's?
}

/// Trait for any abstract event that can be polled using an epoll instance
/// TODO: make this part of vmm-sys-utils? Since this only requires Epoll, it a logical place
pub trait EventSource {
    /// Register self into the epoll instance, with the `token` value as the event.
    /// The EventSource may register multiple file descriptors, but all of them have to have the
    /// same `token`.
    ///
    /// Note that the Epoll instance is wrapped Arc, meaning the EventSource can hold a weak
    /// reference to the epoll instance to modify, remove or even add different watched file
    /// descriptor(s) later.
    fn register_self(&self, epoll: &Arc<Epoll>, token: u64) -> io::Result<()>;

    /// Unregister the EventSource
    fn unregister_self(&self, epoll: &Arc<Epoll>) -> io::Result<()>;
}

impl EventSource for EventFd {
    fn register_self(&self, epoll: &Arc<Epoll>, token: u64) -> io::Result<()> {
        epoll.ctl(
            ControlOperation::Add,
            self.as_raw_fd(),
            EpollEvent::new(EventSet::IN, token),
        )
    }

    fn unregister_self(&self, epoll: &Arc<Epoll>) -> io::Result<()> {
        epoll.ctl(
            ControlOperation::Delete,
            self.as_raw_fd(),
            EpollEvent::default(),
        )
    }
}

impl<T: EventToken> EventManager<T> {
    /// Creates a new WaitContext.
    pub fn new() -> io::Result<EventManager<T>> {
        Ok(Self {
            epoll: Arc::new(Epoll::new()?),
            // tokens: PhantomData,
            tokens: PhantomData,
        })
    }

    pub fn add(&self, event_source: &impl EventSource, token: T) -> io::Result<()> {
        event_source.register_self(&self.epoll, token.as_raw_token())
    }

    pub fn remove(&self, event_source: &impl EventSource) -> io::Result<()> {
        event_source.unregister_self(&self.epoll)
    }

    /// Adds the given `descriptor` to this context, watching for the specified events and
    /// associates the given 'token' with those events.
    ///
    /// A `descriptor` can only be added once and does not need to be kept open. If the `descriptor`
    /// is dropped and there were no duplicated file descriptors (i.e. adding the same descriptor
    /// with a different FD number) added to this context, events will not be reported by `wait`
    /// anymore.
    pub fn add_fd(&self, as_fd: &impl AsFd, event_set: EventSet, token: T) -> io::Result<()> {
        let event = EpollEvent::new(event_set, token.as_raw_token());
        self.epoll
            .ctl(ControlOperation::Add, as_fd.as_fd().as_raw_fd(), event)?;
        Ok(())
    }

    /// If `fd` was previously added to this context, the watched events will be replaced with
    /// `event_type` and the token associated with it will be replaced with the given `token`.
    pub fn modify_fd(&self, as_fd: &impl AsFd, event_type: EventSet, token: T) -> io::Result<()> {
        let event = EpollEvent::new(event_type, token.as_raw_token());
        self.epoll
            .ctl(ControlOperation::Modify, as_fd.as_fd().as_raw_fd(), event)?;
        Ok(())
    }

    /// Deletes the given `fd` from this context. If the `fd` is not being polled by this context,
    /// the call is silently dropped without errors.
    ///
    /// If an `fd`'s token shows up in the list of hangup events, it should be removed using this
    /// method or by closing/dropping (if and only if the fd was never dup()'d/fork()'d) the `fd`.
    /// Failure to do so will cause the `wait` method to always return immediately, causing ~100%
    /// CPU load.
    pub fn delete_fd(&self, as_fd: &impl AsFd) -> io::Result<()> {
        self.epoll.ctl(
            ControlOperation::Delete,
            as_fd.as_fd().as_raw_fd(),
            EpollEvent::default(),
        )?;
        Ok(())
    }

    // TODO: TriggeredEvent should be opaque if the Event is coming from an EventSource
    pub fn wait(&self) -> io::Result<impl Iterator<Item = TriggeredEvent<T>>> {
        self.wait_timeout(None)
    }

    /// Waits for any events to occur in FDs that were previously added to this context.
    ///
    /// This may return earlier than `timeout` with zero events if the duration indicated exceeds
    /// system limits.
    pub fn wait_timeout(
        &self,
        timeout: Option<Duration>,
    ) -> io::Result<impl Iterator<Item = TriggeredEvent<T>>> {
        // FIXME: this is sketchy!
        let mut epoll_events: [EpollEvent; 16] =
            // SAFETY:
            // `MaybeUnint<T>` has the same layout as plain `T` (`epoll_event` in our case).
            // We submit an uninitialized array to the `epoll_wait` system call, which returns how many
            // elements it initialized, and then we convert only the initialized `MaybeUnint` values
            // into `epoll_event` structures after the call.
            unsafe { MaybeUninit::zeroed().assume_init() };

        let timeout_millis: i32 = timeout.map_or(-1, |timeout| {
            timeout.as_millis().try_into().unwrap_or(i32::MAX)
        });

        let event_count = loop {
            match self.epoll.wait(timeout_millis, &mut epoll_events) {
                Ok(count) => break count,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        };

        let events: Vec<_> = epoll_events[0..event_count]
            .iter()
            .map(|e| {
                let event_set = EventSet::from_bits_truncate(e.events);
                TriggeredEvent {
                    token: T::from_raw_token(e.u64),
                    readable: event_set.contains(EventSet::IN),
                    writable: event_set.contains(EventSet::OUT),
                    hungup: event_set.contains(EventSet::HANG_UP)
                        || event_set.contains(EventSet::READ_HANG_UP),
                }
            })
            .collect();
        Ok(events.into_iter())
    }

    // Wait for a single event
    pub fn wait_one(&self) -> io::Result<TriggeredEvent<T>> {
        // FIXME: code duplication
        let mut events: [EpollEvent; 1] = [EpollEvent::default()];
        let event_count = loop {
            match self.epoll.wait(-1, &mut events) {
                Ok(count) => break count,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        };
        assert_eq!(event_count, 1);

        let event_set = EventSet::from_bits_truncate(events[0].events);
        Ok(TriggeredEvent {
            token: T::from_raw_token(events[0].u64),
            readable: event_set.contains(EventSet::IN),
            writable: event_set.contains(EventSet::OUT),
            hungup: event_set.contains(EventSet::HANG_UP)
                || event_set.contains(EventSet::READ_HANG_UP),
        })
    }
}

impl<T: EventToken> AsRawFd for EventManager<T> {
    fn as_raw_fd(&self) -> RawFd {
        self.epoll.as_raw_fd()
    }
}

impl<T: EventToken> AsFd for EventManager<T> {
    fn as_fd<'fd>(&'fd self) -> BorrowedFd<'fd> {
        // SAFETY: The returned BorrowedFd is valid (that should be guaranteed by the Epoll struct)
        // and its lifetime is bound by the lifetime of &self
        unsafe { BorrowedFd::borrow_raw(self.as_raw_fd()) }
    }
}

#[cfg(test)]
mod tests {
    use event_token_derive::EventToken;

    use std::fmt::Debug;

    use super::*;

    #[test]
    #[allow(dead_code)]
    fn event_token_derive() {
        #[derive(EventToken)]
        enum EmptyToken {}

        #[derive(PartialEq, Debug, EventToken)]
        enum Token {
            Alpha,
            Beta,
            // comments
            Gamma(u32),
            Delta,
        }

        assert_eq!(
            Token::from_raw_token(Token::Alpha.as_raw_token()),
            Token::Alpha
        );
        assert_eq!(
            Token::from_raw_token(Token::Beta.as_raw_token()),
            Token::Beta
        );
        assert_eq!(
            Token::from_raw_token(Token::Gamma(55).as_raw_token()),
            Token::Gamma(55)
        );
        assert_eq!(
            Token::from_raw_token(Token::Delta.as_raw_token()),
            Token::Delta
        );
    }

    fn assert_roundtrip<T: EventToken + PartialEq + Debug>(t: T) {
        assert_eq!(T::from_raw_token(t.as_raw_token()), t);
    }

    #[test]
    fn event_token_derive_nested() {
        #[derive(PartialEq, Debug, Copy, Clone, EventToken)]
        enum TokL2 {
            A(u32),
            B(u32),
        }

        #[derive(PartialEq, Debug, Copy, Clone, EventToken)]
        enum TokL3 {
            A(TokL2),
            B(TokL2),
        }

        #[derive(PartialEq, Debug, Copy, Clone, EventToken)]
        enum TokL1 {
            A,
            B(TokL2),
            C(TokL3),
        }
        assert_roundtrip(TokL1::A);
        assert_roundtrip(TokL2::A(0));
        assert_roundtrip(TokL2::B(21432));
        assert_roundtrip(TokL3::A(TokL2::A(0)));
    }

    #[test]
    fn event_token_derive_overflow() {
        #[derive(PartialEq, Debug, Copy, Clone, EventToken)]
        enum Tok {
            A(u32),
            B(u32),
        }
        assert_roundtrip(Tok::A(u32::MAX));
    }
}
