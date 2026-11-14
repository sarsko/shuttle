// This file is derived from turmoil-io-uring 0.1.0 (crates/turmoil-io-uring in
// https://github.com/tokio-rs/turmoil, commit 684acc1a8eea3a9cf2c6959dc47b69dba981cac1),
// Copyright (c) 2022 Carl Lerche, and licensed under the MIT license: see LICENSE-MIT in this crate.
// The changes to it are licensed under the Apache License, Version 2.0, as the rest of Shuttle is.

//! The simulated kernel's state that outlives a ring but not an execution: the fds an `AsyncFd` can
//! wait on, the next ring fd, and the faults and checks a test turned on.
//!
//! It lives in the storage of the running execution, which Shuttle drops when the execution ends, so
//! every execution starts with no rings, the same ring fd numbers, and the defaults, whatever an
//! earlier execution did. A ring holds the host of the execution that built it, so that it can reach
//! the host without borrowing the execution's state, which its destructor may run without.

use std::collections::HashMap;
use std::os::fd::RawFd;
use std::sync::{Arc, Mutex, MutexGuard};

use shuttle_engine::runtime::execution::ExecutionState;
use shuttle_engine::runtime::storage::StorageKey;

use crate::async_fd::Readiness;
use crate::checks::Checks;
use crate::faults::{Errors, Faults, Posted};

/// Simulated ring fds start here, above any fd the process opens, so they never collide with the
/// eventfds sharing the registry.
const RING_FD_BASE: RawFd = 1 << 28;

/// The state of one execution's simulated kernel.
pub(crate) struct Host {
    state: Mutex<HostState>,
}

pub(crate) struct HostState {
    pub(crate) next_ring_fd: RawFd,
    /// The fds an `AsyncFd` can wait on: rings' own fds, and eventfds registered with a ring.
    pub(crate) fds: HashMap<RawFd, Arc<Readiness>>,
    pub(crate) faults: Faults,
    pub(crate) errors: Errors,
    pub(crate) posted: Posted,
    pub(crate) checks: Checks,
}

fn key() -> StorageKey {
    static ID: u8 = 0;
    // The second half tells this slot apart from Shuttle's own (`Once` uses 0x2, `lazy_static` 0x3),
    // whose first half is also an address.
    StorageKey(&ID as *const u8 as usize, 0x696f_7572)
}

impl Host {
    /// The running execution's host, created on first use. Panics outside of a Shuttle test, as the
    /// rest of Shuttle does.
    pub(crate) fn current() -> Arc<Host> {
        ExecutionState::with(|state| {
            if let Some(host) = state.get_storage::<_, Arc<Host>>(key()) {
                return Arc::clone(host);
            }
            let host = Arc::new(Host {
                state: Mutex::new(HostState {
                    next_ring_fd: RING_FD_BASE,
                    fds: HashMap::new(),
                    faults: Faults::default(),
                    errors: Errors::default(),
                    posted: Posted::default(),
                    checks: Checks::default(),
                }),
            });
            state.init_storage(key(), Arc::clone(&host));
            host
        })
    }

    /// A plain lock: nothing that takes a scheduling point or wakes a task runs under it, so it
    /// adds no scheduling points of its own.
    pub(crate) fn lock(&self) -> MutexGuard<'_, HostState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// An fd number for a new simulated ring.
    pub(crate) fn alloc_ring_fd(&self) -> RawFd {
        let mut state = self.lock();
        let fd = state.next_ring_fd;
        state.next_ring_fd += 1;
        fd
    }

    /// Make `fd` waitable. Re-registering a number (an eventfd closed and reopened) replaces the old
    /// entry; `AsyncFd`s built on it keep the old one.
    pub(crate) fn register(&self, fd: RawFd, readiness: Arc<Readiness>) {
        self.lock().fds.insert(fd, readiness);
    }

    /// Forget `fd`, if it still maps to `readiness`.
    pub(crate) fn unregister(&self, fd: RawFd, readiness: &Arc<Readiness>) {
        let mut state = self.lock();
        if state
            .fds
            .get(&fd)
            .is_some_and(|current| Arc::ptr_eq(current, readiness))
        {
            state.fds.remove(&fd);
        }
    }

    /// The readiness an `AsyncFd` on `fd` waits on, if the simulated kernel signals `fd`.
    pub(crate) fn readiness(&self, fd: RawFd) -> Option<Arc<Readiness>> {
        self.lock().fds.get(&fd).cloned()
    }
}
