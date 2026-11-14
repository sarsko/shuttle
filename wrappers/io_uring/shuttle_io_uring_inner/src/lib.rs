// This file is derived from turmoil-io-uring 0.1.0 (crates/turmoil-io-uring in
// https://github.com/tokio-rs/turmoil, commit 684acc1a8eea3a9cf2c6959dc47b69dba981cac1),
// Copyright (c) 2022 Carl Lerche, and licensed under the MIT license: see LICENSE-MIT in this crate.
// The changes to it are licensed under the Apache License, Version 2.0, as the rest of Shuttle is.

//! A simulated `io_uring` kernel for [Shuttle](https://crates.io/crates/shuttle), mirroring the
//! [`io-uring` 0.7](https://docs.rs/io-uring/0.7) crate's API.
//!
//! This crate should not be depended on directly. Use it through the `shuttle-io-uring` wrapper,
//! which is this crate under its `shuttle` feature and `io-uring` otherwise, so code built on the
//! ring runs unchanged under Shuttle.
//!
//! # Why
//!
//! `io-uring` is the kernel interface: it maps the kernel's queues and makes the `io_uring_*` system
//! calls, and the kernel completes ops on its own schedule, which Shuttle can neither see nor
//! control. Waiting for those completions also needs tokio's I/O driver, which a Shuttle execution
//! does not have. A ring driver therefore cannot run under Shuttle at all, and its hardest code
//! (completions racing submissions, wakeups and cancellation) is exactly what Shuttle is for.
//!
//! # The model
//!
//! Each ring has a kernel task. While ops are in flight it is runnable, and each time Shuttle
//! schedules it, it completes one of them, chosen by Shuttle, by running the op's system call on the
//! real file descriptor. So ops stay in flight for as long as Shuttle keeps the kernel task off the
//! CPU, complete in any order, and a failing schedule replays. Every choice the simulation makes is
//! a draw from Shuttle's random number generator, which records it in the schedule.
//!
//! The semantics follow the kernel, not a simplification of it:
//!
//! - A read writes, and a write reads, the caller's buffer when the op completes. The [`checks`]
//!   module's checks, on by default, panic when the caller touches a buffer while its op is in
//!   flight, or hands the kernel the same memory for two ops at once where either is a read.
//! - A vectored op's `iovec` array is copied when the op is submitted (`IORING_FEAT_SUBMIT_STABLE`),
//!   so only the buffers it lists must outlive the submit.
//! - SQ entries reach the kernel only when the queue handle is synced or dropped, and a completion
//!   queue handle sees only what its last tail load exposed.
//! - A completion posts in the kernel's order: store the CQ tail, wake blocked enters, then load the
//!   CQ flags and, unless `IORING_CQ_EVENTFD_DISABLED` is set, signal the registered eventfd: add 1
//!   to its counter, so a driver that reads it sees the count, and wake an [`AsyncFd`] on it. The
//!   tail and flag words are Shuttle atomics, so every interleaving of that protocol is explored.
//! - An op may complete inline, during the enter that submits it.
//! - Timeout SQEs and the bounded waits of [`Submitter::submit_with_args`] are timers, which the
//!   kernel task fires as it completes ops. While I/O is in flight a timer fires only one time in
//!   256: a timeout is normally far longer than the I/O it guards, so firing it then models a device
//!   that outlasts it. A bounded wait with nothing in flight times out at once.
//! - An enter that submitted entries returns how many, whatever became of its wait, and a wait that
//!   times out with a completion to reap succeeds, as the kernel's do. `submit_and_wait` and bounded
//!   enters block the calling task.
//! - A cancel of an in-flight op either cancels it or reports `-EALREADY` and lets it complete;
//!   Shuttle chooses.
//! - Fixed files resolve when an op is submitted, as the kernel takes its file reference then.
//! - Completions that find the CQ full are held back and flushed by the next enter
//!   (`IORING_FEAT_NODROP`); an enter with work to submit meanwhile fails with `EBUSY`, as the
//!   kernel's does.
//! - A read or write at offset `-1` (`u64::MAX`) uses, and advances, the file position
//!   (`IORING_FEAT_RW_CUR_POS`).
//! - Dropping a ring with ops in flight panics, as the kernel would keep using their buffers after
//!   the ring is gone, unless a timer has fired with I/O outstanding: an owner that saw the device
//!   hang may abandon the ring and leak its buffers.
//!
//! The [`faults`] module turns on errors a real kernel can post: refusals with `-EAGAIN`, `-EIO`,
//! short reads and `-ENOSPC`.
//!
//! # Where [`AsyncFd`] lives
//!
//! A driver waits for a ring's completions through tokio's
//! [`AsyncFd`](https://docs.rs/tokio/1/tokio/io/unix/struct.AsyncFd.html), on the ring's own fd or on
//! an eventfd registered with it. Tokio's needs a runtime with an I/O driver, which a Shuttle
//! execution does not have, and it could not see a simulated kernel's events anyway, so this crate
//! has an `AsyncFd` of its own: it waits on the fds the simulated kernel signals, with tokio's
//! clear-ready rules, and rejects any other fd. A driver therefore writes `AsyncFd` once and lets a
//! `cfg` pick tokio's or this one.
//!
//! It is here, rather than in `shuttle-tokio`, because the readiness it reports is this crate's: the
//! simulated kernel is what signals it. It is also only the part a ring driver needs — `new`,
//! `with_interest`, `readable`, `poll_read_ready`, `try_io`, `clear_ready` and `retain_ready` — and
//! readability only. A model of tokio's whole `AsyncFd` in `shuttle-tokio` (the writable and `_mut`
//! halves, `ready`, `async_io`, `clear_ready_matching`, `Ready`, `AsyncFdReadyMutGuard`), which this
//! crate's rings would register their fds with, would let a driver use one path in both modes and
//! would serve fds that have nothing to do with a ring; it is a separate piece of work, and until it
//! exists, an `AsyncFd` under the `shuttle` feature comes from here.
//!
//! # Scheduling points
//!
//! The ring's kernel task takes one before each completion it posts, for the tail store, and one
//! after it, a yield; with an eventfd registered, posting also takes one for the flags load. In the
//! program's own tasks:
//!
//! - building a ring takes one, as it spawns the ring's kernel task;
//! - creating a completion queue handle ([`IoUring::completion`]) and
//!   [`sync`](cqueue::CompletionQueue::sync)ing one each take one, for the tail load; taking
//!   entries from it takes none;
//! - [`eventfd_disabled`](cqueue::CompletionQueue::eventfd_disabled),
//!   [`disable_eventfd`](cqueue::CompletionQueue::disable_eventfd) and
//!   [`enable_eventfd`](cqueue::CompletionQueue::enable_eventfd) each take one, for the flags word;
//! - pushing to and syncing a submission queue take none: without `SQPOLL` the kernel reads the
//!   queue only in an enter, which the submitting task makes itself;
//! - an enter takes none of its own, but an op it completes inline posts its completion then, and a
//!   wait blocks until a completion posts or its timer fires;
//! - waiting on an [`AsyncFd`] takes one only when it blocks.
//!
//! # State per execution
//!
//! Everything the simulation keeps lives in the running execution: the rings, the fds an `AsyncFd`
//! can wait on, the ring fd numbers, which start over at the same number, and the [`faults`] and
//! [`checks`] settings, which start at their defaults. So no execution sees what an earlier one did,
//! and a ring in a `static` that Shuttle resets (its `lazy_static!`, `OnceLock` or `LazyLock`) is a
//! new ring in each execution. Like the rest of Shuttle, building a ring and the functions of
//! [`faults`] and [`checks`] panic outside of a Shuttle test.
//!
//! # Not modelled
//!
//! - `IORING_SETUP_SQPOLL` and `IORING_SETUP_IOPOLL` (rejected at build).
//! - Linked and drained SQEs, provided buffers, and `CQE_SKIP_SUCCESS` (`-EINVAL` completions).
//! - Opcodes other than those in [`opcode`], registered buffers and the other registrations
//!   [`Submitter`] has no method for, `Entry128`/`Entry32`, and `IoUring::from_fd`.
//! - Deferred task work: with `COOP_TASKRUN` or `DEFER_TASKRUN` the kernel posts some completions
//!   only when the submitter next enters. Here every completion posts when it happens, and
//!   `taskrun()` is always `false`.
//! - Time: timers fire at a step Shuttle chooses, whatever their duration, and `ABS_TIMER` and the
//!   clock flags make no difference.
//! - The CQ head is stored as each entry is taken, not when the handle syncs or drops.
//! - Faults and short transfers beyond what the real file returns and what [`faults`] injects.
//! - A crash: ops run their real system calls, so there is no model of what a power loss would keep.
//! - A write the program makes to a registered eventfd itself does not wake an [`AsyncFd`] on it.
//! - A buffer freed while its op is in flight, which is undefined behaviour here as with the kernel.
//!
//! Shuttle's DFS scheduler without random data (`check_dfs`) cannot run a ring: every choice above
//! is a random draw.
//!
//! # Provenance
//!
//! The API mirroring is derived from `turmoil-io-uring` 0.1.0 (MIT, Copyright (c) 2022 Carl Lerche;
//! see `LICENSE-MIT`), which simulates the ring on turmoil's clock and filesystem. This crate keeps
//! its API mirroring and replaces the clock with Shuttle's scheduler, the simulated filesystem with
//! real file descriptors, and its simplified semantics with the kernel's.

