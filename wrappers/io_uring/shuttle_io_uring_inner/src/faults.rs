// This crate began as a fork of turmoil-io-uring 0.1.0 (crates/turmoil-io-uring in
// https://github.com/tokio-rs/turmoil, commit 684acc1a8eea3a9cf2c6959dc47b69dba981cac1),
// Copyright (c) 2022 Carl Lerche, which is licensed under the MIT license: see LICENSE-MIT in this
// crate. This file is new in the fork, and is licensed under the Apache License, Version 2.0, as
// the rest of Shuttle is.

//! Faults the simulated kernel injects: refusals and errors a real kernel can post, chosen with
//! `shuttle::rand` so a schedule replays them.
//!
//! The settings belong to the running execution, as every ring does: they start at the defaults,
//! which inject nothing, in every execution, and apply to every ring of the execution from the call
//! that sets them on. So a test sets them inside the test function, before it submits the ops they
//! are for. Like the rest of Shuttle, the functions here panic outside of a Shuttle test.

use shuttle::rand::{Rng, thread_rng};

use crate::host::Host;

/// What the kernel injects. The default injects nothing.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Faults {
    /// The probability that a read or write submitted without `IOSQE_ASYNC` is refused with
    /// `-EAGAIN`, as the kernel refuses one it tried nonblocking and declined to reissue. A refused
    /// op may have moved part of its range first: a write some of its blocks to the file, a read
    /// some of its buffer.
    pub eagain: f64,
}

/// Inject `faults` into the ops of this execution's rings from now on.
pub fn set_faults(faults: Faults) {
    Host::current().lock().faults = faults;
}

/// The faults this execution's rings inject.
pub fn faults() -> Faults {
    Host::current().lock().faults
}

/// Errors the kernel posts for ops it accepted, none by default. Unlike a refusal, which the
/// submitter resubmits, each surfaces to the caller, so a test turns them on around the ops allowed
/// to fail and off for the ones that must not.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Errors {
    /// The probability a read fails with `-EIO`, having moved part of its range, as a device error
    /// mid-transfer would.
    pub eio: f64,
    /// The probability a read returns short: it moves a block-aligned part of its range, short of all
    /// of it, and reports that count.
    pub short_read: f64,
    /// The probability a `fallocate` fails with `-ENOSPC`.
    pub enospc: f64,
}

/// How many errors of each kind the kernel posted in this execution.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Posted {
    pub eio: u64,
    pub short_reads: u64,
    pub enospc: u64,
}

/// Post `errors` for the ops of this execution's rings from now on.
pub fn set_errors(errors: Errors) {
    Host::current().lock().errors = errors;
}

/// The errors this execution's rings post.
pub fn errors() -> Errors {
    Host::current().lock().errors
}

/// The errors this execution's rings have posted since the last call, and reset the counts.
pub fn take_posted() -> Posted {
    std::mem::take(&mut Host::current().lock().posted)
}

/// Draw with probability `p`, drawing nothing at 0 so a run without faults schedules as it would
/// without this module.
fn draw(p: f64) -> bool {
    p > 0.0 && thread_rng().gen_bool(p.min(1.0))
}

/// Whether to refuse a read or write that may be refused.
pub(crate) fn refuse(host: &Host) -> bool {
    let p = host.lock().faults.eagain;
    draw(p)
}

/// Whether to fail a read with `-EIO`.
pub(crate) fn fail_read(host: &Host) -> bool {
    let p = host.lock().errors.eio;
    let fail = draw(p);
    if fail {
        host.lock().posted.eio += 1;
    }
    fail
}

/// Whether to return a read short.
pub(crate) fn short_read(host: &Host) -> bool {
    let p = host.lock().errors.short_read;
    draw(p)
}

/// Count a short read posted.
pub(crate) fn note_short_read(host: &Host) {
    host.lock().posted.short_reads += 1;
}

/// Whether to fail a `fallocate` with `-ENOSPC`.
pub(crate) fn no_space(host: &Host) -> bool {
    let p = host.lock().errors.enospc;
    let fail = draw(p);
    if fail {
        host.lock().posted.enospc += 1;
    }
    fail
}
