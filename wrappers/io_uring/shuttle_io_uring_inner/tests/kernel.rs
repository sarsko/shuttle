//! The simulated kernel's semantics, each checked across Shuttle schedules.

#![cfg(target_os = "linux")]

use std::collections::HashSet;
use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::FileExt;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use shuttle::{check_random, future};
use shuttle_io_uring_inner::faults::{self, Errors, Faults};
use shuttle_io_uring_inner::types::{Fd, Fixed, SubmitArgs, Timespec};
use shuttle_io_uring_inner::{AsyncFd, IoUring, cqueue, opcode, squeue};

const ITERATIONS: usize = 300;

/// Enough iterations for a one-in-256 simulated hang to show up.
const HANG_ITERATIONS: usize = 5_000;

fn file() -> File {
    tempfile::tempfile().expect("temp file")
}

/// An `iovec` naming `buf`, for a vectored write.
fn iov(buf: &[u8]) -> libc::iovec {
    libc::iovec {
        iov_base: buf.as_ptr() as *mut libc::c_void,
        iov_len: buf.len(),
    }
}

/// An `iovec` naming `buf`, for a vectored read.
fn iov_mut(buf: &mut [u8]) -> libc::iovec {
    libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    }
}

fn eventfd() -> OwnedFd {
    // SAFETY: plain syscall; the fd it returns is owned by nothing else.
    let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    assert!(fd >= 0, "eventfd: {}", std::io::Error::last_os_error());
    // SAFETY: `fd` was just created.
    unsafe { OwnedFd::from_raw_fd(fd) }
}

fn push(ring: &IoUring, entry: squeue::Entry) {
    // SAFETY: each test keeps its buffers alive until their completions are
    // reaped, and pushes from one task at a time.
    unsafe { ring.submission_shared().push(&entry) }.expect("SQ has room");
}

fn reap(ring: &IoUring) -> Vec<cqueue::Entry> {
    // SAFETY: each test reaps from one task at a time.
    unsafe { ring.completion_shared() }.collect()
}

/// Reap until `n` completions have been collected.
fn reap_n(ring: &IoUring, n: usize) -> Vec<cqueue::Entry> {
    let mut cqes = Vec::new();
    while cqes.len() < n {
        ring.submit_and_wait(1).expect("enter");
        cqes.extend(reap(ring));
    }
    cqes
}

#[test]
fn write_then_read_round_trips() {
    check_random(
        || {
            let file = file();
            let fd = Fd(file.as_raw_fd());
            let ring = IoUring::new(8).unwrap();
            let data = b"hello from the simulated kernel".to_vec();
            push(
                &ring,
                opcode::Write::new(fd, data.as_ptr(), data.len() as u32)
                    .build()
                    .user_data(1),
            );
            let cqes = reap_n(&ring, 1);
            assert_eq!((cqes[0].user_data(), cqes[0].result()), (1, data.len() as i32));

            let mut buf = vec![0u8; data.len()];
            push(
                &ring,
                opcode::Read::new(fd, buf.as_mut_ptr(), buf.len() as u32)
                    .build()
                    .user_data(2),
            );
            let cqes = reap_n(&ring, 1);
            assert_eq!((cqes[0].user_data(), cqes[0].result()), (2, data.len() as i32));
            assert_eq!(buf, data);
        },
        ITERATIONS,
    );
}

#[test]
fn completions_arrive_in_any_order() {
    let orders = Arc::new(Mutex::new(HashSet::new()));
    let seen = Arc::clone(&orders);
    check_random(
        move || {
            let ring = IoUring::new(8).unwrap();
            for ud in 1..=3 {
                push(&ring, opcode::Nop::new().build().user_data(ud));
            }
            ring.submit().unwrap();
            let order: Vec<u64> = reap_n(&ring, 3).iter().map(|c| c.user_data()).collect();
            seen.lock().unwrap().insert(order);
        },
        ITERATIONS,
    );
    let orders = orders.lock().unwrap();
    assert_eq!(orders.len(), 6, "every order of three completions: {orders:?}");
}