#![cfg(target_os = "linux")]

mod async_fd;
pub mod checks;
pub mod cqueue;
pub mod faults;
mod host;
mod kernel;
pub mod opcode;
pub mod register;
pub mod squeue;
mod submit;
pub mod types;

use std::io;
use std::marker::PhantomData;
#[cfg(feature = "io_safety")]
use std::os::fd::{AsFd, BorrowedFd};
use std::os::fd::{AsRawFd, RawFd};
use std::sync::Arc;

pub use async_fd::{AsyncFd, AsyncFdReadyGuard, Interest, TryIoError};
pub use cqueue::CompletionQueue;
pub use register::Probe;
pub use squeue::SubmissionQueue;
pub use submit::{EnterFlags, Submitter};

use kernel::Ring;

/// The most SQ entries a ring can have, `IORING_MAX_ENTRIES`.
const MAX_ENTRIES: u32 = 32768;
/// The most CQ entries a ring can have, `IORING_MAX_CQ_ENTRIES`.
const MAX_CQ_ENTRIES: u32 = 2 * MAX_ENTRIES;

/// A simulated ring, mirroring the real crate's `IoUring`.
pub struct IoUring<S = squeue::Entry, C = cqueue::Entry>
where
    S: squeue::EntryMarker,
    C: cqueue::EntryMarker,
{
    ring: Arc<Ring>,
    params: Parameters,
    _markers: PhantomData<(S, C)>,
}

