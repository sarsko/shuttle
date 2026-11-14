//! Bounded waits (`submit_with_args` with a timeout) are timers the kernel task fires as it fires
//! timeout SQEs, and an enter returns what the kernel's `io_uring_enter` returns.

#![cfg(target_os = "linux")]

mod common;

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use common::{fd, file, push, reap, reap_n};
use shuttle::check_random;
use shuttle_io_uring_inner::types::{SubmitArgs, Timespec};
use shuttle_io_uring_inner::{IoUring, opcode, squeue};

/// Enough iterations for a one-in-256 timer to show up.
const ITERATIONS: usize = 5_000;

fn timeout() -> Timespec {
    Timespec::new().nsec(100_000_000)
}

fn async_read(ring: &IoUring, file: &std::fs::File, buf: &mut [u8], user_data: u64) {
    push(
        ring,
        opcode::Read::new(fd(file), buf.as_mut_ptr(), buf.len() as u32)
            .build()
            .flags(squeue::Flags::ASYNC)
            .user_data(user_data),
    );
}

/// With I/O in flight, a bounded wait times out only as often as a timeout SQE fires, one time in
/// 256, rather than at every poll.
#[test]
fn a_bounded_wait_with_io_in_flight_rarely_times_out() {
    let timeouts = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&timeouts);
    check_random(
        move || {
            let file = file();
            let ring = IoUring::new(8).unwrap();
            let mut buf = vec![0_u8; 512];
            async_read(&ring, &file, &mut buf, 1);
            assert_eq!(ring.submit().unwrap(), 1);
            let ts = timeout();
            let args = SubmitArgs::new().timespec(&ts);
            match ring.submitter().submit_with_args(1, &args) {
                // Nothing to submit, and the read completed.
                Ok(0) => assert_eq!(reap(&ring).len(), 1),
                Err(e) if e.raw_os_error() == Some(libc::ETIME) => {
                    seen.fetch_add(1, Ordering::Relaxed);
                    reap_n(&ring, 1);
                }
                other => panic!("bounded wait returned {other:?}"),
            }
        },
        ITERATIONS,
    );
    let timeouts = timeouts.load(Ordering::Relaxed);
    assert!(
        timeouts > 0 && timeouts < ITERATIONS / 20,
        "{timeouts} of {ITERATIONS} bounded waits timed out with a read in flight"
    );
}

/// A bounded wait that ends without its count met succeeds if a completion is there to reap, as
/// the kernel's `io_cqring_wait` does; only one that finds nothing fails with `ETIME`.
#[test]
fn a_bounded_wait_that_ends_with_completions_to_reap_succeeds() {
    let outcomes = Arc::new(Mutex::new(HashSet::new()));
    let seen = Arc::clone(&outcomes);
    check_random(
        move || {
            let ring = IoUring::new(8).unwrap();
            push(
                &ring,
                opcode::Nop::new().build().flags(squeue::Flags::ASYNC).user_data(1),
            );
            ring.submit().unwrap();
            let ts = timeout();
            let args = SubmitArgs::new().timespec(&ts);
            // Two completions are wanted, and only one op is in flight, so the wait never meets its
            // count: it ends when its timer fires, or when the nop completes and leaves nothing in
            // flight.
            let outcome = match ring.submitter().submit_with_args(2, &args) {
                Ok(0) => {
                    assert_eq!(reap(&ring).len(), 1, "a successful wait has a completion to reap");
                    "succeeded"
                }
                Err(e) if e.raw_os_error() == Some(libc::ETIME) => {
                    // The nop was in flight when the wait ended; the kernel task may post it before
                    // the tail load below.
                    if reap(&ring).is_empty() {
                        reap_n(&ring, 1);
                    }
                    "timed out"
                }
                other => panic!("bounded wait returned {other:?}"),
            };
            seen.lock().unwrap().insert(outcome);
        },
        ITERATIONS,
    );
    assert_eq!(*outcomes.lock().unwrap(), HashSet::from(["succeeded", "timed out"]));
}

/// An enter that submits entries returns how many it submitted, whatever became of its wait, as
/// `io_uring_enter` returns the wait's error only when it submitted nothing.
#[test]
fn an_enter_that_submits_returns_the_count_whatever_its_wait() {
    check_random(
        || {
            let ring = IoUring::new(8).unwrap();
            push(&ring, opcode::Nop::new().build().user_data(1));
            let ts = timeout();
            let args = SubmitArgs::new().timespec(&ts);
            // The wait for two completions times out, as one op cannot meet it.
            assert_eq!(ring.submitter().submit_with_args(2, &args).unwrap(), 1);
            reap_n(&ring, 1);
        },
        ITERATIONS / 10,
    );
}