#[test]
fn ops_complete_inline_or_stay_in_flight() {
    let outcomes = Arc::new(Mutex::new(HashSet::new()));
    let seen = Arc::clone(&outcomes);
    check_random(
        move || {
            let ring = IoUring::new(8).unwrap();
            push(&ring, opcode::Nop::new().build().user_data(1));
            ring.submit().unwrap();
            // Visible straight after the submit if it completed inline, or if
            // the kernel ran before the reap's tail load; in flight otherwise.
            let visible = reap(&ring).len();
            seen.lock().unwrap().insert(visible);
            if visible == 0 {
                reap_n(&ring, 1);
            }
        },
        ITERATIONS,
    );
    assert_eq!(*outcomes.lock().unwrap(), HashSet::from([0, 1]));
}

#[test]
fn eventfd_wakes_a_waiter_on_completion() {
    check_random(
        || {
            let ring = IoUring::new(8).unwrap();
            let efd = eventfd();
            ring.submitter().register_eventfd(efd.as_raw_fd()).unwrap();
            let async_fd = AsyncFd::new(efd).unwrap();
            let waiter = future::spawn(async move {
                let mut guard = async_fd.readable().await.unwrap();
                guard.clear_ready();
            });
            push(&ring, opcode::Nop::new().build().user_data(1));
            ring.submit().unwrap();
            future::block_on(waiter).unwrap();
            // The signal follows the post, so the completion is visible.
            assert_eq!(reap(&ring).len(), 1);
        },
        ITERATIONS,
    );
}

#[test]
fn eventfd_disabled_posts_without_signalling() {
    check_random(
        || {
            let ring = IoUring::new(8).unwrap();
            let efd = eventfd();
            ring.submitter().register_eventfd(efd.as_raw_fd()).unwrap();
            let async_fd = AsyncFd::new(efd).unwrap();
            // SAFETY: the only CQ handle.
            unsafe { ring.completion_shared() }.disable_eventfd();
            push(&ring, opcode::Nop::new().build().user_data(1));
            ring.submit_and_wait(1).unwrap();
            let mut cx = Context::from_waker(Waker::noop());
            assert!(async_fd.poll_read_ready(&mut cx).is_pending());
            assert_eq!(reap(&ring).len(), 1);
        },
        ITERATIONS,
    );
}

#[test]
fn clear_ready_keeps_a_newer_event() {
    check_random(
        || {
            let ring = IoUring::new(8).unwrap();
            let efd = eventfd();
            ring.submitter().register_eventfd(efd.as_raw_fd()).unwrap();
            let async_fd = AsyncFd::new(efd).unwrap();
            push(&ring, opcode::Nop::new().build().user_data(1));
            ring.submit().unwrap();
            let mut guard = future::block_on(async_fd.readable()).unwrap();
            push(&ring, opcode::Nop::new().build().user_data(2));
            ring.submit().unwrap();
            reap_n(&ring, 2);
            // Whether the second signal came before or after this clear, the
            // fd must read ready again: a clear never erases a newer event.
            guard.clear_ready();
            // The guard holds only a shared borrow of the `AsyncFd`, so it need not end here.
            future::block_on(async_fd.readable()).unwrap();
        },
        ITERATIONS,
    );
}

#[test]
fn fixed_files_resolve_through_the_table() {
    check_random(
        || {
            let file = file();
            let ring = IoUring::new(8).unwrap();
            let submitter = ring.submitter();
            submitter.register_files_sparse(2).unwrap();
            assert_eq!(submitter.register_files_update(0, &[file.as_raw_fd()]).unwrap(), 1);
            let data = [7u8; 16];
            for (ud, idx) in [(1, 0), (2, 1), (3, 5)] {
                push(
                    &ring,
                    opcode::Write::new(Fixed(idx), data.as_ptr(), data.len() as u32)
                        .build()
                        .user_data(ud),
                );
            }
            ring.submit().unwrap();
            let mut results: Vec<(u64, i32)> = reap_n(&ring, 3).iter().map(|c| (c.user_data(), c.result())).collect();
            results.sort();
            assert_eq!(results, vec![(1, 16), (2, -libc::EBADF), (3, -libc::EBADF)]);
        },
        ITERATIONS,
    );
}

