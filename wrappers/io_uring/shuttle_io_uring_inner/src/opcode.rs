// This file is derived from turmoil-io-uring 0.1.0 (crates/turmoil-io-uring in
// https://github.com/tokio-rs/turmoil, commit 684acc1a8eea3a9cf2c6959dc47b69dba981cac1),
// Copyright (c) 2022 Carl Lerche, and licensed under the MIT license: see LICENSE-MIT in this crate.
// The changes to it are licensed under the Apache License, Version 2.0, as the rest of Shuttle is.

//! Opcode builders mirroring [`io_uring::opcode`](https://docs.rs/io-uring/0.7/io_uring/opcode).
//!
//! The simulated set is what file I/O on a ring needs: [`Nop`], [`Read`], [`Readv`], [`Write`],
//! [`Writev`], [`Fsync`], [`Fallocate`], [`Timeout`] and [`AsyncCancel`]. Signatures and `CODE`
//! values match the real crate's, so a consumer compiles unchanged; an opcode outside this set has no
//! builder here, so a consumer that uses one fails to build under the `shuttle` feature rather than at
//! run time.

use crate::squeue::{Entry, SqOp};
use crate::types::sealed::{Target, UseFixed};
use crate::types::{FsyncFlags, TimeoutFlags, Timespec};

/// Do nothing, mirroring `IORING_OP_NOP`.
#[derive(Debug, Default)]
pub struct Nop {}

impl Nop {
    pub const CODE: u8 = 0;

    pub fn new() -> Self {
        Self {}
    }

    pub fn build(self) -> Entry {
        Entry::new(SqOp::Nop, None)
    }
}

/// Read into a buffer at an offset, mirroring `IORING_OP_READ`.
#[derive(Debug)]
pub struct Read {
    fd: Target,
    buf: *mut u8,
    len: u32,
    offset: u64,
}

impl Read {
    pub const CODE: u8 = 22;

    pub fn new(fd: impl UseFixed, buf: *mut u8, len: u32) -> Self {
        Self {
            fd: fd.into(),
            buf,
            len,
            offset: 0,
        }
    }

    pub const fn offset(mut self, offset: u64) -> Self {
        self.offset = offset;
        self
    }

    /// Accepted for source compatibility; I/O priority is not simulated.
    pub const fn ioprio(self, _ioprio: u16) -> Self {
        self
    }

    /// Accepted for source compatibility; per-op RWF flags are not simulated.
    pub const fn rw_flags(self, _rw_flags: i32) -> Self {
        self
    }

    /// Accepted for source compatibility. A buffer group takes effect only with
    /// `IOSQE_BUFFER_SELECT`, which posts `-EINVAL`.
    pub const fn buf_group(self, _buf_group: u16) -> Self {
        self
    }

    pub fn build(self) -> Entry {
        Entry::new(
            SqOp::Read {
                fd: self.fd,
                buf: self.buf,
                len: self.len,
                offset: self.offset,
            },
            Some(self.fd),
        )
    }
}

/// Write a buffer at an offset, mirroring `IORING_OP_WRITE`.
#[derive(Debug)]
pub struct Write {
    fd: Target,
    buf: *const u8,
    len: u32,
    offset: u64,
}

impl Write {
    pub const CODE: u8 = 23;

    pub fn new(fd: impl UseFixed, buf: *const u8, len: u32) -> Self {
        Self {
            fd: fd.into(),
            buf,
            len,
            offset: 0,
        }
    }

    pub const fn offset(mut self, offset: u64) -> Self {
        self.offset = offset;
        self
    }

    /// Accepted for source compatibility; I/O priority is not simulated.
    pub const fn ioprio(self, _ioprio: u16) -> Self {
        self
    }

    /// Accepted for source compatibility; per-op RWF flags are not simulated.
    pub const fn rw_flags(self, _rw_flags: i32) -> Self {
        self
    }

    /// Accepted for source compatibility; write streams are not simulated.
    pub const fn write_stream(self, _write_stream: u8) -> Self {
        self
    }

    pub fn build(self) -> Entry {
        Entry::new(
            SqOp::Write {
                fd: self.fd,
                buf: self.buf,
                len: self.len,
                offset: self.offset,
            },
            Some(self.fd),
        )
    }
}

/// Read into several buffers at an offset, mirroring `IORING_OP_READV`.
///
/// The kernel copies the `iovec` array when it consumes the entry, at
/// submission (`IORING_FEAT_SUBMIT_STABLE`), so the array need only stay valid
/// until the submit; the buffers it points to, until the completion is reaped.
#[derive(Debug)]
pub struct Readv {
    fd: Target,
    iovec: *const libc::iovec,
    len: u32,
    offset: u64,
}

impl Readv {
    pub const CODE: u8 = 1;

    /// `len` is the number of entries in the `iovec` array.
    pub fn new(fd: impl UseFixed, iovec: *const libc::iovec, len: u32) -> Self {
        Self {
            fd: fd.into(),
            iovec,
            len,
            offset: 0,
        }
    }

    pub const fn offset(mut self, offset: u64) -> Self {
        self.offset = offset;
        self
    }

    /// Accepted for source compatibility; I/O priority is not simulated.
    pub const fn ioprio(self, _ioprio: u16) -> Self {
        self
    }

    /// Accepted for source compatibility; per-op RWF flags are not simulated.
    pub const fn rw_flags(self, _rw_flags: i32) -> Self {
        self
    }

    /// Accepted for source compatibility. A buffer group takes effect only
    /// with `IOSQE_BUFFER_SELECT`, which posts `-EINVAL`.
    pub const fn buf_group(self, _buf_group: u16) -> Self {
        self
    }