/// A wait's timer that fires with I/O in flight is a hang, as a timeout SQE's is: the owner may then
/// drop the ring with the I/O in flight.
#[test]
fn a_bounded_wait_that_times_out_with_io_in_flight_is_a_hang() {
    let hangs = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&hangs);
    check_random(
        move || {
            let file = file();
            let ring = IoUring::new(8).unwrap();
            let mut buf = vec![0_u8; 512];
            async_read(&ring, &file, &mut buf, 1);
            ring.submit().unwrap();
            let ts = timeout();
            let args = SubmitArgs::new().timespec(&ts);
            if ring.submitter().submit_with_args(1, &args).is_err() {
                seen.fetch_add(1, Ordering::Relaxed);
                // Without the hang this panics, the read being in flight.
                drop(ring);
            } else {
                reap(&ring);
            }
        },
        ITERATIONS,
    );
    assert!(hangs.load(Ordering::Relaxed) > 0);
}

/// With only timers in flight, a bounded wait's timer and a timeout SQE each fire first in some
/// schedules: Shuttle has no clock to order them by.
#[test]
fn a_bounded_wait_with_only_timers_in_flight_fires_either_first() {
    let outcomes = Arc::new(Mutex::new(HashSet::new()));
    let seen = Arc::clone(&outcomes);
    check_random(
        move || {
            let ring = IoUring::new(8).unwrap();
            let sqe_timeout = Timespec::new().sec(60);
            push(&ring, opcode::Timeout::new(&raw const sqe_timeout).build().user_data(9));
            ring.submit().unwrap();
            let ts = timeout();
            let args = SubmitArgs::new().timespec(&ts);
            let first = match ring.submitter().submit_with_args(1, &args) {
                Ok(0) => {
                    let cqes = reap(&ring);
                    assert_eq!((cqes[0].user_data(), cqes[0].result()), (9, -libc::ETIME));
                    "timeout SQE"
                }
                Err(e) if e.raw_os_error() == Some(libc::ETIME) => {
                    reap_n(&ring, 1);
                    "wait"
                }
                other => panic!("bounded wait returned {other:?}"),
            };
            seen.lock().unwrap().insert(first);
        },
        200,
    );
    assert_eq!(*outcomes.lock().unwrap(), HashSet::from(["timeout SQE", "wait"]));
}

/// With nothing in flight, a bounded wait ends at once: with `ETIME` if nothing is there to reap, and
/// successfully if a completion is, even short of its count.
#[test]
fn a_bounded_wait_with_nothing_in_flight_ends_at_once() {
    check_random(
        || {
            let ring = IoUring::new(8).unwrap();
            let ts = timeout();
            let args = SubmitArgs::new().timespec(&ts);
            let err = ring.submitter().submit_with_args(1, &args).unwrap_err();
            assert_eq!(err.raw_os_error(), Some(libc::ETIME));

            push(&ring, opcode::Nop::new().build().user_data(1));
            assert_eq!(ring.submit_and_wait(1).unwrap(), 1);
            assert_eq!(ring.submitter().submit_with_args(2, &args).unwrap(), 0);
            assert_eq!(reap(&ring).len(), 1);
        },
        100,
    );
}

/// `submit_with_args_and_flags` takes `ABS_TIMER`, and fails `NO_IOWAIT` and any other flag with
/// `EINVAL`, as the real crate and a kernel without `IORING_FEAT_NO_IOWAIT` do.
#[test]
fn submit_with_args_and_flags_checks_its_flags() {
    use shuttle_io_uring_inner::EnterFlags;
    check_random(
        || {
            let ring = IoUring::new(8).unwrap();
            assert!(!ring.params().is_feature_no_iowait());
            let ts = timeout();
            let args = SubmitArgs::new().timespec(&ts);
            for flags in [EnterFlags::NO_IOWAIT, EnterFlags::GETEVENTS, EnterFlags::SQ_WAKEUP] {
                let err = ring
                    .submitter()
                    .submit_with_args_and_flags(1, &args, flags)
                    .unwrap_err();
                assert_eq!(err.raw_os_error(), Some(libc::EINVAL), "{flags:?}");
            }
            push(&ring, opcode::Nop::new().build().user_data(1));
            let submitted = ring
                .submitter()
                .submit_with_args_and_flags(1, &args, EnterFlags::ABS_TIMER)
                .unwrap();
            assert_eq!(submitted, 1);
            reap_n(&ring, 1);
        },
        20,
    );
}

/// A driver that loops on bounded waits until its ops complete, as a ring thread typically does,
/// finishes under PCT, which keeps the highest-priority task running while it can. A
/// completion the kernel task has posted but whose tail it has not stored yet is still to come, so a
/// bounded wait blocks for it rather than timing out at once, again and again, while the kernel task
/// never runs.
#[test]
fn a_bounded_wait_loop_finishes_under_pct() {
    shuttle::check_pct(
        || {
            let ring = IoUring::new(8).unwrap();
            for ud in 0..4 {
                push(&ring, opcode::Nop::new().build().user_data(ud));
            }
            ring.submit().unwrap();
            let ts = timeout();
            let args = SubmitArgs::new().timespec(&ts);
            let mut done = 0;
            for _ in 0..10_000 {
                done += reap(&ring).len();
                if done == 4 {
                    return;
                }
                let _ = ring.submitter().submit_with_args(1, &args);
            }
            panic!("the bounded waits spun with {done} of 4 completions reaped");
        },
        300,
        3,
    );
}
