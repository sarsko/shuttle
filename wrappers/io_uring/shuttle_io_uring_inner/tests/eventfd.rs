//! A registered eventfd is signalled as the kernel signals it: its counter goes up by one for each
//! signalled completion, so a driver that reads it sees the count rather than `EAGAIN`.

#![cfg(target_os = "linux")]

mod common;

use std::os::fd::{AsRawFd, RawFd};

use common::{drain_eventfd, eventfd, file, push, reap, reap_n};
use shuttle::{check_random, future};
use shuttle_io_uring_inner::{AsyncFd, IoUring, opcode, squeue};

const ITERATIONS: usize = 200;

/// The eventfd's count once the kernel task has finished the posts it began. The kernel signals the
/// eventfd after it stores the CQ tail, so a consumer can reap a completion before its signal
/// arrives, here as on Linux: this lets the other tasks run until `expected` signals have arrived,
/// then a while longer, to see that no more do.
fn settled_count(fd: RawFd, expected: u64) -> u64 {
    let mut total = 0;
    for round in 0..1000 {
        total += drain_eventfd(fd).unwrap_or(0);
        if total >= expected && round >= 20 {
            break;
        }
        shuttle::thread::yield_now();
    }
    total
}

/// Every completion posted while `IORING_CQ_EVENTFD_DISABLED` is clear adds 1 to the counter,
/// whether it posted inline or from the kernel task; none posted while it is set does.
#[test]
fn the_counter_counts_each_signalled_completion() {
    check_random(
        || {
            let ring = IoUring::new(8).unwrap();
            let efd = eventfd();
            ring.submitter().register_eventfd(efd.as_raw_fd()).unwrap();
            for ud in 1..=3 {
                push(&ring, opcode::Nop::new().build().user_data(ud));
            }
            ring.submit().unwrap();
            reap_n(&ring, 3);
            assert_eq!(settled_count(efd.as_raw_fd(), 3), 3);

            // SAFETY: the only CQ handle.
            unsafe { ring.completion_shared() }.disable_eventfd();
            for ud in 4..=5 {
                push(&ring, opcode::Nop::new().build().user_data(ud));
            }
            ring.submit().unwrap();
            reap_n(&ring, 2);
            assert_eq!(settled_count(efd.as_raw_fd(), 0), 0);

            // SAFETY: the only CQ handle.
            unsafe { ring.completion_shared() }.enable_eventfd();
            push(&ring, opcode::Nop::new().build().user_data(6));
            ring.submit().unwrap();
            reap_n(&ring, 1);
            assert_eq!(settled_count(efd.as_raw_fd(), 1), 1);
        },
        ITERATIONS,
    );
}

/// A driver that waits for the eventfd to be readable and then reads it, as a reactor does, finds the
/// count of completions: the counter is written before the readiness is signalled.
#[test]
fn a_driver_that_reads_the_eventfd_finds_the_count() {
    check_random(
        || {
            let ring = IoUring::new(8).unwrap();
            let efd = eventfd();
            ring.submitter().register_eventfd(efd.as_raw_fd()).unwrap();
            let raw = efd.as_raw_fd();
            let async_fd = AsyncFd::new(efd).unwrap();
            let driver = future::spawn(async move {
                let mut guard = async_fd.readable().await.unwrap();
                let count = drain_eventfd(raw);
                guard.clear_ready();
                count
            });
            push(&ring, opcode::Nop::new().build().user_data(1));
            ring.submit().unwrap();
            let count = future::block_on(driver).unwrap();
            assert!(matches!(count, Some(n) if n >= 1), "the driver read {count:?}");
            reap_n(&ring, 1);
        },
        ITERATIONS,
    );
}