/// Ring parameters, mirroring the real crate's `Parameters`. The simulated kernel reports the
/// features of a current Linux, except for those of the parts it does not model, which it reports
/// absent.
#[derive(Clone, Debug)]
pub struct Parameters {
    sq_entries: u32,
    cq_entries: u32,
    single_issuer: bool,
}

impl Parameters {
    pub fn is_setup_sqpoll(&self) -> bool {
        false
    }

    pub fn is_setup_iopoll(&self) -> bool {
        false
    }

    pub fn is_setup_single_issuer(&self) -> bool {
        self.single_issuer
    }

    pub fn is_feature_single_mmap(&self) -> bool {
        true
    }

    /// Completions that find the CQ full are held back, not dropped.
    pub fn is_feature_nodrop(&self) -> bool {
        true
    }

    pub fn is_feature_submit_stable(&self) -> bool {
        true
    }

    /// A read or write at offset `-1` uses the file position.
    pub fn is_feature_rw_cur_pos(&self) -> bool {
        true
    }

    pub fn is_feature_cur_personality(&self) -> bool {
        true
    }

    pub fn is_feature_fast_poll(&self) -> bool {
        true
    }

    /// The CQ ring has its flags word (`IORING_CQ_EVENTFD_DISABLED`).
    pub fn is_feature_poll_32bits(&self) -> bool {
        true
    }

    pub fn is_feature_sqpoll_nonfixed(&self) -> bool {
        true
    }

    /// Enters take [`SubmitArgs`](types::SubmitArgs) timeouts.
    pub fn is_feature_ext_arg(&self) -> bool {
        true
    }

    pub fn is_feature_native_workers(&self) -> bool {
        true
    }

    /// `false`: registered resources are not simulated.
    pub fn is_feature_resource_tagging(&self) -> bool {
        false
    }

    /// `false`: `IOSQE_CQE_SKIP_SUCCESS` posts `-EINVAL`.
    pub fn is_feature_skip_cqe_on_success(&self) -> bool {
        false
    }

