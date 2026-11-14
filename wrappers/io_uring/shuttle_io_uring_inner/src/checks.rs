// This crate began as a fork of turmoil-io-uring 0.1.0 (crates/turmoil-io-uring in
// https://github.com/tokio-rs/turmoil, commit 684acc1a8eea3a9cf2c6959dc47b69dba981cac1),
// Copyright (c) 2022 Carl Lerche, which is licensed under the MIT license: see LICENSE-MIT in this
// crate. This file is new in the fork, and is licensed under the Apache License, Version 2.0, as
// the rest of Shuttle is.

//! Checks the simulated kernel makes of how a ring's user treats the memory and the files of the ops
//! in flight. Each failed check panics, in the task that submitted the op or in the ring's kernel
//! task, with a message that names the op by its `user_data`; the failing schedule replays.
//!
//! The io_uring contract is that the kernel owns an op's buffers from its submission until its
//! completion is reaped: it reads a write's buffer, and writes a read's, at any point in between. On
//! Linux, breaking it shows up, if at all, as corrupt data some time later. Here:
//!
//! - **A write's buffer** is copied when the op is submitted and compared when it completes. A
//!   difference means it was changed while the kernel could still read it, and the op panics,
//!   naming the first byte that differs.
//! - **A read's buffer** is saved and filled with `0xA5` bytes when the op goes in flight, so that
//!   code that consumes it before the completion sees garbage rather than plausible data. When the
//!   op completes it must still hold only `0xA5`s, or something wrote into it while the kernel could
//!   too, and the op panics. Otherwise the saved bytes go back before the transfer, so what the read
//!   does not cover (past a short read's count, say) is as it was.
//! - **Two ops in flight whose buffers overlap**, where either is a read, panic when the second is
//!   submitted: the kernel would write the same memory from both, or write memory it is reading for
//!   the other. Two writes from one buffer are fine.
//!
//! These are the [`Checks::buffers`] checks, which are on by default. A buffer freed while its op is
//! in flight is undefined behaviour, here as with the kernel, and is not detected.
//!
//! [`Checks::file_ranges`], off by default, also panics when two ops in flight on the same file (by
//! device and inode) cover overlapping byte ranges and either is a write: which of the two the file
//! ends up with, or which data the read returns, then depends on the order they complete in, which
//! the kernel does not promise. Code that orders such ops itself, by waiting for one's completion
//! before submitting the other, never trips it.
//!
//! The settings belong to the running execution and start at the defaults in every execution, as
//! the [`faults`](crate::faults) do.

use crate::host::Host;

/// The checks the simulated kernel makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Checks {
    /// Check that no op's buffer is changed, or shared with a read, while the op is in flight. On by
    /// default.
    pub buffers: bool,
    /// Check that no two ops in flight cover overlapping ranges of a file where either is a write.
    /// Off by default.
    pub file_ranges: bool,
}

impl Default for Checks {
    fn default() -> Self {
        Self {
            buffers: true,
            file_ranges: false,
        }
    }
}

/// Make `checks` for the ops this execution's rings submit from now on.
pub fn set_checks(checks: Checks) {
    Host::current().lock().checks = checks;
}

/// The checks this execution's rings make.
pub fn checks() -> Checks {
    Host::current().lock().checks
}