#[test]
fn unsynced_entries_are_not_submitted() {
    check_random(
        || {
            let ring = IoUring::new(8).unwrap();
            // SAFETY: the only SQ handle.
            let mut sq = unsafe { ring.submission_shared() };
            // SAFETY: a nop has no buffers.
            unsafe { sq.push(&opcode::Nop::new().build().user_data(1)) }.unwrap();
            assert_eq!(ring.submit().unwrap(), 0, "pushed but not synced");
            sq.sync();
            assert_eq!(ring.submit().unwrap(), 1);
            drop(sq);
            reap_n(&ring, 1);
        },
        ITERATIONS,
    );
}

#[test]
fn bounded_wait_with_nothing_in_flight_times_out() {
    check_random(
        || {
            let ring = IoUring::new(8).unwrap();
            let timeout = Timespec::from(Duration::from_millis(1));
            let args = SubmitArgs::new().timespec(&timeout);
            let err = ring.submitter().submit_with_args(1, &args).unwrap_err();
            assert_eq!(err.raw_os_error(), Some(libc::ETIME));
        },
        ITERATIONS,
    );
}

#[test]
#[should_panic(expected = "deadlock")]
fn unbounded_wait_with_nothing_in_flight_deadlocks() {
    check_random(
        || {
            let ring = IoUring::new(8).unwrap();
            let _ = ring.submit_and_wait(1);
        },
        ITERATIONS,
    );
}

#[test]
fn timeout_fires_or_meets_its_count() {
    let outcomes = Arc::new(Mutex::new(HashSet::new()));
    let seen = Arc::clone(&outcomes);
    check_random(
        move || {
            let ring = IoUring::new(8).unwrap();
            let timeout = Timespec::from(Duration::from_secs(1));
            push(
                &ring,
                opcode::Timeout::new(&raw const timeout).count(2).build().user_data(9),
            );
            push(&ring, opcode::Nop::new().build().user_data(1));
            push(&ring, opcode::Nop::new().build().user_data(2));
            ring.submit().unwrap();
            let cqes = reap_n(&ring, 3);
            let timer = cqes.iter().find(|c| c.user_data() == 9).unwrap().result();
            assert!(timer == 0 || timer == -libc::ETIME, "timeout result {timer}");
            seen.lock().unwrap().insert(timer);
        },
        // The timer fires with I/O in flight one time in 256 per completion.
        HANG_ITERATIONS,
    );
    assert_eq!(*outcomes.lock().unwrap(), HashSet::from([0, -libc::ETIME]));
}

#[test]
fn cancel_cancels_or_reports_already() {
    let outcomes = Arc::new(Mutex::new(HashSet::new()));
    let seen = Arc::clone(&outcomes);
    check_random(
        move || {
            let ring = IoUring::new(8).unwrap();
            let timeout = Timespec::from(Duration::from_secs(1));
            push(&ring, opcode::Timeout::new(&raw const timeout).build().user_data(1));
            ring.submit().unwrap();
            // Let the timer fire first in some schedules.
            future::block_on(future::yield_now());
            push(&ring, opcode::AsyncCancel::new(1).build().user_data(2));
            ring.submit().unwrap();
            let mut results: Vec<(u64, i32)> = reap_n(&ring, 2).iter().map(|c| (c.user_data(), c.result())).collect();
            results.sort();
            let cancel = results[1].1;
            let target = results[0].1;
            match cancel {
                0 => assert_eq!(target, -libc::ECANCELED),
                c if c == -libc::EALREADY || c == -libc::ENOENT => {
                    assert_eq!(target, -libc::ETIME)
                }
                other => panic!("cancel result {other}"),
            }
            seen.lock().unwrap().insert(cancel);
        },
        ITERATIONS,
    );
    assert_eq!(
        *outcomes.lock().unwrap(),
        HashSet::from([0, -libc::EALREADY, -libc::ENOENT])
    );
}