    /// `false`: linked SQEs are not simulated.
    pub fn is_feature_linked_file(&self) -> bool {
        false
    }

    /// `false`: there are no socket opcodes.
    pub fn is_feature_recvsend_bundle(&self) -> bool {
        false
    }

    /// `false`: a minimum wait would need a clock.
    pub fn is_feature_min_timeout(&self) -> bool {
        false
    }

    /// `false`: see [`Submitter::submit_with_args_and_flags`].
    pub fn is_feature_no_iowait(&self) -> bool {
        false
    }

    pub fn sq_entries(&self) -> u32 {
        self.sq_entries
    }

    /// Always zero: there is no SQ polling thread.
    pub fn sq_thread_idle(&self) -> u32 {
        0
    }

    pub fn cq_entries(&self) -> u32 {
        self.cq_entries
    }
}

/// Ring setup options, mirroring the real crate's `Builder`.
///
/// `SQPOLL` and `IOPOLL` fail the build with `EINVAL`; the task-run flags are accepted (see the crate
/// docs on deferred task work), and so are the options that make no difference to the simulation.
/// The entry counts are checked and rounded as the kernel does.
#[derive(Clone)]
pub struct Builder<S = squeue::Entry, C = cqueue::Entry>
where
    S: squeue::EntryMarker,
    C: cqueue::EntryMarker,
{
    sqpoll: bool,
    iopoll: bool,
    single_issuer: bool,
    clamp: bool,
    r_disabled: bool,
    cq_entries: Option<u32>,
    _markers: PhantomData<(S, C)>,
}

impl<S: squeue::EntryMarker, C: cqueue::EntryMarker> Builder<S, C> {
    /// Accepted: the simulation maps no memory for a child process to inherit.
    pub fn dontfork(&mut self) -> &mut Self {
        self
    }

    pub fn setup_iopoll(&mut self) -> &mut Self {
        self.iopoll = true;
        self
    }

    pub fn setup_sqpoll(&mut self, _idle: u32) -> &mut Self {
        self.sqpoll = true;
        self
    }

    /// Accepted; `SQPOLL` itself fails the build.
    pub fn setup_sqpoll_cpu(&mut self, _cpu: u32) -> &mut Self {
        self
    }

    pub fn setup_cqsize(&mut self, entries: u32) -> &mut Self {
        self.cq_entries = Some(entries);
        self
    }

    pub fn setup_clamp(&mut self) -> &mut Self {
        self.clamp = true;
        self
    }

    /// Accepted: the simulation has no worker pool to share.
    pub fn setup_attach_wq(&mut self, _fd: RawFd) -> &mut Self {
        self
    }

    /// Build the ring disabled: it refuses submissions with `EBADFD` until
    /// [`Submitter::register_enable_rings`].
    pub fn setup_r_disabled(&mut self) -> &mut Self {
        self.r_disabled = true;
        self
    }

    pub fn setup_submit_all(&mut self) -> &mut Self {
        self
    }

    pub fn setup_coop_taskrun(&mut self) -> &mut Self {
        self
    }

    pub fn setup_taskrun_flag(&mut self) -> &mut Self {
        self
    }

    pub fn setup_defer_taskrun(&mut self) -> &mut Self {
        self
    }

    pub fn setup_single_issuer(&mut self) -> &mut Self {
        self.single_issuer = true;
        self
    }

    /// Accepted: the simulation has no SQ array.
    pub fn setup_no_sqarray(&mut self) -> &mut Self {
        self
    }

