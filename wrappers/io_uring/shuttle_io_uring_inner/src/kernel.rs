// This file is derived from turmoil-io-uring 0.1.0 (crates/turmoil-io-uring in
// https://github.com/tokio-rs/turmoil, commit 684acc1a8eea3a9cf2c6959dc47b69dba981cac1),
// Copyright (c) 2022 Carl Lerche, and licensed under the MIT license: see LICENSE-MIT in this crate.
// The changes to it are licensed under the Apache License, Version 2.0, as the rest of Shuttle is.

//! The simulated kernel: one ring's queues, the ops it holds in flight, and the Shuttle task that
//! completes them.
//!
//! Each ring has a kernel task. While ops are in flight it is runnable, and each time Shuttle
//! schedules it, it completes one of them, chosen with `shuttle::rand`, by running the op's system
//! call against the real file descriptor. An op therefore stays in flight for as long as Shuttle
//! keeps the task off the CPU, ops complete in any order, and a schedule replays both. Buffers behave
//! as they do with the kernel: a read writes the caller's buffer, and a write reads it, when the op
//! completes. A vectored op's `iovec` array is copied when the op is submitted, as the kernel does
//! with `IORING_FEAT_SUBMIT_STABLE`; the buffers it lists, at completion.
//!
//! Posting a completion follows the kernel's order (`io_uring/eventfd.c`): store the CQ tail, then
//! load the CQ flags and signal the registered eventfd unless `IORING_CQ_EVENTFD_DISABLED` is set.
//! Both words are Shuttle atomics, so a consumer that clears the flag and then loads the tail races
//! the kernel at every interleaving Shuttle can choose.
//!
//! Timers are the timeout SQEs in flight and the bounded waits of blocked enters. The kernel task
//! fires them as it completes ops, but while I/O is in flight only one time in [`HANG_ODDS`].

use std::collections::VecDeque;
use std::future::poll_fn;
use std::io;
use std::mem::{self, MaybeUninit};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};
use std::thread;

use shuttle::current::{self, TaskName};
use shuttle::future;
use shuttle::rand::{Rng, thread_rng};
use shuttle::sync::atomic::{AtomicU32, Ordering};

use crate::async_fd::Readiness;
use crate::cqueue;
use crate::faults;
use crate::host::Host;
use crate::squeue::{self, Flags, SqOp};
use crate::types::Timespec;
use crate::types::sealed::Target;

/// `IORING_CQ_EVENTFD_DISABLED`.
const CQ_EVENTFD_DISABLED: u32 = 1;

/// How often an op completes inline, during the enter that submits it, instead of going in flight.
/// The kernel does this whenever an op can finish without waiting; here it lets a consumer find a
/// completion its own submit just posted.
const INLINE_COMPLETION: f64 = 0.25;

/// One in this many completions taken while both I/O and timers are in flight goes to a timer.
/// Otherwise I/O completes first: a timeout is typically far longer than an I/O (a driver that
/// drains its ring at shutdown might arm a 60 s timer, and a ring thread's bounded wait 100 ms), so
/// a timer that fires with I/O outstanding models a device that outlasts it, a hang, which should be
/// rare rather than a coin flip.
const HANG_ODDS: u32 = 256;

/// The block a refused read or write moves part of its range in: the largest logical block size, so
/// the part stays aligned for `O_DIRECT`.
const PART_BLOCK: u32 = 4096;

/// The offset that makes a read or write use, and advance, the file position, as `read(2)` and
/// `write(2)` do (`IORING_FEAT_RW_CUR_POS`): `-1` as the SQE's `u64`.
const CUR_POS: u64 = u64::MAX;

/// What a read's buffer holds while the read is in flight (see [`crate::checks`]).
const POISON: u8 = 0xa5;

/// One simulated ring.
pub(crate) struct Ring {
    /// The ring's (simulated) file descriptor.
    pub(crate) fd: RawFd,
    pub(crate) sq_entries: u32,
    pub(crate) cq_entries: u32,
    /// The kernel of the execution that built the ring.
    host: Arc<Host>,
    /// Bookkeeping no consumer can observe directly, behind a plain lock so it adds no scheduling
    /// points of its own.
    state: Mutex<State>,
    /// Completions posted: the kernel stores it after writing a completion, and a consumer's `sync`
    /// loads it.
    cq_tail: AtomicU32,
    /// The CQ ring's flag word.
    cq_flags: AtomicU32,
    /// Readiness of the ring fd itself, for an `AsyncFd` on it. Every post sets it, whatever the
    /// eventfd flag says, as polling the ring fd does.
    readiness: Arc<Readiness>,
}

struct State {
    /// Published SQEs the kernel has not consumed.
    sq: VecDeque<squeue::Entry>,
    /// Ops the kernel holds: submitted and not yet completed. The kernel task takes an op out and
    /// posts its completion with no scheduling point in between, so no other task sees it in
    /// neither place.
    inflight: Vec<InFlight>,
    /// Completions posted and not yet taken.
    cq: VecDeque<cqueue::Entry>,
    /// Completions that found the CQ full. With `IORING_FEAT_NODROP` the kernel holds them back and
    /// flushes them on the next enter.
    overflow: VecDeque<cqueue::Entry>,
    /// Completions in the CQ whose tail store has not happened yet: posted, and still invisible.
    unpublished: u32,
    /// Completions consumers have taken.
    cq_head: u32,
    /// The CQ tail as of the kernel's last store: what an enter waiting for completions counts, so
    /// it never wakes for one a consumer cannot see.
    published: u32,
    /// Completions ever posted, which a timeout with a count counts.
    posted: u64,
    /// The registered-file table, once registered.
    files: Option<Vec<RawFd>>,
    /// The registered eventfd.
    eventfd: Option<EventFd>,
    /// The kernel task, parked while nothing is in flight.
    kernel: Option<Waker>,
    /// Tasks blocked in an enter until completions post.
    waiters: Vec<Waker>,
    /// The timers of the bounded waits of blocked enters, which the kernel task fires as it fires
    /// timeout SQEs.
    wait_timers: Vec<WaitTimer>,
    next_wait_timer: u64,
    /// Built with `IORING_SETUP_R_DISABLED` and not yet enabled.
    disabled: bool,
    /// A timer has fired with I/O outstanding: the device was seen to hang, so the owner may drop
    /// the ring with ops in flight, leaking their buffers (see [`Ring::close`]).
    hung: bool,
    closed: bool,
}

