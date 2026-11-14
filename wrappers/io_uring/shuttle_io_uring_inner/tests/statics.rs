//! Everything the simulation keeps belongs to one execution, so no execution sees what an earlier
//! one did, and a ring in a `static` that Shuttle resets is a new ring in every execution.

#![cfg(target_os = "linux")]

mod common;

use std::os::fd::AsRawFd;
use std::sync::{Arc, Mutex};

use common::{eventfd, push, reap_n};
use shuttle::check_random;
use shuttle::sync::LazyLock;
use shuttle_io_uring_inner::checks::{self, Checks};
use shuttle_io_uring_inner::faults::{self, Errors, Faults, Posted};
use shuttle_io_uring_inner::{AsyncFd, IoUring, opcode};

const ITERATIONS: usize = 20;

/// Each execution starts with the default faults, errors and checks, and no errors counted, though
/// every earlier one turned them on and left them on.
#[test]
fn faults_and_checks_start_at_their_defaults_in_every_execution() {
    check_random(
        || {
            assert_eq!(faults::faults(), Faults::default());
            assert_eq!(faults::errors(), Errors::default());
            assert_eq!(checks::checks(), Checks::default());
            assert_eq!(faults::take_posted(), Posted::default());
            faults::set_faults(Faults { eagain: 1.0 });
            faults::set_errors(Errors {
                eio: 1.0,
                ..Errors::default()
            });
            checks::set_checks(Checks {
                buffers: false,
                file_ranges: true,
            });
            let file = common::file();
            let ring = IoUring::new(8).unwrap();
            let mut buf = [0_u8; 8];
            push(
                &ring,
                opcode::Read::new(common::fd(&file), buf.as_mut_ptr(), 8)
                    .build()
                    .flags(shuttle_io_uring_inner::squeue::Flags::ASYNC)
                    .user_data(1),
            );
            assert_eq!(reap_n(&ring, 1)[0].result(), -libc::EIO);
        },
        ITERATIONS,
    );
}

/// Ring fd numbers start over in every execution, so a ring's fd, and the kernel task's name, are
/// the same in each.
#[test]
fn ring_fds_start_over_in_every_execution() {
    let fds = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&fds);
    check_random(
        move || {
            let first = IoUring::new(8).unwrap();
            let second = IoUring::new(8).unwrap();
            seen.lock().unwrap().push((first.as_raw_fd(), second.as_raw_fd()));
        },
        ITERATIONS,
    );
    let fds = fds.lock().unwrap();
    assert_eq!(fds.len(), ITERATIONS);
    assert!(fds.iter().all(|&pair| pair == fds[0]), "{fds:?}");
    assert_eq!(fds[0].1, fds[0].0 + 1);
}

/// An `AsyncFd` waits only on what this execution's rings signal: an eventfd that an earlier
/// execution registered, and that a later one opens with the same number, is not one of them.
#[test]
fn an_async_fd_never_sees_an_earlier_executions_eventfd() {
    check_random(
        || {
            let efd = eventfd();
            let err = AsyncFd::new(efd.as_raw_fd()).map(|_| ()).unwrap_err();
            assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
            let ring = IoUring::new(8).unwrap();
            ring.submitter().register_eventfd(efd.as_raw_fd()).unwrap();
            AsyncFd::new(efd.as_raw_fd()).unwrap();
            // Left registered: the ring goes with the execution.
            std::mem::forget(ring);
        },
        ITERATIONS,
    );
}

shuttle::lazy_static! {
    static ref LAZY_STATIC_RING: IoUring = IoUring::new(8).unwrap();
}

static LAZY_LOCK_RING: LazyLock<IoUring> = LazyLock::new(|| IoUring::new(8).unwrap());

/// A ring in a `lazy_static!` or a `LazyLock` is built anew in every execution, with a kernel task of
/// that execution: one left from an earlier execution would have no kernel task, and its ops would
/// never complete.
#[test]
fn a_ring_in_a_reset_static_is_a_new_ring_in_every_execution() {
    let fds = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&fds);
    check_random(
        move || {
            for ring in [&*LAZY_STATIC_RING, &*LAZY_LOCK_RING] {
                push(ring, opcode::Nop::new().build().user_data(1));
                assert_eq!(reap_n(ring, 1)[0].user_data(), 1);
            }
            seen.lock()
                .unwrap()
                .push((LAZY_STATIC_RING.as_raw_fd(), LAZY_LOCK_RING.as_raw_fd()));
        },
        ITERATIONS,
    );
    let fds = fds.lock().unwrap();
    assert!(fds.iter().all(|&pair| pair == fds[0]), "{fds:?}");
}

/// Building a ring, like the rest of Shuttle, panics outside of a Shuttle test.
#[test]
#[should_panic(expected = "Are you accessing a Shuttle primitive outside of a Shuttle test?")]
fn building_a_ring_outside_of_shuttle_panics() {
    let _ = IoUring::new(8);
}

/// So do the settings of the faults and checks, which belong to an execution.
#[test]
#[should_panic(expected = "Are you accessing a Shuttle primitive outside of a Shuttle test?")]
fn setting_faults_outside_of_shuttle_panics() {
    faults::set_faults(Faults { eagain: 0.5 });
}