#[test]
#[should_panic(expected = "in flight")]
fn dropping_a_ring_with_ops_in_flight_panics() {
    check_random(
        || {
            let ring = IoUring::new(8).unwrap();
            let timeout = Timespec::from(Duration::from_secs(1));
            push(&ring, opcode::Timeout::new(&raw const timeout).build().user_data(1));
            ring.submit().unwrap();
            drop(ring);
        },
        ITERATIONS,
    );
}

#[test]
fn a_ring_that_saw_a_hang_may_drop_ops_in_flight() {
    let hangs = Arc::new(Mutex::new(0usize));
    let seen = Arc::clone(&hangs);
    check_random(
        move || {
            let file = file();
            let ring = IoUring::new(8).unwrap();
            let mut buf = vec![0u8; 512];
            let timeout = Timespec::from(Duration::from_secs(60));
            push(&ring, opcode::Timeout::new(&raw const timeout).build().user_data(9));
            push(
                &ring,
                opcode::Read::new(Fd(file.as_raw_fd()), buf.as_mut_ptr(), buf.len() as u32)
                    .build()
                    .user_data(1),
            );
            ring.submit().unwrap();
            let cqes = reap_n(&ring, 1);
            let read_done = cqes.iter().any(|c| c.user_data() == 1);
            if cqes[0].user_data() == 9 && !read_done {
                // The timer outlasted the read: the device hung, and the
                // owner may give up on the ring with the read in flight.
                assert_eq!(cqes[0].result(), -libc::ETIME);
                *seen.lock().unwrap() += 1;
                drop(ring);
            } else if cqes.len() < 2 {
                reap_n(&ring, 1);
            }
        },
        HANG_ITERATIONS,
    );
    let hangs = *hangs.lock().unwrap();
    assert!(
        hangs > 0 && hangs < HANG_ITERATIONS / 20,
        "{hangs} of {HANG_ITERATIONS} iterations saw the timer outlast the read"
    );
}

#[test]
fn async_fd_rejects_an_unregistered_fd() {
    check_random(
        || {
            let efd = eventfd();
            let err = AsyncFd::new(efd).map(|_| ()).unwrap_err();
            assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        },
        10,
    );
}

#[test]
fn ring_fd_is_readable_after_a_completion() {
    check_random(
        || {
            let ring = Arc::new(IoUring::new(8).unwrap());
            struct RingFd(Arc<IoUring>);
            impl AsRawFd for RingFd {
                fn as_raw_fd(&self) -> std::os::fd::RawFd {
                    self.0.as_raw_fd()
                }
            }
            let async_fd = AsyncFd::new(RingFd(Arc::clone(&ring))).unwrap();
            let mut cx = Context::from_waker(Waker::noop());
            assert!(matches!(async_fd.poll_read_ready(&mut cx), Poll::Pending));
            push(&ring, opcode::Nop::new().build().user_data(1));
            ring.submit().unwrap();
            future::block_on(async_fd.readable()).unwrap();
            assert_eq!(reap(&ring).len(), 1);
        },
        ITERATIONS,
    );
}

/// With every refusable op refused, a read or write submitted without
/// `IOSQE_ASYNC` reports `-EAGAIN` having moved at most a block-aligned part
/// of its range, and one submitted with it completes.
#[test]
fn refused_ops_report_eagain_after_moving_a_part() {
    const BLOCK: usize = 4096;
    check_random(
        || {
            faults::set_faults(Faults { eagain: 1.0 });
            let file = file();
            let fd = Fd(file.as_raw_fd());
            let ring = IoUring::new(8).unwrap();
            let data = vec![0xab_u8; 3 * BLOCK];
            push(
                &ring,
                opcode::Write::new(fd, data.as_ptr(), data.len() as u32)
                    .build()
                    .user_data(1),
            );
            let cqes = reap_n(&ring, 1);
            assert_eq!(cqes[0].result(), -libc::EAGAIN);
            let moved = file.metadata().unwrap().len() as usize;
            assert!(moved.is_multiple_of(BLOCK) && moved < data.len(), "moved {moved}");

            push(
                &ring,
                opcode::Write::new(fd, data.as_ptr(), data.len() as u32)
                    .build()
                    .flags(squeue::Flags::ASYNC)
                    .user_data(2),
            );
            let cqes = reap_n(&ring, 1);
            assert_eq!(cqes[0].result(), data.len() as i32);

            let mut buf = vec![0_u8; data.len()];
            push(
                &ring,
                opcode::Read::new(fd, buf.as_mut_ptr(), buf.len() as u32)
                    .build()
                    .user_data(3),
            );
            let cqes = reap_n(&ring, 1);
            assert_eq!(cqes[0].result(), -libc::EAGAIN);
            let filled = buf.iter().take_while(|&&b| b == 0xab).count();
            assert!(filled.is_multiple_of(BLOCK) && filled < buf.len(), "filled {filled}");
            assert!(buf[filled..].iter().all(|&b| b == 0), "only a prefix is filled");
            faults::set_faults(Faults::default());
        },
        ITERATIONS,
    );
}