/// An eventfd registered with the ring.
struct EventFd {
    /// The number the ring's user registered, which an `AsyncFd` on the eventfd names.
    fd: RawFd,
    /// The ring's own reference to the eventfd, as the kernel takes one: a duplicate of `fd`, so a
    /// signal still reaches the eventfd after its user closes `fd`, and never a file that reuses the
    /// number.
    own: Arc<OwnedFd>,
    readiness: Arc<Readiness>,
    /// Registered with `register_eventfd_async`: signal only what the kernel task posts.
    async_only: bool,
}

/// The timer of a bounded wait: the wait times out when the kernel task fires it.
struct WaitTimer {
    id: u64,
    waker: Waker,
    fired: bool,
}

/// What the kernel task does next.
enum Work {
    /// Complete an op.
    Op(InFlight),
    /// Wake the enter whose bounded wait just timed out.
    WaitTimedOut(Waker),
}

/// What [`State::pick_next`] picked: an index into `inflight` or into `wait_timers`.
enum Pick {
    Op(usize),
    WaitTimer(usize),
}

struct InFlight {
    user_data: u64,
    op: Op,
    /// A read or write submitted without `IOSQE_ASYNC`, which the kernel may refuse (see
    /// [`faults::Faults::eagain`]).
    refusable: bool,
    /// What [`crate::checks::Checks::buffers`] saved of the op's buffers when it went in flight.
    guard: Option<BufferGuard>,
    /// The op's range of its file, for [`crate::checks::Checks::file_ranges`].
    file_range: Option<FileRange>,
}

impl InFlight {
    /// Complete the op: check and restore its buffers, then run its system call, or refuse it with
    /// `-EAGAIN` at the rate the faults set, after moving part of its range; or post one of the
    /// errors the errors set (see [`faults::Errors`]).
    fn complete(self, host: &Host) -> i32 {
        if let Some(guard) = &self.guard {
            guard.release(self.user_data);
        }
        if self.refusable && faults::refuse(host) {
            self.op.move_part();
            return -libc::EAGAIN;
        }
        match self.op {
            Op::Read { .. } | Op::Readv { .. } if faults::fail_read(host) => {
                self.op.move_part();
                return -libc::EIO;
            }
            Op::Read { .. } | Op::Readv { .. } if faults::short_read(host) => {
                let moved = self.op.move_part();
                if moved > 0 {
                    faults::note_short_read(host);
                    return moved as i32;
                }
            }
            Op::Fallocate { .. } if faults::no_space(host) => return -libc::ENOSPC,
            _ => {}
        }
        self.op.execute()
    }
}

/// An op the kernel holds, its file resolved at submission as the kernel takes its file reference
/// then. An `offset` of [`CUR_POS`] uses the file position.
enum Op {
    Nop,
    Read {
        fd: RawFd,
        buf: *mut u8,
        len: u32,
        offset: u64,
    },
    Write {
        fd: RawFd,
        buf: *const u8,
        len: u32,
        offset: u64,
    },
    /// The `iovec` array is the kernel's copy, taken at submission.
    Readv {
        fd: RawFd,
        iovecs: Vec<libc::iovec>,
        offset: u64,
    },
    Writev {
        fd: RawFd,
        iovecs: Vec<libc::iovec>,
        offset: u64,
    },
    Fsync {
        fd: RawFd,
        datasync: bool,
    },
    Fallocate {
        fd: RawFd,
        offset: u64,
        len: u64,
        mode: i32,
    },
    /// A timer; with a non-zero `count`, also met once `count` completions have posted after
    /// `since`.
    Timeout {
        count: u32,
        since: u64,
    },
}

// SAFETY: as for `SqOp`: the buffers are the submitter's, valid and unaliased until the completion
// is reaped, and only the task completing the op touches them.
unsafe impl Send for Op {}

/// A part of memory an op hands the kernel.
#[derive(Clone, Copy)]
struct MemRange {
    start: usize,
    len: usize,
    /// The kernel writes it (a read's buffer), rather than reads it (a write's).
    kernel_writes: bool,
}

impl MemRange {
    fn overlaps(&self, other: &MemRange) -> bool {
        self.start < other.start + other.len && other.start < self.start + self.len
    }
}

/// A range of a file an op reads or writes.
#[derive(Clone, Copy)]
struct FileRange {
    dev: u64,
    ino: u64,
    start: u64,
    end: u64,
    write: bool,
}

