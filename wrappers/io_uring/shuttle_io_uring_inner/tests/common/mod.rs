// Each test binary that includes this module uses some of it.
#![allow(dead_code)]

use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use shuttle_io_uring_inner::{IoUring, cqueue, squeue};

pub fn file() -> File {
    tempfile::tempfile().expect("temp file")
}

/// An `iovec` naming `buf`, for a vectored write.
pub fn iov(buf: &[u8]) -> libc::iovec {
    libc::iovec {
        iov_base: buf.as_ptr() as *mut libc::c_void,
        iov_len: buf.len(),
    }
}

/// An `iovec` naming `buf`, for a vectored read.
pub fn iov_mut(buf: &mut [u8]) -> libc::iovec {
    libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    }
}

/// A nonblocking eventfd.
pub fn eventfd() -> OwnedFd {
    // SAFETY: plain syscall; the fd it returns is owned by nothing else.
    let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    assert!(fd >= 0, "eventfd: {}", std::io::Error::last_os_error());
    // SAFETY: `fd` was just created.
    unsafe { OwnedFd::from_raw_fd(fd) }
}

/// Read and reset an eventfd's counter: `None` if it is zero.
pub fn drain_eventfd(fd: RawFd) -> Option<u64> {
    let mut count = 0_u64;
    // SAFETY: reads 8 bytes into `count`.
    let n = unsafe { libc::read(fd, (&raw mut count).cast(), 8) };
    if n == 8 {
        Some(count)
    } else {
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EAGAIN),
            "an empty nonblocking eventfd reads EAGAIN"
        );
        None
    }
}

pub fn push(ring: &IoUring, entry: squeue::Entry) {
    // SAFETY: each test keeps its buffers alive until their completions are reaped, and pushes from
    // one task at a time.
    unsafe { ring.submission_shared().push(&entry) }.expect("SQ has room");
}

pub fn reap(ring: &IoUring) -> Vec<cqueue::Entry> {
    // SAFETY: each test reaps from one task at a time.
    unsafe { ring.completion_shared() }.collect()
}

/// Reap until `n` completions have been collected.
pub fn reap_n(ring: &IoUring, n: usize) -> Vec<cqueue::Entry> {
    let mut cqes = Vec::new();
    while cqes.len() < n {
        ring.submit_and_wait(1).expect("enter");
        cqes.extend(reap(ring));
    }
    cqes
}

/// The `(user_data, result)` pairs of `cqes`, sorted.
pub fn results(cqes: &[cqueue::Entry]) -> Vec<(u64, i32)> {
    let mut results: Vec<(u64, i32)> = cqes.iter().map(|c| (c.user_data(), c.result())).collect();
    results.sort();
    results
}

/// The fd of a file, as an op names it.
pub fn fd(file: &File) -> shuttle_io_uring_inner::types::Fd {
    shuttle_io_uring_inner::types::Fd(file.as_raw_fd())
}
