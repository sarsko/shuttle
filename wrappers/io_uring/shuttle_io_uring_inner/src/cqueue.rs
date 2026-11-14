// This file is derived from turmoil-io-uring 0.1.0 (crates/turmoil-io-uring in
// https://github.com/tokio-rs/turmoil, commit 684acc1a8eea3a9cf2c6959dc47b69dba981cac1),
// Copyright (c) 2022 Carl Lerche, and licensed under the MIT license: see LICENSE-MIT in this crate.
// The changes to it are licensed under the Apache License, Version 2.0, as the rest of Shuttle is.

//! Completion queue, mirroring [`io_uring::cqueue`](https://docs.rs/io-uring/0.7/io_uring/cqueue).

use std::fmt;
use std::mem::MaybeUninit;

use crate::kernel::Ring;

/// `IORING_CQE_F_BUFFER`.
const F_BUFFER: u32 = 1;
/// `IORING_CQE_F_MORE`.
const F_MORE: u32 = 1 << 1;
/// `IORING_CQE_F_SOCK_NONEMPTY`.
const F_SOCK_NONEMPTY: u32 = 1 << 2;
/// `IORING_CQE_F_NOTIF`.
const F_NOTIF: u32 = 1 << 3;
/// `IORING_CQE_F_BUF_MORE`.
const F_BUF_MORE: u32 = 1 << 4;
/// `IORING_CQE_BUFFER_SHIFT`.
const BUFFER_SHIFT: u32 = 16;

/// Selects the CQE shape. Only the default 16-byte [`Entry`] is simulated.
pub trait EntryMarker: private::Sealed + Clone + fmt::Debug + Send + Sync + 'static {}

/// One completion queue entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub(crate) user_data: u64,
    pub(crate) result: i32,
    pub(crate) flags: u32,
}

impl Entry {
    /// The op's result: non-negative on success, `-errno` on failure.
    pub fn result(&self) -> i32 {
        self.result
    }

    /// The `user_data` of the SQE this completes.
    pub fn user_data(&self) -> u64 {
        self.user_data
    }

    /// Overwrite the `user_data`, as the real crate allows.
    pub fn set_user_data(&mut self, user_data: u64) {
        self.user_data = user_data;
    }

    /// The CQE flags. Always zero: the simulation has no multishot ops, provided buffers, or
    /// notification CQEs.
    pub fn flags(&self) -> u32 {
        self.flags
    }
}

impl EntryMarker for Entry {}

/// The completion queue, mirroring the real crate's `CompletionQueue`.
///
/// As on a real ring, the handle sees the completions posted by the time it was created or last
/// [`sync`](Self::sync)ed: loading the kernel's CQ tail is a Shuttle scheduling point, so the kernel
/// can post between a consumer's loads, and a consumer that forgets to `sync` misses completions here
/// as it would on Linux. Taking an entry is not a scheduling point.
pub struct CompletionQueue<'a, E: EntryMarker = Entry> {
    ring: &'a Ring,
    /// Completions the last tail load exposed and not yet taken.
    visible: u32,
    _marker: std::marker::PhantomData<E>,
}

impl<'a, E: EntryMarker> CompletionQueue<'a, E> {
    pub(crate) fn new(ring: &'a Ring) -> Self {
        let mut queue = Self {
            ring,
            visible: 0,
            _marker: std::marker::PhantomData,
        };
        queue.sync();
        queue
    }

    /// Load the kernel's CQ tail. A Shuttle scheduling point.
    pub fn sync(&mut self) {
        self.visible = self.ring.cq_visible();
    }

    /// Completions the kernel dropped for lack of room. Always zero: the simulation has
    /// `IORING_FEAT_NODROP` and holds them back instead, to be flushed by the next enter.
    pub fn overflow(&self) -> u32 {
        0
    }

    /// Whether `IORING_CQ_EVENTFD_DISABLED` is set: the kernel then posts completions without
    /// signalling the registered eventfd. Loading the flag word is a Shuttle scheduling point.
    pub fn eventfd_disabled(&self) -> bool {
        self.ring.eventfd_disabled()
    }

    /// Set `IORING_CQ_EVENTFD_DISABLED`: the kernel stops signalling the registered eventfd. A
    /// Shuttle scheduling point.
    pub fn disable_eventfd(&self) {
        self.ring.set_eventfd_disabled(true);
    }

    /// Clear `IORING_CQ_EVENTFD_DISABLED`. The kernel loads the flag after storing each new tail, so
    /// a consumer that clears it and then loads the tail races the kernel exactly as on Linux; both
    /// are Shuttle scheduling points.
    pub fn enable_eventfd(&self) {
        self.ring.set_eventfd_disabled(false);
    }

    /// The number of entries the queue holds.
    pub fn capacity(&self) -> usize {
        self.ring.cq_entries as usize
    }

    /// Whether no completion is visible.
    pub fn is_empty(&self) -> bool {
        self.visible == 0
    }

    /// Whether the visible completions fill the queue.
    pub fn is_full(&self) -> bool {
        self.len() == self.capacity()
    }

    /// Take up to `entries.len()` visible completions into `entries`, and return them.
    pub fn fill<'b>(&mut self, entries: &'b mut [MaybeUninit<E>]) -> &'b mut [E] {
        let len = self.len().min(entries.len());
        for slot in &mut entries[..len] {
            slot.write(self.next().expect("a visible completion is there to take"));
        }
        // SAFETY: the first `len` slots were just initialized, and `MaybeUninit<E>` has `E`'s layout.
        unsafe { std::slice::from_raw_parts_mut(entries.as_mut_ptr().cast::<E>(), len) }
    }
}

impl<E: EntryMarker> Iterator for CompletionQueue<'_, E> {
    type Item = E;

    fn next(&mut self) -> Option<E> {
        if self.visible == 0 {
            return None;
        }
        let entry = self.ring.take_cqe();
        self.visible -= 1;
        let any: Box<dyn std::any::Any> = Box::new(entry);
        Some(*any.downcast::<E>().expect("Entry is the only simulated CQE shape"))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.visible as usize, Some(self.visible as usize))
    }
}

impl<E: EntryMarker> ExactSizeIterator for CompletionQueue<'_, E> {}

/// Whether the CQE `flags` say the kernel has more data for the buffer it selected
/// (`IORING_CQE_F_BUF_MORE`). Never set by the simulation.
pub fn buffer_more(flags: u32) -> bool {
    flags & F_BUF_MORE != 0
}

/// The id of the buffer the kernel selected for the op, if the CQE `flags` say it selected one
/// (`IORING_CQE_F_BUFFER`). Never set by the simulation.
pub fn buffer_select(flags: u32) -> Option<u16> {
    (flags & F_BUFFER != 0).then_some((flags >> BUFFER_SHIFT) as u16)
}

/// Whether the CQE `flags` say more completions follow for the same SQE (`IORING_CQE_F_MORE`).
/// Never set by the simulation.
pub fn more(flags: u32) -> bool {
    flags & F_MORE != 0
}

/// Whether the CQE `flags` say the socket has more data to read (`IORING_CQE_F_SOCK_NONEMPTY`).
/// Never set by the simulation.
pub fn sock_nonempty(flags: u32) -> bool {
    flags & F_SOCK_NONEMPTY != 0
}

/// Whether the CQE `flags` mark a zero-copy notification (`IORING_CQE_F_NOTIF`). Never set by the
/// simulation.
pub fn notif(flags: u32) -> bool {
    flags & F_NOTIF != 0
}

mod private {
    pub trait Sealed {}
    impl Sealed for super::Entry {}
}