impl Op {
    /// The op's name in a panic message.
    fn name(&self) -> &'static str {
        match self {
            Op::Nop => "nop",
            Op::Read { .. } => "read",
            Op::Write { .. } => "write",
            Op::Readv { .. } => "readv",
            Op::Writev { .. } => "writev",
            Op::Fsync { .. } => "fsync",
            Op::Fallocate { .. } => "fallocate",
            Op::Timeout { .. } => "timeout",
        }
    }

    /// The memory the op hands the kernel, leaving out empty and null buffers, which the system call
    /// fails or ignores itself.
    fn memory(&self) -> Vec<MemRange> {
        let ranges = match self {
            Op::Read { buf, len, .. } => vec![MemRange {
                start: *buf as usize,
                len: *len as usize,
                kernel_writes: true,
            }],
            Op::Write { buf, len, .. } => vec![MemRange {
                start: *buf as usize,
                len: *len as usize,
                kernel_writes: false,
            }],
            Op::Readv { iovecs, .. } | Op::Writev { iovecs, .. } => {
                let kernel_writes = matches!(self, Op::Readv { .. });
                iovecs
                    .iter()
                    .map(|iov| MemRange {
                        start: iov.iov_base as usize,
                        len: iov.iov_len,
                        kernel_writes,
                    })
                    .collect()
            }
            _ => Vec::new(),
        };
        ranges.into_iter().filter(|r| r.start != 0 && r.len > 0).collect()
    }

    /// The op's range of its file, if it covers one of a regular file or block device.
    fn file_range(&self) -> Option<FileRange> {
        let (fd, offset, len, write) = match self {
            Op::Read { fd, len, offset, .. } => (*fd, *offset, u64::from(*len), false),
            Op::Write { fd, len, offset, .. } => (*fd, *offset, u64::from(*len), true),
            Op::Readv { fd, offset, .. } | Op::Writev { fd, offset, .. } => (
                *fd,
                *offset,
                self.transfer_len().unwrap_or(0) as u64,
                matches!(self, Op::Writev { .. }),
            ),
            Op::Fallocate { fd, offset, len, .. } => (*fd, *offset, *len, true),
            _ => return None,
        };
        if offset == CUR_POS || len == 0 {
            return None;
        }
        // SAFETY: `fstat` writes the struct it is given, and only reads the fd.
        let mut stat: libc::stat = unsafe { mem::zeroed() };
        if unsafe { libc::fstat(fd, &mut stat) } != 0 {
            return None;
        }
        let kind = stat.st_mode & libc::S_IFMT;
        if kind != libc::S_IFREG && kind != libc::S_IFBLK {
            return None;
        }
        Some(FileRange {
            dev: stat.st_dev,
            ino: stat.st_ino,
            start: offset,
            end: offset.saturating_add(len),
            write,
        })
    }

    /// Move a part of a read's or write's range, whole blocks short of all of it, as one the kernel
    /// refuses may have before it gave up, and return the bytes moved; nothing for any other op.
    fn move_part(&self) -> usize {
        let Some(len) = self.transfer_len() else {
            return 0;
        };
        // Drawn as a `u32`, as before vectored ops, so recorded schedules of single-buffer ops still
        // replay.
        let blocks = u32::try_from(len / PART_BLOCK as usize).unwrap_or(u32::MAX);
        if blocks == 0 {
            return 0;
        }
        let part = thread_rng().gen_range(0..blocks) as usize * PART_BLOCK as usize;
        if part == 0 {
            return 0;
        }
        // SAFETY: as for `execute`, and `part` is shorter than the buffers. A part that fails moved
        // nothing, which a refusal also allows.
        let moved = unsafe {
            match self {
                Op::Read { fd, buf, offset, .. } => read(*fd, *buf, part, *offset),
                Op::Write { fd, buf, offset, .. } => write(*fd, *buf, part, *offset),
                Op::Readv { fd, iovecs, offset } => readv(*fd, &prefix(iovecs, part), *offset),
                Op::Writev { fd, iovecs, offset } => writev(*fd, &prefix(iovecs, part), *offset),
                _ => unreachable!("only reads and writes move a part"),
            }
        };
        moved.max(0) as usize
    }

    /// The bytes a read or write moves when it completes in full; `None` for any other op.
    fn transfer_len(&self) -> Option<usize> {
        match self {
            Op::Read { len, .. } | Op::Write { len, .. } => Some(*len as usize),
            Op::Readv { iovecs, .. } | Op::Writev { iovecs, .. } => Some(iovecs.iter().map(|iov| iov.iov_len).sum()),
            _ => None,
        }
    }

    /// Run the op's system call and return its CQE result.
    fn execute(self) -> i32 {
        // SAFETY: the io_uring contract: the submitter keeps each buffer valid, and does not touch
        // it, until the op's completion is reaped.
        let ret: isize = unsafe {
            match self {
                Op::Nop => 0,
                Op::Read { fd, buf, len, offset } => read(fd, buf, len as usize, offset),
                Op::Write { fd, buf, len, offset } => write(fd, buf, len as usize, offset),
                Op::Readv { fd, iovecs, offset } => readv(fd, &iovecs, offset),
                Op::Writev { fd, iovecs, offset } => writev(fd, &iovecs, offset),
                Op::Fsync { fd, datasync } => (if datasync { libc::fdatasync(fd) } else { libc::fsync(fd) }) as isize,
                Op::Fallocate { fd, offset, len, mode } => {
                    libc::fallocate(fd, mode, offset as libc::off_t, len as libc::off_t) as isize
                }
                // The kernel task completes a timer when its timeout fires.
                Op::Timeout { .. } => return -libc::ETIME,
            }
        };
        if ret < 0 {
            -io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO)
        } else {
            ret as i32
        }
    }
}

/// `pread(2)`, or `read(2)` at the file position.
///
/// # Safety
///
/// `buf` is valid for writes of `len` bytes.
unsafe fn read(fd: RawFd, buf: *mut u8, len: usize, offset: u64) -> isize {
    unsafe {
        if offset == CUR_POS {
            libc::read(fd, buf.cast(), len)
        } else {
            libc::pread(fd, buf.cast(), len, offset as libc::off_t)
        }
    }
}

/// `pwrite(2)`, or `write(2)` at the file position.
///
/// # Safety
///
/// `buf` is valid for reads of `len` bytes.
unsafe fn write(fd: RawFd, buf: *const u8, len: usize, offset: u64) -> isize {
    unsafe {
        if offset == CUR_POS {
            libc::write(fd, buf.cast(), len)
        } else {
            libc::pwrite(fd, buf.cast(), len, offset as libc::off_t)
        }
    }
}

/// `preadv(2)`, or `readv(2)` at the file position.
///
/// # Safety
///
/// Each `iovec` names memory valid for writes.
unsafe fn readv(fd: RawFd, iovecs: &[libc::iovec], offset: u64) -> isize {
    unsafe {
        if offset == CUR_POS {
            libc::readv(fd, iovecs.as_ptr(), iovecs.len() as libc::c_int)
        } else {
            libc::preadv(fd, iovecs.as_ptr(), iovecs.len() as libc::c_int, offset as libc::off_t)
        }
    }
}

/// `pwritev(2)`, or `writev(2)` at the file position.
///
/// # Safety
///
/// Each `iovec` names memory valid for reads.
unsafe fn writev(fd: RawFd, iovecs: &[libc::iovec], offset: u64) -> isize {
    unsafe {
        if offset == CUR_POS {
            libc::writev(fd, iovecs.as_ptr(), iovecs.len() as libc::c_int)
        } else {
            libc::pwritev(fd, iovecs.as_ptr(), iovecs.len() as libc::c_int, offset as libc::off_t)
        }
    }
}

