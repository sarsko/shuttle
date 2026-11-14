//! Tests of the wrapper as a dependent crate sees it. The workspace builds it without the `shuttle`
//! feature, so these check that it then is io-uring, driving the kernel's ring;
//! `cargo test -p shuttle-io-uring --features shuttle` runs the other half. `shuttle-io-uring-inner`'s
//! tests run the same consumer code, and build the same parity probe, against the simulation.

#![cfg(target_os = "linux")]

extern crate shuttle_io_uring as io_uring;

mod consumer;
mod parity;

/// Whether this process can build a kernel ring: a seccomp filter or `kernel.io_uring_disabled` can
/// forbid it, as some containers do.
#[cfg(not(feature = "shuttle"))]
fn kernel_ring_available() -> bool {
    match io_uring::IoUring::new(8) {
        Ok(_) => true,
        Err(e) => {
            eprintln!("skipping: this process cannot build an io_uring: {e}");
            false
        }
    }
}

#[cfg(not(feature = "shuttle"))]
mod without_shuttle {
    use super::kernel_ring_available;
    use std::any::TypeId;

    #[test]
    fn types_are_io_urings_own() {
        assert_eq!(
            TypeId::of::<io_uring::IoUring>(),
            TypeId::of::<io_uring_orig::IoUring>()
        );
        assert_eq!(
            TypeId::of::<io_uring::Builder>(),
            TypeId::of::<io_uring_orig::Builder>()
        );
        assert_eq!(
            TypeId::of::<io_uring::squeue::Entry>(),
            TypeId::of::<io_uring_orig::squeue::Entry>()
        );
        assert_eq!(
            TypeId::of::<io_uring::cqueue::Entry>(),
            TypeId::of::<io_uring_orig::cqueue::Entry>()
        );
        assert_eq!(
            TypeId::of::<io_uring::opcode::Writev>(),
            TypeId::of::<io_uring_orig::opcode::Writev>()
        );
    }

    #[test]
    fn the_parity_probe_builds_against_io_uring() {
        super::parity::signatures();
    }

    #[test]
    fn consumer_code_runs_on_the_kernel() {
        if kernel_ring_available() {
            super::consumer::exercise();
        }
    }

    /// The kernel completes ops on its own schedule, so under Shuttle a test whose control flow
    /// depends on when a completion shows up does not replay, which
    /// `check_uncontrolled_nondeterminism` reports. With the `shuttle` feature the same code replays
    /// (`shuttle-io-uring-inner`'s `consumer_code_replays`).
    #[test]
    fn kernel_completions_are_nondeterminism_under_shuttle() {
        if !kernel_ring_available() {
            return;
        }
        let result = std::panic::catch_unwind(|| {
            shuttle::check_uncontrolled_nondeterminism(
                || {
                    let ring = io_uring::IoUring::new(8).unwrap();
                    let ts = io_uring::types::Timespec::new().nsec(200_000);
                    let timeout = io_uring::opcode::Timeout::new(&ts).build().user_data(1);
                    // SAFETY: the timespec outlives the op, which completes before the ring drops.
                    unsafe { ring.submission_shared().push(&timeout) }.unwrap();
                    ring.submit().unwrap();
                    // How many times this polls depends on when the kernel's timer fires, and each
                    // poll is a Shuttle scheduling point.
                    // SAFETY: the only CQ handle.
                    while unsafe { ring.completion_shared() }.is_empty() {
                        shuttle::thread::yield_now();
                    }
                },
                20,
            )
        });
        let message = result.expect_err("the kernel's timing is uncontrolled nondeterminism");
        let message = message
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| message.downcast_ref::<&str>().copied())
            .unwrap_or_default();
        assert!(message.contains("nondeterminism"), "{message}");
    }
}

#[cfg(feature = "shuttle")]
mod with_shuttle {
    use std::any::TypeId;

    #[test]
    fn types_are_the_simulations() {
        assert_eq!(
            TypeId::of::<io_uring::IoUring>(),
            TypeId::of::<shuttle_io_uring_inner::IoUring>()
        );
        assert_ne!(
            TypeId::of::<io_uring::IoUring>(),
            TypeId::of::<io_uring_orig::IoUring>()
        );
        assert_eq!(
            TypeId::of::<io_uring::squeue::Entry>(),
            TypeId::of::<shuttle_io_uring_inner::squeue::Entry>()
        );
    }

    #[test]
    fn the_parity_probe_builds_against_the_simulation() {
        super::parity::signatures();
    }

    #[test]
    fn consumer_code_runs_under_shuttle() {
        shuttle::check_random(super::consumer::exercise, 50);
    }

    #[test]
    fn consumer_code_replays() {
        shuttle::check_uncontrolled_nondeterminism(super::consumer::exercise, 20);
    }

    #[test]
    #[should_panic(expected = "Are you accessing a Shuttle primitive outside of a Shuttle test?")]
    fn building_a_ring_outside_of_shuttle_panics() {
        let _ = io_uring::IoUring::new(8);
    }
}