/// With errors on, a read fails with `-EIO` or returns a block-aligned part
/// of its range short, and a `fallocate` fails with `-ENOSPC`; with them off
/// again the same ops complete in full.
#[test]
fn posted_errors_surface_and_stop_when_turned_off() {
    const BLOCK: usize = 4096;
    check_random(
        || {
            let file = file();
            let fd = Fd(file.as_raw_fd());
            let ring = IoUring::new(8).unwrap();
            let data = vec![0xab_u8; 3 * BLOCK];
            push(
                &ring,
                opcode::Write::new(fd, data.as_ptr(), data.len() as u32)
                    .build()
                    .flags(squeue::Flags::ASYNC)
                    .user_data(1),
            );
            assert_eq!(reap_n(&ring, 1)[0].result(), data.len() as i32);

            let read = |user_data| {
                let mut buf = vec![0_u8; data.len()];
                push(
                    &ring,
                    opcode::Read::new(fd, buf.as_mut_ptr(), buf.len() as u32)
                        .build()
                        .flags(squeue::Flags::ASYNC)
                        .user_data(user_data),
                );
                reap_n(&ring, 1)[0].result()
            };
            faults::set_errors(Errors {
                eio: 1.0,
                ..Errors::default()
            });
            assert_eq!(read(2), -libc::EIO);
            faults::set_errors(Errors {
                short_read: 1.0,
                ..Errors::default()
            });
            let short = read(3);
            assert!(
                short == data.len() as i32
                    || (short > 0 && (short as usize).is_multiple_of(BLOCK) && (short as usize) < data.len()),
                "a short read returned {short}"
            );
            faults::set_errors(Errors {
                enospc: 1.0,
                ..Errors::default()
            });
            push(
                &ring,
                opcode::Fallocate::new(fd, BLOCK as u64)
                    .offset(data.len() as u64)
                    .build()
                    .user_data(4),
            );
            assert_eq!(reap_n(&ring, 1)[0].result(), -libc::ENOSPC);
            let posted = faults::take_posted();
            assert_eq!((posted.eio, posted.enospc), (1, 1));

            faults::set_errors(Errors::default());
            assert_eq!(read(5), data.len() as i32);
        },
        ITERATIONS,
    );
}

/// Vectored ops gather from and scatter into their buffers in order, at their
/// offset.
#[test]
fn vectored_write_then_read_round_trips() {
    check_random(
        || {
            let file = file();
            let fd = Fd(file.as_raw_fd());
            let ring = IoUring::new(8).unwrap();
            let parts: [&[u8]; 3] = [b"hello ", b"from the ", b"simulated kernel"];
            let data = parts.concat();
            let writes: Vec<libc::iovec> = parts.iter().map(|part| iov(part)).collect();
            push(
                &ring,
                opcode::Writev::new(fd, writes.as_ptr(), writes.len() as u32)
                    .build()
                    .user_data(1),
            );
            let cqes = reap_n(&ring, 1);
            assert_eq!((cqes[0].user_data(), cqes[0].result()), (1, data.len() as i32));

            // Split the read elsewhere than the write, and start it past 0.
            let mut head = vec![0_u8; 4];
            let mut tail = vec![0_u8; data.len() - 2 - head.len()];
            let reads = [iov_mut(&mut head), iov_mut(&mut tail)];
            push(
                &ring,
                opcode::Readv::new(fd, reads.as_ptr(), reads.len() as u32)
                    .offset(2)
                    .build()
                    .user_data(2),
            );
            let cqes = reap_n(&ring, 1);
            assert_eq!((cqes[0].user_data(), cqes[0].result()), (2, data.len() as i32 - 2));
            assert_eq!([head, tail].concat(), data[2..]);
        },
        ITERATIONS,
    );
}