    pub fn build(self) -> Entry {
        Entry::new(
            SqOp::Readv {
                fd: self.fd,
                iovec: self.iovec,
                len: self.len,
                offset: self.offset,
            },
            Some(self.fd),
        )
    }
}

/// Write several buffers at an offset, mirroring `IORING_OP_WRITEV`.
///
/// The `iovec` array is copied at submission, as for [`Readv`].
#[derive(Debug)]
pub struct Writev {
    fd: Target,
    iovec: *const libc::iovec,
    len: u32,
    offset: u64,
}

impl Writev {
    pub const CODE: u8 = 2;

    /// `len` is the number of entries in the `iovec` array.
    pub fn new(fd: impl UseFixed, iovec: *const libc::iovec, len: u32) -> Self {
        Self {
            fd: fd.into(),
            iovec,
            len,
            offset: 0,
        }
    }

    pub const fn offset(mut self, offset: u64) -> Self {
        self.offset = offset;
        self
    }

    /// Accepted for source compatibility; I/O priority is not simulated.
    pub const fn ioprio(self, _ioprio: u16) -> Self {
        self
    }

    /// Accepted for source compatibility; per-op RWF flags are not simulated.
    pub const fn rw_flags(self, _rw_flags: i32) -> Self {
        self
    }

    /// Accepted for source compatibility; write streams are not simulated.
    pub const fn write_stream(self, _write_stream: u8) -> Self {
        self
    }

    pub fn build(self) -> Entry {
        Entry::new(
            SqOp::Writev {
                fd: self.fd,
                iovec: self.iovec,
                len: self.len,
                offset: self.offset,
            },
            Some(self.fd),
        )
    }
}

/// Flush a file, mirroring `IORING_OP_FSYNC`.
///
/// The simulation always flushes the whole file, which a sync of a range (set with
/// [`offset`](Self::offset) and [`len`](Self::len)) promises at least.
#[derive(Debug)]
pub struct Fsync {
    fd: Target,
    flags: FsyncFlags,
}

impl Fsync {
    pub const CODE: u8 = 3;

    pub fn new(fd: impl UseFixed) -> Self {
        Self {
            fd: fd.into(),
            flags: FsyncFlags::empty(),
        }
    }

    /// Accepted: the simulation syncs the whole file.
    pub const fn offset(self, _offset: u64) -> Self {
        self
    }

    /// Accepted: the simulation syncs the whole file.
    pub const fn len(self, _len: u32) -> Self {
        self
    }

    pub const fn flags(mut self, flags: FsyncFlags) -> Self {
        self.flags = flags;
        self
    }

    pub fn build(self) -> Entry {
        Entry::new(
            SqOp::Fsync {
                fd: self.fd,
                datasync: self.flags.contains(FsyncFlags::DATASYNC),
            },
            Some(self.fd),
        )
    }
}

/// Allocate file space, mirroring `IORING_OP_FALLOCATE`.
#[derive(Debug)]
pub struct Fallocate {
    fd: Target,
    len: u64,
    offset: u64,
    mode: i32,
}

impl Fallocate {
    pub const CODE: u8 = 17;

    pub fn new(fd: impl UseFixed, len: u64) -> Self {
        Self {
            fd: fd.into(),
            len,
            offset: 0,
            mode: 0,
        }
    }

    pub const fn offset(mut self, offset: u64) -> Self {
        self.offset = offset;
        self
    }

    pub const fn mode(mut self, mode: i32) -> Self {
        self.mode = mode;
        self
    }

    pub fn build(self) -> Entry {
        Entry::new(
            SqOp::Fallocate {
                fd: self.fd,
                offset: self.offset,
                len: self.len,
                mode: self.mode,
            },
            Some(self.fd),
        )
    }
}

/// A timeout, mirroring `IORING_OP_TIMEOUT`.
///
/// It completes with `0` once `count` other completions have posted after it was submitted (when
/// `count` is non-zero), or with `-ETIME` when the ring's kernel task fires its timer. Shuttle has no
/// clock, so the duration only says the op is a timer: the kernel task fires it at a step Shuttle
/// chooses, but while I/O is in flight only one time in 256, as a timeout normally outlasts the I/O
/// it guards (see the crate docs).
#[derive(Debug)]
pub struct Timeout {
    count: u32,
}

impl Timeout {
    pub const CODE: u8 = 11;

    /// The kernel reads `*timespec` when the op is submitted; the simulation
    /// never reads it.
    pub fn new(_timespec: *const Timespec) -> Self {
        Self { count: 0 }
    }

    pub const fn count(mut self, count: u32) -> Self {
        self.count = count;
        self
    }

    /// Accepted; see [`TimeoutFlags`].
    pub const fn flags(self, _flags: TimeoutFlags) -> Self {
        self
    }

    pub fn build(self) -> Entry {
        Entry::new(SqOp::Timeout { count: self.count }, None)
    }
}

/// Cancel an in-flight op by its `user_data`, mirroring
/// `IORING_OP_ASYNC_CANCEL`.
#[derive(Debug)]
pub struct AsyncCancel {
    user_data: u64,
}

impl AsyncCancel {
    pub const CODE: u8 = 14;

    pub fn new(user_data: u64) -> Self {
        Self { user_data }
    }

    pub fn build(self) -> Entry {
        Entry::new(
            SqOp::AsyncCancel {
                user_data: self.user_data,
            },
            None,
        )
    }
}

/// The opcodes the simulation completes, for [`Probe`](crate::Probe).
pub(crate) const SUPPORTED: [u8; 9] = [
    Nop::CODE,
    Read::CODE,
    Readv::CODE,
    Write::CODE,
    Writev::CODE,
    Fsync::CODE,
    Fallocate::CODE,
    Timeout::CODE,
    AsyncCancel::CODE,
];
