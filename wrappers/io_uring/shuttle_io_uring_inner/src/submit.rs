// This file is derived from turmoil-io-uring 0.1.0 (crates/turmoil-io-uring in
// https://github.com/tokio-rs/turmoil, commit 684acc1a8eea3a9cf2c6959dc47b69dba981cac1),
// Copyright (c) 2022 Carl Lerche, and licensed under the MIT license: see LICENSE-MIT in this crate.
// The changes to it are licensed under the Apache License, Version 2.0, as the rest of Shuttle is.

//! Submitting and registering, mirroring the real crate's `Submitter`.

use std::io;
use std::mem;
use std::os::fd::RawFd;

use bitflags::bitflags;

use crate::kernel::Ring;
use crate::register::Probe;
use crate::types::SubmitArgs;

bitflags! {
    /// `io_uring_enter` flags, mirroring the real crate's `EnterFlags`.
    #[derive(Default, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct EnterFlags: u32 {
        const GETEVENTS = 1;
        const SQ_WAKEUP = 1 << 1;
        const SQ_WAIT = 1 << 2;
        const EXT_ARG = 1 << 3;
        const REGISTERED_RING = 1 << 4;
        const ABS_TIMER = 1 << 5;
        const EXT_ARG_REG = 1 << 6;
        const NO_IOWAIT = 1 << 7;
    }
}

/// The ring's syscall interface: `io_uring_enter` and `io_uring_register`.
pub struct Submitter<'a> {
    ring: &'a Ring,
}

impl<'a> Submitter<'a> {
    pub(crate) fn new(ring: &'a Ring) -> Self {
        Self { ring }
    }

    /// `io_uring_enter`.
    ///
    /// With `EXT_ARG`, `arg` must be a [`SubmitArgs`], as the kernel requires of
    /// `io_uring_getevents_arg`; its timeout bounds the wait.
    ///
    /// # Safety
    ///
    /// As in the real crate: `arg` must point at the argument type `flag` selects.
    pub unsafe fn enter<T: Sized>(
        &self,
        to_submit: u32,
        min_complete: u32,
        flag: u32,
        arg: Option<&T>,
    ) -> io::Result<usize> {
        let flags = EnterFlags::from_bits_retain(flag);
        let timeout = match arg {
            Some(arg) if flags.contains(EnterFlags::EXT_ARG) => {
                if mem::size_of::<T>() != mem::size_of::<SubmitArgs<'_, '_>>() {
                    return Err(io::Error::from_raw_os_error(libc::EINVAL));
                }
                // SAFETY: with `EXT_ARG` the caller passes a `SubmitArgs` (checked by size, as the
                // kernel checks `argsz`).
                let args = unsafe { &*(arg as *const T).cast::<SubmitArgs<'_, '_>>() };
                args.timeout
            }
            _ => None,
        };
        self.ring
            .enter(to_submit, min_complete, flags.contains(EnterFlags::GETEVENTS), timeout)
    }

    /// Submit every published entry.
    pub fn submit(&self) -> io::Result<usize> {
        self.submit_and_wait(0)
    }

    /// Submit every published entry, then block until `want` completions are visible.
    pub fn submit_and_wait(&self, want: usize) -> io::Result<usize> {
        self.ring
            .enter(self.ring.sq_published() as u32, want as u32, want > 0, None)
    }

    /// Submit every published entry, then block until `want` completions are visible or the wait
    /// times out.
    ///
    /// Shuttle has no clock, so a bounded wait times out when the ring's kernel task fires it, as it
    /// fires a timeout SQE: while I/O is in flight one time in 256, when only timers are, at any step
    /// Shuttle chooses, and when nothing is in flight, at once. As the kernel's does, a wait that
    /// times out with completions to reap succeeds, and an enter that submitted entries returns how
    /// many it did, whatever became of its wait; only an enter that submitted nothing and found
    /// nothing to reap fails with `ETIME`.
    pub fn submit_with_args(&self, want: usize, args: &SubmitArgs<'_, '_>) -> io::Result<usize> {
        self.submit_with_args_and_flags(want, args, EnterFlags::empty())
    }

    /// Like [`submit_with_args`](Self::submit_with_args), with extra `flags` for the wait.
    ///
    /// As in the real crate only `ABS_TIMER` and `NO_IOWAIT` are accepted, and any other flag fails
    /// with `EINVAL`. `ABS_TIMER` makes no difference, Shuttle having no clock. `NO_IOWAIT` fails with
    /// `EINVAL`, as on a kernel without `IORING_FEAT_NO_IOWAIT`, which the simulation does not report.
    pub fn submit_with_args_and_flags(
        &self,
        want: usize,
        args: &SubmitArgs<'_, '_>,
        flags: EnterFlags,
    ) -> io::Result<usize> {
        if !EnterFlags::ABS_TIMER.contains(flags) {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        self.ring
            .enter(self.ring.sq_published() as u32, want as u32, want > 0, args.timeout)
    }

    /// For SQ polling, which the simulation does not have: returns at once.
    pub fn squeue_wait(&self) -> io::Result<usize> {
        Ok(0)
    }

    pub fn register_files(&self, fds: &[RawFd]) -> io::Result<()> {
        self.ring.register_files(fds.to_vec())
    }

    pub fn register_files_sparse(&self, nr: u32) -> io::Result<()> {
        self.ring.register_files(vec![-1; nr as usize])
    }

    pub fn register_files_update(&self, offset: u32, fds: &[RawFd]) -> io::Result<usize> {
        self.ring.register_files_update(offset, fds)
    }

    pub fn unregister_files(&self) -> io::Result<()> {
        self.ring.unregister_files()
    }

    /// Have the kernel signal `eventfd` as it posts each completion, unless
    /// `IORING_CQ_EVENTFD_DISABLED` is set: it adds 1 to the eventfd's counter, as the kernel does,
    /// and an [`AsyncFd`](crate::AsyncFd) on `eventfd` sees the event. The ring keeps its own
    /// reference to the eventfd, as the kernel does, so closing `eventfd` does not stop the signals.
    pub fn register_eventfd(&self, eventfd: RawFd) -> io::Result<()> {
        self.ring.register_eventfd(eventfd, false)
    }

    /// Like [`register_eventfd`](Self::register_eventfd), but the kernel signals `eventfd` only for
    /// completions that post asynchronously, here: the ones the ring's kernel task posts, not the
    /// ones an enter posts as it submits the op.
    pub fn register_eventfd_async(&self, eventfd: RawFd) -> io::Result<()> {
        self.ring.register_eventfd(eventfd, true)
    }

    pub fn unregister_eventfd(&self) -> io::Result<()> {
        self.ring.unregister_eventfd()
    }

    /// Report the simulated opcodes.
    pub fn register_probe(&self, probe: &mut Probe) -> io::Result<()> {
        probe.fill();
        Ok(())
    }

    /// Enable a ring built with [`setup_r_disabled`](crate::Builder::setup_r_disabled), which refuses
    /// submissions with `EBADFD` until then. Fails with `EBADFD` on a ring that is not disabled.
    pub fn register_enable_rings(&self) -> io::Result<()> {
        self.ring.enable()
    }
}