/// The kernel copies a vectored op's `iovec` array at submission, so the
/// submitter may reuse the array once the submit returns, with the op still in
/// flight.
#[test]
fn vectored_iovec_array_is_copied_at_submission() {
    check_random(
        || {
            let file = file();
            let fd = Fd(file.as_raw_fd());
            let ring = IoUring::new(8).unwrap();
            let data = vec![0xaa_u8; 64];
            let other = vec![0xbb_u8; 64];
            let mut writes = [iov(&data)];
            push(
                &ring,
                opcode::Writev::new(fd, writes.as_ptr(), writes.len() as u32)
                    .build()
                    .user_data(1),
            );
            ring.submit().unwrap();
            // A kernel that read the array at completion would write `other`.
            writes[0] = iov(&other);
            std::hint::black_box(&writes);
            assert_eq!(reap_n(&ring, 1)[0].result(), data.len() as i32);
            let mut written = vec![0_u8; data.len()];
            file.read_exact_at(&mut written, 0).unwrap();
            assert_eq!(written, data);
        },
        ITERATIONS,
    );
}

/// A vectored op with more than `UIO_MAXIOV` entries fails with `-EINVAL`, and
/// one with a null array with `-EFAULT`, as the kernel's import of the array
/// does; an empty array moves nothing.
#[test]
fn vectored_ops_check_their_iovec_arrays() {
    check_random(
        || {
            let file = file();
            let fd = Fd(file.as_raw_fd());
            let ring = IoUring::new(8).unwrap();
            let mut byte = [0_u8; 1];
            let too_many = vec![iov_mut(&mut byte); libc::UIO_MAXIOV as usize + 1];
            push(
                &ring,
                opcode::Readv::new(fd, too_many.as_ptr(), too_many.len() as u32)
                    .build()
                    .user_data(1),
            );
            push(&ring, opcode::Writev::new(fd, std::ptr::null(), 1).build().user_data(2));
            push(&ring, opcode::Writev::new(fd, std::ptr::null(), 0).build().user_data(3));
            ring.submit().unwrap();
            let mut results: Vec<(u64, i32)> = reap_n(&ring, 3).iter().map(|c| (c.user_data(), c.result())).collect();
            results.sort();
            assert_eq!(results, vec![(1, -libc::EINVAL), (2, -libc::EFAULT), (3, 0)]);
        },
        ITERATIONS,
    );
}

/// Vectored ops name fixed files through the registered table, as
/// single-buffer ones do.
#[test]
fn vectored_ops_resolve_fixed_files() {
    check_random(
        || {
            let file = file();
            let ring = IoUring::new(8).unwrap();
            let submitter = ring.submitter();
            submitter.register_files_sparse(2).unwrap();
            submitter.register_files_update(0, &[file.as_raw_fd()]).unwrap();
            let data = [7_u8; 16];
            let writes = [iov(&data)];
            for (ud, idx) in [(1, 0), (2, 1)] {
                push(
                    &ring,
                    opcode::Writev::new(Fixed(idx), writes.as_ptr(), writes.len() as u32)
                        .build()
                        .user_data(ud),
                );
            }
            ring.submit().unwrap();
            let mut results: Vec<(u64, i32)> = reap_n(&ring, 2).iter().map(|c| (c.user_data(), c.result())).collect();
            results.sort();
            assert_eq!(results, vec![(1, 16), (2, -libc::EBADF)]);

            let mut buf = [0_u8; 16];
            let reads = [iov_mut(&mut buf)];
            push(
                &ring,
                opcode::Readv::new(Fixed(0), reads.as_ptr(), reads.len() as u32)
                    .build()
                    .user_data(3),
            );
            assert_eq!(reap_n(&ring, 1)[0].result(), 16);
            assert_eq!(buf, data);
        },
        ITERATIONS,
    );
}