/// Copy a vectored op's `iovec` array, as the kernel's import of it does when the op is submitted:
/// more than `UIO_MAXIOV` entries is `EINVAL`, and a null array of any is `EFAULT`.
fn import_iovecs(iovec: *const libc::iovec, count: u32) -> Result<Vec<libc::iovec>, i32> {
    if count > libc::UIO_MAXIOV as u32 {
        return Err(libc::EINVAL);
    }
    if count == 0 {
        return Ok(Vec::new());
    }
    if iovec.is_null() {
        return Err(libc::EFAULT);
    }
    // SAFETY: the io_uring contract: the submitter keeps the array valid until the entry is
    // submitted, which is now.
    Ok(unsafe { std::slice::from_raw_parts(iovec, count as usize) }.to_vec())
}

/// The first `bytes` of `iovecs`: whole entries, then the one `bytes` ends in, trimmed.
fn prefix(iovecs: &[libc::iovec], mut bytes: usize) -> Vec<libc::iovec> {
    let mut part = Vec::new();
    for iov in iovecs {
        if bytes == 0 {
            break;
        }
        let len = iov.iov_len.min(bytes);
        part.push(libc::iovec {
            iov_base: iov.iov_base,
            iov_len: len,
        });
        bytes -= len;
    }
    part
}

/// What [`crate::checks::Checks::buffers`] keeps of an op's buffers while it is in flight: a copy of
/// each buffer a write reads, and the contents each buffer a read writes had at submission, which
/// holds [`POISON`] meanwhile.
struct BufferGuard {
    op: &'static str,
    writes: Vec<(*const u8, Vec<u8>)>,
    reads: Vec<(*mut u8, Vec<MaybeUninit<u8>>)>,
}

// SAFETY: the pointers are the op's buffers, which the kernel owns until the op completes (see
// `Op`'s `Send`); only the task holding the guard touches them.
unsafe impl Send for BufferGuard {}

impl BufferGuard {
    /// Save the op's buffers, and fill its read buffers with [`POISON`].
    fn take(op: &Op) -> Self {
        let mut guard = Self {
            op: op.name(),
            writes: Vec::new(),
            reads: Vec::new(),
        };
        for range in op.memory() {
            if range.kernel_writes {
                let buf = range.start as *mut u8;
                let mut saved = Vec::with_capacity(range.len);
                // SAFETY: the io_uring contract makes the buffer the kernel's, valid for writes of
                // `len` bytes, until the op completes. It may be uninitialized, which `MaybeUninit`
                // allows.
                unsafe {
                    std::ptr::copy_nonoverlapping(buf.cast::<MaybeUninit<u8>>(), saved.as_mut_ptr(), range.len);
                    saved.set_len(range.len);
                    std::ptr::write_bytes(buf, POISON, range.len);
                }
                guard.reads.push((buf, saved));
            } else {
                let buf = range.start as *const u8;
                // SAFETY: the io_uring contract: valid for reads of `len` bytes until the op completes.
                let saved = unsafe { std::slice::from_raw_parts(buf, range.len) }.to_vec();
                guard.writes.push((buf, saved));
            }
        }
        guard
    }

    /// Check that nothing touched the buffers while the op was in flight, panicking if something
    /// did, then put back what the read buffers held at submission, for the transfer to overwrite.
    fn release(&self, user_data: u64) {
        let op = self.op;
        for (buf, saved) in &self.writes {
            // SAFETY: as in `take`.
            let now = unsafe { std::slice::from_raw_parts(*buf, saved.len()) };
            if let Some(i) = now.iter().zip(saved).position(|(now, saved)| now != saved) {
                panic!(
                    "shuttle-io-uring: the buffer of the {op} with user_data {user_data:#x} changed while the {op} \
                     was in flight: byte {i} of its {} at {:p} was {:#04x} when the op was submitted and is {:#04x} \
                     now. The kernel reads a write's buffer at any time until the op completes.",
                    saved.len(),
                    buf.wrapping_add(i),
                    saved[i],
                    now[i]
                );
            }
        }
        for (buf, saved) in &self.reads {
            // SAFETY: as in `take`; the bytes are the `POISON` it wrote, or what something else wrote
            // since.
            let now = unsafe { std::slice::from_raw_parts(*buf, saved.len()) };
            if let Some(i) = now.iter().position(|&byte| byte != POISON) {
                panic!(
                    "shuttle-io-uring: something wrote into the buffer of the {op} with user_data {user_data:#x} \
                     while the {op} was in flight: byte {i} of its {} at {:p} is {:#04x}, not the {POISON:#04x} the \
                     simulated kernel filled it with. The kernel writes a read's buffer at any time until the op \
                     completes.",
                    saved.len(),
                    buf.wrapping_add(i),
                    now[i]
                );
            }
            // SAFETY: as in `take`.
            unsafe { std::ptr::copy_nonoverlapping(saved.as_ptr(), buf.cast::<MaybeUninit<u8>>(), saved.len()) };
        }
    }
}

/// Signal an eventfd as the kernel's `eventfd_signal` does: add 1 to its counter. The kernel's never
/// blocks, saturating the counter instead, so this writes only if the write would not block.
fn signal_eventfd(fd: RawFd) {
    let mut pollfd = libc::pollfd {
        fd,
        events: libc::POLLOUT,
        revents: 0,
    };
    // SAFETY: plain system calls on an fd the ring owns, with buffers that live across them.
    unsafe {
        if libc::poll(&mut pollfd, 1, 0) == 1 && pollfd.revents & libc::POLLOUT != 0 {
            let one: u64 = 1;
            libc::write(fd, (&raw const one).cast(), mem::size_of::<u64>());
        }
    }
}

/// Whether `fd` is an eventfd, or cannot be told: the kernel refuses to register anything else.
fn maybe_eventfd(fd: RawFd) -> bool {
    match std::fs::read_link(format!("/proc/self/fd/{fd}")) {
        Ok(target) => target.as_os_str() == "anon_inode:[eventfd]",
        Err(_) => true,
    }
}