    /// Build a ring with `entries` SQ entries, rounded up to a power of two, and twice as many CQ
    /// entries unless [`setup_cqsize`](Self::setup_cqsize) says otherwise. As the kernel does, fails
    /// with `EINVAL` for no entries, and for more than 32768 SQ or 65536 CQ entries or fewer CQ than
    /// SQ entries, unless [`setup_clamp`](Self::setup_clamp) lowers the counts to the maximums.
    ///
    /// Panics outside of a Shuttle test.
    pub fn build(&self, entries: u32) -> io::Result<IoUring<S, C>> {
        let einval = || io::Error::from_raw_os_error(libc::EINVAL);
        if self.sqpoll || self.iopoll || entries == 0 {
            return Err(einval());
        }
        let entries = match entries {
            entries if entries <= MAX_ENTRIES => entries,
            _ if self.clamp => MAX_ENTRIES,
            _ => return Err(einval()),
        };
        let sq_entries = entries.next_power_of_two();
        let cq_entries = match self.cq_entries {
            None => 2 * sq_entries,
            Some(0) => return Err(einval()),
            Some(entries) => {
                let entries = match entries {
                    entries if entries <= MAX_CQ_ENTRIES => entries,
                    _ if self.clamp => MAX_CQ_ENTRIES,
                    _ => return Err(einval()),
                };
                let entries = entries.next_power_of_two();
                if entries < sq_entries {
                    return Err(einval());
                }
                entries
            }
        };
        Ok(IoUring {
            ring: Ring::start(sq_entries, cq_entries, self.r_disabled),
            params: Parameters {
                sq_entries,
                cq_entries,
                single_issuer: self.single_issuer,
            },
            _markers: PhantomData,
        })
    }
}

impl IoUring<squeue::Entry, cqueue::Entry> {
    /// A ring with `entries` SQ entries and twice as many CQ entries. Panics outside of a Shuttle
    /// test.
    pub fn new(entries: u32) -> io::Result<Self> {
        Self::builder().build(entries)
    }
}

impl<S: squeue::EntryMarker, C: cqueue::EntryMarker> IoUring<S, C> {
    pub fn builder() -> Builder<S, C> {
        Builder {
            sqpoll: false,
            iopoll: false,
            single_issuer: false,
            clamp: false,
            r_disabled: false,
            cq_entries: None,
            _markers: PhantomData,
        }
    }

    pub fn submitter(&self) -> Submitter<'_> {
        Submitter::new(&self.ring)
    }

    pub fn params(&self) -> &Parameters {
        &self.params
    }

    pub fn submit(&self) -> io::Result<usize> {
        self.submitter().submit()
    }

    pub fn submit_and_wait(&self, want: usize) -> io::Result<usize> {
        self.submitter().submit_and_wait(want)
    }

    pub fn split(
        &mut self,
    ) -> (
        Submitter<'_>,
        squeue::SubmissionQueue<'_, S>,
        cqueue::CompletionQueue<'_, C>,
    ) {
        (
            Submitter::new(&self.ring),
            squeue::SubmissionQueue::new(&self.ring),
            cqueue::CompletionQueue::new(&self.ring),
        )
    }

    pub fn submission(&mut self) -> squeue::SubmissionQueue<'_, S> {
        squeue::SubmissionQueue::new(&self.ring)
    }

    /// # Safety
    ///
    /// As in the real crate: the caller must be the queue's only writer.
    pub unsafe fn submission_shared(&self) -> squeue::SubmissionQueue<'_, S> {
        squeue::SubmissionQueue::new(&self.ring)
    }

    /// A handle on the completion queue, which loads the CQ tail: a Shuttle scheduling point.
    pub fn completion(&mut self) -> cqueue::CompletionQueue<'_, C> {
        cqueue::CompletionQueue::new(&self.ring)
    }

    /// # Safety
    ///
    /// As in the real crate: the caller must be the queue's only reader.
    pub unsafe fn completion_shared(&self) -> cqueue::CompletionQueue<'_, C> {
        cqueue::CompletionQueue::new(&self.ring)
    }
}

impl<S: squeue::EntryMarker, C: cqueue::EntryMarker> Drop for IoUring<S, C> {
    fn drop(&mut self) {
        self.ring.close();
    }
}

impl<S: squeue::EntryMarker, C: cqueue::EntryMarker> AsRawFd for IoUring<S, C> {
    /// The simulated ring fd, which an [`AsyncFd`] can wait on.
    fn as_raw_fd(&self) -> RawFd {
        self.ring.fd
    }
}

#[cfg(feature = "io_safety")]
impl<S: squeue::EntryMarker, C: cqueue::EntryMarker> AsFd for IoUring<S, C> {
    /// The simulated ring fd. It names no open file: use it only to wait on it with an [`AsyncFd`],
    /// never in a system call.
    fn as_fd(&self) -> BorrowedFd<'_> {
        // SAFETY: the fd is the simulated ring's number, valid for as long as the ring; nothing reads
        // it as a kernel descriptor.
        unsafe { BorrowedFd::borrow_raw(self.ring.fd) }
    }
}