/// With `register_eventfd_async`, the kernel signals only the completions it posts asynchronously:
/// here, those of the kernel task, and not those an enter posts as it submits.
#[test]
fn an_async_eventfd_counts_only_the_kernel_tasks_completions() {
    check_random(
        || {
            let ring = IoUring::new(8).unwrap();
            let efd = eventfd();
            ring.submitter().register_eventfd_async(efd.as_raw_fd()).unwrap();
            // A linked SQE fails as the enter submits it.
            push(
                &ring,
                opcode::Nop::new().build().flags(squeue::Flags::IO_LINK).user_data(1),
            );
            ring.submit().unwrap();
            assert_eq!(reap(&ring)[0].result(), -libc::EINVAL);
            assert_eq!(settled_count(efd.as_raw_fd(), 0), 0);
            // An `IOSQE_ASYNC` op never completes inline.
            push(
                &ring,
                opcode::Nop::new().build().flags(squeue::Flags::ASYNC).user_data(2),
            );
            ring.submit().unwrap();
            reap_n(&ring, 1);
            assert_eq!(settled_count(efd.as_raw_fd(), 1), 1);
        },
        ITERATIONS,
    );
}

/// The kernel registers only an eventfd, and only one: anything else fails as it does.
#[test]
fn registering_checks_the_fd() {
    check_random(
        || {
            let ring = IoUring::new(8).unwrap();
            let regular = file();
            let err = ring.submitter().register_eventfd(regular.as_raw_fd()).unwrap_err();
            assert_eq!(err.raw_os_error(), Some(libc::EINVAL), "a regular file");
            let err = ring.submitter().register_eventfd(-1).unwrap_err();
            assert_eq!(err.raw_os_error(), Some(libc::EBADF), "no fd");
            let err = ring.submitter().unregister_eventfd().unwrap_err();
            assert_eq!(err.raw_os_error(), Some(libc::ENXIO), "nothing registered");
            let efd = eventfd();
            ring.submitter().register_eventfd(efd.as_raw_fd()).unwrap();
            let other = eventfd();
            let err = ring.submitter().register_eventfd(other.as_raw_fd()).unwrap_err();
            assert_eq!(err.raw_os_error(), Some(libc::EBUSY), "a second eventfd");
            ring.submitter().unregister_eventfd().unwrap();
            ring.submitter().register_eventfd(other.as_raw_fd()).unwrap();
        },
        10,
    );
}

/// The ring keeps a reference of its own to the eventfd, as the kernel does: the signals still reach
/// the eventfd after its fd is closed, and so never go to whatever file reuses the number.
#[test]
fn the_ring_holds_its_own_reference_to_the_eventfd() {
    check_random(
        || {
            let ring = IoUring::new(8).unwrap();
            let efd = eventfd();
            ring.submitter().register_eventfd(efd.as_raw_fd()).unwrap();
            // Keep a second reference to read the counter through, then close the registered fd.
            // SAFETY: plain syscall on an open fd.
            let reader = unsafe { libc::dup(efd.as_raw_fd()) };
            assert!(reader >= 0);
            drop(efd);

            push(&ring, opcode::Nop::new().build().user_data(1));
            ring.submit().unwrap();
            reap_n(&ring, 1);
            assert_eq!(settled_count(reader, 1), 1);
            // SAFETY: `reader` is ours.
            unsafe { libc::close(reader) };
        },
        ITERATIONS,
    );
}

/// The kernel's signal never blocks; at a saturated counter, neither does the simulation's.
#[test]
fn a_saturated_counter_does_not_block_the_post() {
    check_random(
        || {
            let ring = IoUring::new(8).unwrap();
            // A blocking eventfd, at the largest count it holds.
            // SAFETY: plain syscall; the fd it returns is ours.
            let raw = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC) };
            assert!(raw >= 0);
            let max = u64::MAX - 1;
            // SAFETY: writes 8 bytes from `max`.
            assert_eq!(unsafe { libc::write(raw, (&raw const max).cast(), 8) }, 8);
            ring.submitter().register_eventfd(raw).unwrap();
            push(&ring, opcode::Nop::new().build().user_data(1));
            ring.submit().unwrap();
            reap_n(&ring, 1);
            let mut count = 0_u64;
            // SAFETY: reads 8 bytes into `count`; the counter is not zero, so this does not block.
            assert_eq!(unsafe { libc::read(raw, (&raw mut count).cast(), 8) }, 8);
            assert_eq!(count, max);
            ring.submitter().unregister_eventfd().unwrap();
            // SAFETY: `raw` is ours.
            unsafe { libc::close(raw) };
        },
        20,
    );
}