impl Ring {
    /// A new ring and its kernel task. Panics outside of a Shuttle test.
    pub(crate) fn start(sq_entries: u32, cq_entries: u32, disabled: bool) -> Arc<Ring> {
        let host = Host::current();
        let fd = host.alloc_ring_fd();
        let ring = Arc::new(Ring {
            fd,
            sq_entries,
            cq_entries,
            host: Arc::clone(&host),
            state: Mutex::new(State {
                sq: VecDeque::new(),
                inflight: Vec::new(),
                cq: VecDeque::new(),
                overflow: VecDeque::new(),
                unpublished: 0,
                cq_head: 0,
                published: 0,
                posted: 0,
                files: None,
                eventfd: None,
                kernel: None,
                waiters: Vec::new(),
                wait_timers: Vec::new(),
                next_wait_timer: 0,
                disabled,
                hung: false,
                closed: false,
            }),
            cq_tail: AtomicU32::new(0),
            cq_flags: AtomicU32::new(0),
            readiness: Arc::new(Readiness::new()),
        });
        host.register(fd, Arc::clone(&ring.readiness));
        let kernel = Arc::clone(&ring);
        // Detached: the task ends once the ring is closed.
        drop(future::spawn(async move {
            current::set_label_for_task(
                current::me(),
                TaskName::from(format!("io_uring kernel (ring {})", kernel.fd)),
            );
            while let Some(work) = poll_fn(|cx| kernel.poll_next(cx)).await {
                match work {
                    Work::Op(op) => {
                        let user_data = op.user_data;
                        let result = op.complete(&kernel.host);
                        kernel.post(user_data, result, true);
                    }
                    Work::WaitTimedOut(waiter) => waiter.wake(),
                }
                future::yield_now().await;
            }
        }));
        ring
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Take the next op to complete or timer to fire, chosen by Shuttle, or park until there is one.
    /// Ends the kernel task once the ring is closed.
    fn poll_next(&self, cx: &mut Context<'_>) -> Poll<Option<Work>> {
        let mut state = self.lock();
        if state.closed {
            return Poll::Ready(None);
        }
        if state.inflight.is_empty() && !state.wait_timers.iter().any(|t| !t.fired) {
            state.kernel = Some(cx.waker().clone());
            return Poll::Pending;
        }
        let work = match state.pick_next() {
            Pick::Op(i) => {
                let op = state.inflight.swap_remove(i);
                if matches!(op.op, Op::Timeout { .. }) && state.io_in_flight() {
                    state.hung = true;
                }
                Work::Op(op)
            }
            Pick::WaitTimer(i) => {
                if state.io_in_flight() {
                    state.hung = true;
                }
                let timer = &mut state.wait_timers[i];
                timer.fired = true;
                Work::WaitTimedOut(timer.waker.clone())
            }
        };
        Poll::Ready(Some(work))
    }

    // ---- submission queue ----

    /// Make entries a queue handle pushed visible to the kernel.
    pub(crate) fn publish_sqes(&self, entries: Vec<squeue::Entry>) {
        self.lock().sq.extend(entries);
    }

    /// Published entries the kernel has not consumed.
    pub(crate) fn sq_published(&self) -> usize {
        self.lock().sq.len()
    }

    // ---- completion queue ----

    /// Completions visible to a consumer that loads the tail now. Loading the tail is a scheduling
    /// point.
    pub(crate) fn cq_visible(&self) -> u32 {
        let tail = self.cq_tail.load(Ordering::SeqCst);
        tail.wrapping_sub(self.lock().cq_head)
    }

    /// Take the oldest posted completion. Only called for a completion a tail load exposed, so one
    /// is there.
    pub(crate) fn take_cqe(&self) -> cqueue::Entry {
        let mut state = self.lock();
        let entry = state
            .cq
            .pop_front()
            .expect("a completion the CQ tail exposed is posted");
        state.cq_head = state.cq_head.wrapping_add(1);
        entry
    }

    pub(crate) fn cq_overflowed(&self) -> bool {
        !self.lock().overflow.is_empty()
    }

    pub(crate) fn eventfd_disabled(&self) -> bool {
        self.cq_flags.load(Ordering::SeqCst) & CQ_EVENTFD_DISABLED != 0
    }

    pub(crate) fn set_eventfd_disabled(&self, disabled: bool) {
        if disabled {
            self.cq_flags.fetch_or(CQ_EVENTFD_DISABLED, Ordering::SeqCst);
        } else {
            self.cq_flags.fetch_and(!CQ_EVENTFD_DISABLED, Ordering::SeqCst);
        }
    }

    /// Post a completion, in the kernel's order: store the tail, wake waiters, then load the flags
    /// and signal the eventfd if allowed: add to its counter, then signal an `AsyncFd` waiting on it.
    /// `from_kernel` says the kernel task posts it, rather than an enter as it submits the op.
    fn post(&self, user_data: u64, result: i32, from_kernel: bool) {
        let (added, eventfd) = {
            let mut state = self.lock();
            let mut added = state.push_cqe(
                cqueue::Entry {
                    user_data,
                    result,
                    flags: 0,
                },
                self.cq_entries,
            );
            added += state.complete_counted_timeouts(self.cq_entries);
            let eventfd = state
                .eventfd
                .as_ref()
                .filter(|eventfd| from_kernel || !eventfd.async_only)
                .map(|eventfd| (Arc::clone(&eventfd.own), Arc::clone(&eventfd.readiness)));
            (added, eventfd)
        };
        self.publish(added);
        self.readiness.signal();
        if let Some((own, readiness)) = eventfd
            && self.cq_flags.load(Ordering::SeqCst) & CQ_EVENTFD_DISABLED == 0
        {
            signal_eventfd(own.as_raw_fd());
            readiness.signal();
        }
    }

    /// Move held-back completions into the CQ as room allows, as every enter does.
    fn flush_overflow(&self) {
        let moved = {
            let mut state = self.lock();
            let mut moved = 0;
            while (state.cq.len() as u32) < self.cq_entries {
                let Some(entry) = state.overflow.pop_front() else {
                    break;
                };
                state.cq.push_back(entry);
                state.unpublished += 1;
                moved += 1;
            }
            moved
        };
        self.publish(moved);
    }

    /// Store the tail past `added` new completions, then wake the enters waiting for them.
    fn publish(&self, added: u32) {
        if added == 0 {
            return;
        }
        self.cq_tail.fetch_add(added, Ordering::SeqCst);
        let waiters = {
            let mut state = self.lock();
            state.published = state.published.wrapping_add(added);
            state.unpublished -= added;
            mem::take(&mut state.waiters)
        };
        for waiter in waiters {
            waiter.wake();
        }
    }

    // ---- enter ----

    /// `io_uring_enter`: submit up to `to_submit` published entries, then, with `getevents`, block
    /// until `min_complete` completions are visible (at most a full CQ's worth).
    ///
    /// As the kernel's, an enter that submitted entries returns how many, whatever became of its
    /// wait, and one that submitted fewer than `to_submit` does not wait.
    pub(crate) fn enter(
        &self,
        to_submit: u32,
        min_complete: u32,
        getevents: bool,
        timeout: Option<Timespec>,
    ) -> io::Result<usize> {
        self.flush_overflow();
        let mut submitted = 0;
        if to_submit > 0 {
            if self.lock().disabled {
                return Err(io::Error::from_raw_os_error(libc::EBADFD));
            }
            if self.cq_overflowed() {
                // The kernel refuses new work while it cannot flush the CQ.
                return Err(io::Error::from_raw_os_error(libc::EBUSY));
            }
            submitted = self.submit(to_submit);
            if submitted != to_submit as usize {
                return Ok(submitted);
            }
        }
        if getevents && min_complete > 0 {
            let waited = self.wait(min_complete.min(self.cq_entries), timeout);
            if submitted == 0 {
                waited?;
            }
        }
        Ok(submitted)
    }

    fn submit(&self, to_submit: u32) -> usize {
        let entries: Vec<squeue::Entry> = {
            let mut state = self.lock();
            let n = (to_submit as usize).min(state.sq.len());
            state.sq.drain(..n).collect()
        };
        let n = entries.len();
        for entry in entries {
            self.submit_one(entry);
        }
        let kernel = {
            let mut state = self.lock();
            if state.inflight.is_empty() {
                None
            } else {
                state.kernel.take()
            }
        };
        if let Some(kernel) = kernel {
            kernel.wake();
        }
        n
    }

    fn submit_one(&self, entry: squeue::Entry) {
        let user_data = entry.user_data;
        if entry.flags.intersects(Flags::UNSUPPORTED) {
            self.post(user_data, -libc::EINVAL, false);
            return;
        }
        let op = match entry.op {
            SqOp::Nop => Op::Nop,
            SqOp::Read { fd, buf, len, offset } => match self.resolve(fd) {
                Ok(fd) => Op::Read { fd, buf, len, offset },
                Err(errno) => return self.post(user_data, -errno, false),
            },
            SqOp::Write { fd, buf, len, offset } => match self.resolve(fd) {
                Ok(fd) => Op::Write { fd, buf, len, offset },
                Err(errno) => return self.post(user_data, -errno, false),
            },
            SqOp::Readv { fd, iovec, len, offset } => {
                match self.resolve(fd).and_then(|fd| Ok((fd, import_iovecs(iovec, len)?))) {
                    Ok((fd, iovecs)) => Op::Readv { fd, iovecs, offset },
                    Err(errno) => return self.post(user_data, -errno, false),
                }
            }
            SqOp::Writev { fd, iovec, len, offset } => {
                match self.resolve(fd).and_then(|fd| Ok((fd, import_iovecs(iovec, len)?))) {
                    Ok((fd, iovecs)) => Op::Writev { fd, iovecs, offset },
                    Err(errno) => return self.post(user_data, -errno, false),
                }
            }
            SqOp::Fsync { fd, datasync } => match self.resolve(fd) {
                Ok(fd) => Op::Fsync { fd, datasync },
                Err(errno) => return self.post(user_data, -errno, false),
            },
            SqOp::Fallocate { fd, offset, len, mode } => match self.resolve(fd) {
                Ok(fd) => Op::Fallocate { fd, offset, len, mode },
                Err(errno) => return self.post(user_data, -errno, false),
            },
            SqOp::Timeout { count } => Op::Timeout {
                count,
                since: self.lock().posted,
            },
            SqOp::AsyncCancel { user_data: target } => return self.cancel(user_data, target),
        };
        let checks = self.host.lock().checks;
        if checks.buffers {
            self.check_buffers(user_data, &op);
        }
        let file_range = if checks.file_ranges {
            self.check_file_range(user_data, &op)
        } else {
            None
        };
        let refusable = matches!(
            op,
            Op::Read { .. } | Op::Write { .. } | Op::Readv { .. } | Op::Writev { .. }
        ) && !entry.flags.contains(Flags::ASYNC);
        let inline = !matches!(op, Op::Timeout { .. })
            && !entry.flags.contains(Flags::ASYNC)
            && thread_rng().gen_bool(INLINE_COMPLETION);
        if inline {
            let op = InFlight {
                user_data,
                op,
                refusable,
                guard: None,
                file_range: None,
            };
            let result = op.complete(&self.host);
            self.post(user_data, result, false);
        } else {
            let guard = checks.buffers.then(|| BufferGuard::take(&op));
            self.lock().inflight.push(InFlight {
                user_data,
                op,
                refusable,
                guard,
                file_range,
            });
        }
    }

    /// Panic if the memory of `op`, which is being submitted, overlaps that of an op in flight where
    /// either op's kernel writes it (see [`crate::checks`]).
    fn check_buffers(&self, user_data: u64, op: &Op) {
        let memory = op.memory();
        if memory.is_empty() {
            return;
        }
        let conflict = {
            let state = self.lock();
            state.inflight.iter().find_map(|other| {
                other.op.memory().into_iter().find_map(|theirs| {
                    memory
                        .iter()
                        .find(|ours| ours.overlaps(&theirs) && (ours.kernel_writes || theirs.kernel_writes))
                        .map(|ours| (*ours, theirs, other.user_data, other.op.name()))
                })
            })
        };
        if let Some((ours, theirs, other, other_name)) = conflict {
            panic!(
                "shuttle-io-uring: the buffer of the {} with user_data {user_data:#x} ({:#x}..{:#x}) overlaps the \
                 buffer of the {other_name} with user_data {other:#x} ({:#x}..{:#x}), which is in flight: the kernel \
                 would write memory that the other op reads or writes.",
                op.name(),
                ours.start,
                ours.start + ours.len,
                theirs.start,
                theirs.start + theirs.len,
            );
        }
    }

    /// Panic if `op`, which is being submitted, covers a range of its file that an op in flight
    /// covers too, where either writes it (see [`crate::checks`]); return the op's range.
    fn check_file_range(&self, user_data: u64, op: &Op) -> Option<FileRange> {
        let ours = op.file_range()?;
        let conflict = {
            let state = self.lock();
            state.inflight.iter().find_map(|other| {
                let theirs = other.file_range?;
                let same_file = theirs.dev == ours.dev && theirs.ino == ours.ino;
                let overlap = ours.start < theirs.end && theirs.start < ours.end;
                (same_file && overlap && (ours.write || theirs.write)).then_some((
                    theirs,
                    other.user_data,
                    other.op.name(),
                ))
            })
        };
        if let Some((theirs, other, other_name)) = conflict {
            panic!(
                "shuttle-io-uring: the {} with user_data {user_data:#x} covers bytes {}..{} of the file with inode \
                 {} on device {:#x}, and the {other_name} with user_data {other:#x}, which is in flight, covers bytes \
                 {}..{} of it: what the file holds, or what the read returns, depends on which op completes first, \
                 which the kernel does not promise.",
                op.name(),
                ours.start,
                ours.end,
                ours.ino,
                ours.dev,
                theirs.start,
                theirs.end,
            );
        }
        Some(ours)
    }

    /// The fd an SQE's target names: the kernel looks a fixed file up when the op is submitted.
    fn resolve(&self, target: Target) -> Result<RawFd, i32> {
        match target {
            Target::Fd(fd) => Ok(fd),
            Target::Fixed(idx) => match &self.lock().files {
                Some(files) => match files.get(idx as usize) {
                    Some(&fd) if fd >= 0 => Ok(fd),
                    _ => Err(libc::EBADF),
                },
                None => Err(libc::EBADF),
            },
        }
    }

    /// `IORING_OP_ASYNC_CANCEL`. An op the kernel already handed to the device may be past
    /// cancelling, so a cancel of an in-flight op either cancels it or reports `-EALREADY` and lets
    /// it complete; Shuttle chooses. A cancel that finds nothing reports `-ENOENT`.
    fn cancel(&self, cancel_user_data: u64, target: u64) {
        let cancelled = {
            let mut state = self.lock();
            match state.inflight.iter().position(|op| op.user_data == target) {
                Some(i) if thread_rng().gen_bool(0.5) => Some(Some(state.inflight.swap_remove(i))),
                Some(_) => Some(None),
                None => None,
            }
        };
        match cancelled {
            Some(Some(op)) => {
                // The kernel never started the op, so its buffers are as they were.
                if let Some(guard) = &op.guard {
                    guard.release(target);
                }
                self.post(target, -libc::ECANCELED, false);
                self.post(cancel_user_data, 0, false);
            }
            Some(None) => self.post(cancel_user_data, -libc::EALREADY, false),
            None => self.post(cancel_user_data, -libc::ENOENT, false),
        }
    }

    /// Block the calling task until `min_complete` completions are visible.
    ///
    /// A bounded wait (`timeout`) also ends when the kernel task fires its timer, and at once if
    /// nothing is in flight to post a completion. It then succeeds if a completion is visible,
    /// however few, as the kernel's does, and fails with `ETIME` otherwise.
    fn wait(&self, min_complete: u32, timeout: Option<Timespec>) -> io::Result<()> {
        let mut timer: Option<u64> = None;
        future::block_on(poll_fn(|cx| {
            let mut state = self.lock();
            let visible = state.published.wrapping_sub(state.cq_head);
            if visible >= min_complete {
                state.end_wait(timer, cx.waker());
                return Poll::Ready(Ok(()));
            }
            if timeout.is_some() {
                let fired = timer.is_some_and(|id| state.wait_timers.iter().any(|t| t.id == id && t.fired));
                if fired || !state.in_flight() {
                    state.end_wait(timer, cx.waker());
                    return Poll::Ready(if visible > 0 {
                        Ok(())
                    } else {
                        Err(io::Error::from_raw_os_error(libc::ETIME))
                    });
                }
                match timer.and_then(|id| state.wait_timers.iter_mut().find(|t| t.id == id)) {
                    Some(timer) => timer.waker = cx.waker().clone(),
                    None => {
                        let id = state.next_wait_timer;
                        state.next_wait_timer += 1;
                        state.wait_timers.push(WaitTimer {
                            id,
                            waker: cx.waker().clone(),
                            fired: false,
                        });
                        timer = Some(id);
                    }
                }
            }
            if !state.waiters.iter().any(|w| w.will_wake(cx.waker())) {
                state.waiters.push(cx.waker().clone());
            }
            Poll::Pending
        }))
    }

    // ---- registration ----

    pub(crate) fn register_files(&self, fds: Vec<RawFd>) -> io::Result<()> {
        let mut state = self.lock();
        if state.files.is_some() {
            return Err(io::Error::from_raw_os_error(libc::EBUSY));
        }
        state.files = Some(fds);
        Ok(())
    }

    pub(crate) fn register_files_update(&self, offset: u32, fds: &[RawFd]) -> io::Result<usize> {
        let mut state = self.lock();
        let Some(files) = state.files.as_mut() else {
            return Err(io::Error::from_raw_os_error(libc::ENXIO));
        };
        let start = offset as usize;
        let Some(slots) = files.get_mut(start..start + fds.len()) else {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        };
        slots.copy_from_slice(fds);
        Ok(fds.len())
    }

    pub(crate) fn unregister_files(&self) -> io::Result<()> {
        match self.lock().files.take() {
            Some(_) => Ok(()),
            None => Err(io::Error::from_raw_os_error(libc::ENXIO)),
        }
    }

    /// Register `fd` as the ring's eventfd, keeping a reference of the ring's own to it, as the kernel
    /// does. An fd that is not open fails with `EBADF`, and one that is not an eventfd with `EINVAL`.
    pub(crate) fn register_eventfd(&self, fd: RawFd, async_only: bool) -> io::Result<()> {
        if self.lock().eventfd.is_some() {
            return Err(io::Error::from_raw_os_error(libc::EBUSY));
        }
        // SAFETY: `fcntl` only reads `fd`; the fd it returns is owned by nothing else.
        let own = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
        if own < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `own` was just created.
        let own = unsafe { OwnedFd::from_raw_fd(own) };
        if !maybe_eventfd(own.as_raw_fd()) {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        let readiness = Arc::new(Readiness::new());
        {
            let mut state = self.lock();
            if state.eventfd.is_some() {
                return Err(io::Error::from_raw_os_error(libc::EBUSY));
            }
            state.eventfd = Some(EventFd {
                fd,
                own: Arc::new(own),
                readiness: Arc::clone(&readiness),
                async_only,
            });
        }
        // Outside the ring's lock: nothing that can switch tasks runs under it.
        self.host.register(fd, readiness);
        Ok(())
    }

    pub(crate) fn unregister_eventfd(&self) -> io::Result<()> {
        let eventfd = self.lock().eventfd.take();
        match eventfd {
            Some(eventfd) => {
                self.host.unregister(eventfd.fd, &eventfd.readiness);
                Ok(())
            }
            None => Err(io::Error::from_raw_os_error(libc::ENXIO)),
        }
    }

    /// `IORING_REGISTER_ENABLE_RINGS`.
    pub(crate) fn enable(&self) -> io::Result<()> {
        let mut state = self.lock();
        if !state.disabled {
            return Err(io::Error::from_raw_os_error(libc::EBADFD));
        }
        state.disabled = false;
        Ok(())
    }

    // ---- close ----

    /// Close the ring, as dropping the last `IoUring` does.
    ///
    /// The kernel keeps working on ops in flight when a ring closes, reading and writing their
    /// buffers until they finish, so dropping a ring with ops in flight is only sound if their
    /// buffers outlive them. That is a teardown bug often enough that the simulation panics on it,
    /// unless the thread is already panicking or a timer has fired with I/O outstanding: an owner
    /// that saw the device hang may give up on the ring and leak the buffers, as a driver may once its
    /// drain timer expires. Either way it drops the ops rather than run them against buffers that may
    /// be gone, and leaves a read's buffer filled with what [`crate::checks`] put there.
    pub(crate) fn close(&self) {
        let (inflight, hung, kernel, eventfd) = {
            let mut state = self.lock();
            state.closed = true;
            let inflight = mem::take(&mut state.inflight);
            state.wait_timers.clear();
            (inflight.len(), state.hung, state.kernel.take(), state.eventfd.take())
        };
        if let Some(kernel) = kernel {
            kernel.wake();
        }
        self.host.unregister(self.fd, &self.readiness);
        if let Some(eventfd) = eventfd {
            self.host.unregister(eventfd.fd, &eventfd.readiness);
        }
        if inflight > 0 && !hung && !thread::panicking() {
            panic!(
                "shuttle-io-uring: ring {} dropped with {inflight} op(s) in flight; the kernel would keep reading \
                 and writing their buffers after the ring is gone",
                self.fd
            );
        }
    }
}

impl State {
    /// Post one completion into the CQ, or hold it back if the CQ is full. Returns how many entries
    /// reached the CQ.
    fn push_cqe(&mut self, entry: cqueue::Entry, cq_entries: u32) -> u32 {
        self.posted += 1;
        if self.overflow.is_empty() && (self.cq.len() as u32) < cq_entries {
            self.cq.push_back(entry);
            self.unpublished += 1;
            1
        } else {
            self.overflow.push_back(entry);
            0
        }
    }

    /// Whether a completion is still to come: an op is in flight, or a completion is posted whose
    /// tail store has not happened yet. The kernel task stores the tail after a scheduling point, so
    /// a waiter that counted only the ops would time out at once, again and again, for a completion
    /// that has all but posted, and a waiter that runs ahead of the kernel task, as one PCT favours
    /// does, would never let the store happen.
    fn in_flight(&self) -> bool {
        !self.inflight.is_empty() || self.unpublished > 0
    }

    /// Whether an op other than a timer is in flight.
    fn io_in_flight(&self) -> bool {
        self.inflight.iter().any(|op| !matches!(op.op, Op::Timeout { .. }))
    }

    /// Forget the timer of a wait that is ending, and its waker among the waiters.
    fn end_wait(&mut self, timer: Option<u64>, waker: &Waker) {
        if let Some(id) = timer {
            self.wait_timers.retain(|t| t.id != id);
        }
        self.waiters.retain(|w| !w.will_wake(waker));
    }

    /// The op to complete or the timer to fire next, chosen by Shuttle: any of them when they are all
    /// I/O or all timers, else I/O, but for one time in [`HANG_ODDS`] a timer. Timers are the timeout
    /// SQEs in flight and the timers of bounded waits that have not fired. Without bounded waits the
    /// draws are those of the ops alone.
    fn pick_next(&self) -> Pick {
        let waits: Vec<usize> = (0..self.wait_timers.len())
            .filter(|&i| !self.wait_timers[i].fired)
            .collect();
        let is_timer = |op: &InFlight| matches!(op.op, Op::Timeout { .. });
        let n = self.inflight.len() + waits.len();
        let timers = self.inflight.iter().filter(|op| is_timer(op)).count() + waits.len();
        // The candidates in a fixed order, the ops first, each with whether it is a timer.
        let candidates = self
            .inflight
            .iter()
            .enumerate()
            .map(|(i, op)| (Pick::Op(i), is_timer(op)))
            .chain(waits.iter().map(|&i| (Pick::WaitTimer(i), true)));
        if n == 1 {
            return candidates.map(|(pick, _)| pick).next().expect("one candidate");
        }
        let mut rng = thread_rng();
        if timers == 0 || timers == n {
            let k = rng.gen_range(0..n);
            return candidates
                .map(|(pick, _)| pick)
                .nth(k)
                .expect("k counts the candidates");
        }
        let want_timer = rng.gen_ratio(1, HANG_ODDS);
        let k = rng.gen_range(0..if want_timer { timers } else { n - timers });
        candidates
            .filter(|&(_, timer)| timer == want_timer)
            .map(|(pick, _)| pick)
            .nth(k)
            .expect("k counts candidates of the chosen kind")
    }

    /// Complete every timeout whose completion count is now met, with `0`. Each counts toward the
    /// others, as the kernel's CQ sequence does.
    fn complete_counted_timeouts(&mut self, cq_entries: u32) -> u32 {
        let mut added = 0;
        loop {
            let posted = self.posted;
            let Some(i) = self.inflight.iter().position(|op| match op.op {
                Op::Timeout { count, since } => count > 0 && posted - since >= u64::from(count),
                _ => false,
            }) else {
                return added;
            };
            let op = self.inflight.swap_remove(i);
            added += self.push_cqe(
                cqueue::Entry {
                    user_data: op.user_data,
                    result: 0,
                    flags: 0,
                },
                cq_entries,
            );
        }
    }
}