/// A refused vectored op, and a failed or short vectored read, moves a
/// block-aligned prefix of its range across the buffers it lists, as a
/// single-buffer op does.
#[test]
fn vectored_faults_move_a_block_aligned_prefix() {
    const BLOCK: usize = 4096;
    check_random(
        || {
            let file = file();
            let fd = Fd(file.as_raw_fd());
            let ring = IoUring::new(8).unwrap();
            // Buffer boundaries fall inside blocks, so a prefix splits a buffer.
            let bufs = [
                vec![0x11_u8; BLOCK + 512],
                vec![0x22_u8; BLOCK],
                vec![0x33_u8; 2 * BLOCK - 512],
            ];
            let data = bufs.concat();
            let writes: Vec<libc::iovec> = bufs.iter().map(|buf| iov(buf)).collect();
            let write = |user_data, flags| {
                push(
                    &ring,
                    opcode::Writev::new(fd, writes.as_ptr(), writes.len() as u32)
                        .build()
                        .flags(flags)
                        .user_data(user_data),
                );
                reap_n(&ring, 1)[0].result()
            };
            // Scatters into buffers split elsewhere than the write's.
            let read = |user_data, flags| {
                let mut head = vec![0_u8; 3 * BLOCK - 100];
                let mut tail = vec![0_u8; data.len() - head.len()];
                let reads = [iov_mut(&mut head), iov_mut(&mut tail)];
                push(
                    &ring,
                    opcode::Readv::new(fd, reads.as_ptr(), reads.len() as u32)
                        .build()
                        .flags(flags)
                        .user_data(user_data),
                );
                let result = reap_n(&ring, 1)[0].result();
                (result, [head, tail].concat())
            };
            // How much of `buf` holds a prefix of `data`; the rest is unread.
            let filled = |buf: &[u8]| {
                let n = buf.iter().zip(&data).take_while(|(b, d)| b == d).count();
                assert!(buf[n..].iter().all(|&b| b == 0), "only a prefix is filled");
                n
            };

            faults::set_faults(Faults { eagain: 1.0 });
            assert_eq!(write(1, squeue::Flags::empty()), -libc::EAGAIN);
            let len = file.metadata().unwrap().len() as usize;
            assert!(len.is_multiple_of(BLOCK) && len < data.len(), "moved {len}");
            let mut moved = vec![0_u8; len];
            file.read_exact_at(&mut moved, 0).unwrap();
            assert_eq!(moved, data[..len], "a refused write moves a prefix");
            assert_eq!(write(2, squeue::Flags::ASYNC), data.len() as i32);
            let (result, buf) = read(3, squeue::Flags::empty());
            assert_eq!(result, -libc::EAGAIN);
            let n = filled(&buf);
            assert!(n.is_multiple_of(BLOCK) && n < data.len(), "a refused read filled {n}");
            faults::set_faults(Faults::default());

            faults::set_errors(Errors {
                eio: 1.0,
                ..Errors::default()
            });
            let (result, buf) = read(4, squeue::Flags::ASYNC);
            assert_eq!(result, -libc::EIO);
            let n = filled(&buf);
            assert!(n.is_multiple_of(BLOCK) && n < data.len(), "a failed read filled {n}");
            faults::set_errors(Errors {
                short_read: 1.0,
                ..Errors::default()
            });
            let (short, buf) = read(5, squeue::Flags::ASYNC);
            let short = usize::try_from(short).expect("a short read succeeds");
            assert!(
                short == data.len() || (short > 0 && short.is_multiple_of(BLOCK) && short < data.len()),
                "a short read returned {short}"
            );
            assert_eq!(filled(&buf), short);
            faults::set_errors(Errors::default());
            assert_eq!(read(6, squeue::Flags::ASYNC), (data.len() as i32, data.clone()));
        },
        ITERATIONS,
    );
}
