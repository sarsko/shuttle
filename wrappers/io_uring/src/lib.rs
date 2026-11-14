//! This crate provides a Shuttle-compatible implementation and wrapper for [`io-uring`] in order to
//! make it more ergonomic to run a codebase that drives an `io_uring` under Shuttle.
//!
//! [`io-uring`]: <https://crates.io/crates/io-uring>
//!
//! To use this crate, add something akin to the following to your Cargo.toml:
//!
//! ```text
//! [features]
//! shuttle = [
//!    "io-uring/shuttle",
//! ]
//!
//! [target.'cfg(target_os = "linux")'.dependencies]
//! io-uring = { package = "shuttle-io-uring", version = "0.7" }
//! ```
//!
//! The rest of the codebase then remains unchanged, and running with a Shuttle-compatible `io-uring`
//! can be done via the "shuttle" feature flag. Without it, this crate is io-uring: the kernel's ring.
//! With it, it is [`shuttle-io-uring-inner`]: a simulated kernel, with io-uring 0.7.16's API for the
//! operations it models, whose completions Shuttle schedules. Each ring has a kernel task that
//! completes the ops in flight, one per step, in an order Shuttle chooses, by running each op's
//! system call on the real file descriptor when it completes. So a ring driver runs unchanged under
//! Shuttle, its races between completions, submissions, wakeups and cancellation are explored, and a
//! failing schedule replays. See [`shuttle-io-uring-inner`] for the model, what it checks, and what it
//! does not model.
//!
//! Under the feature the crate also has a few items that io-uring does not, all of them for tests:
//! `AsyncFd`, for the readiness of the fds the simulated kernel signals (tokio's `AsyncFd` needs an I/O
//! driver, which a Shuttle execution does not have), and the `faults` and `checks` modules, which turn
//! on the errors the simulated kernel injects and the checks it makes of a ring's buffers.
//!
//! io_uring exists only on Linux, and so on other targets this crate is empty.
//!
//! By default the version of `io-uring` exported is the latest `0.7` version (the same as if you had
//! written `io-uring = "0.7"` in your Cargo.toml), at least 0.7.16. If you need to constrain the
//! version of `io-uring` that you depend on (eg. to pin the version), then this can be done by adding
//! an entry like:
//!
//! ```text
//! [dependencies]
//! io-uring-version-import-dont-use = { package = "io-uring", version = "=0.7.16" }
//! ```
//!
//! [`shuttle-io-uring-inner`]: <https://docs.rs/shuttle-io-uring-inner>

#[cfg(target_os = "linux")]
cfg_if::cfg_if! {
    if #[cfg(feature = "shuttle")] {
        pub use shuttle_io_uring_inner::*;
    } else {
        pub use io_uring_orig::*;
    }
}
