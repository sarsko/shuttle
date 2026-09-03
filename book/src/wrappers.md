# Third-party crates and wrappers

Everything so far assumed the concurrency lives in *your* code, where you can swap
`std::sync::Mutex` for `shuttle::sync::Mutex`. Real crates are not like that. Your service
holds a `tokio::sync::Mutex` behind a connection pool, a dependency uses `parking_lot::RwLock`
for its cache, another hands out `DashMap` handles. You do not own those lines. This chapter is
about closing that gap with the wrapper crates in `wrappers/`.

## Why an unwrapped dependency breaks your test

Shuttle only controls the primitives it implements. A Shuttle test runs on a single OS thread,
with a cooperative scheduler that gets a choice at every `shuttle::sync` operation and every
`.await` on a Shuttle future ([How Shuttle works](./internals.md)). Everything else is
invisible to it, which gives you two failure modes:

* **Silent under-testing.** A real `parking_lot::RwLock` inside a dependency blocks the one
  thread Shuttle is using. Shuttle never learns a lock was taken, so it never tries the
  interleaving where two of your tasks race inside that dependency. The test passes and tells
  you nothing.
* **Deadlock or hang.** If two Shuttle tasks contend on a real lock, the first to block parks
  the only thread that could release it. The test wedges instead of reporting a deadlock the
  way Shuttle would for its own primitives.

The same applies to `std::thread::spawn`, real `tokio::spawn`, real atomics used as
handshakes, and `HashMap` iteration order. [Determinism rules and common
pitfalls](./pitfalls.md) covers the failure signatures.

## The wrapper approach

A wrapper crate is a drop-in replacement that re-exports *either* the real crate *or* a
Shuttle-compatible reimplementation, chosen at compile time by a `shuttle` feature. The whole
mechanism is a `cfg_if!` at the top of a tiny crate:

```rust,ignore
// shuttle-parking_lot/src/lib.rs, in full
cfg_if::cfg_if! {
    if #[cfg(feature = "shuttle")] {
        pub use shuttle_parking_lot_impl::*;
    } else {
        pub use parking_lot::*;
    }
}
```

You wire it up with Cargo's `package` key, so the dependency keeps the *name* your source code
already uses:

```toml
[features]
shuttle = [
    "tokio/shuttle",
    "parking_lot/shuttle",
    # ... etc. for all wrapped dependencies
]

[dependencies]
tokio = { package = "shuttle-tokio", version = "1" }
parking_lot = { package = "shuttle-parking_lot", version = "0.12" }
```

Not one line of `use tokio::sync::Mutex;` changes. Without `--features shuttle` you get real
tokio and real parking_lot; with it, implementations built on `shuttle::sync` and
`shuttle::future`.

Two things to get right. The string before the slash in the feature list is the **dependency
key**, not the package name — `"parking_lot/shuttle"`, matching the `parking_lot = { package =
... }` line. And because this is a compile-time swap, a Shuttle run and a production build are
two separate builds that cannot share artifacts; plan for both in CI ([Performance and
continuous integration](./ci-and-performance.md)).

## Available wrappers

Versions are what the repository declares today. "Wrapper version" is what you write in
`version = "..."`; "wrapped version" is the constraint the wrapper puts on the real crate,
which matters for [pinning](#pinning-the-wrapped-crate).

| You depend on | Wrapper crate | Wrapper version | Wrapped version | Covers |
| --- | --- | --- | --- | --- |
| `tokio` | `shuttle-tokio` | `1` | `1` | `task`/`spawn`, `runtime`, all of `sync`, `time` stubs, `select!`, `#[tokio::test]` |
| `tokio-stream` | `shuttle-tokio-stream` | `0.1` | `0.1` | `StreamExt`, `wrappers`, timeouts (fork of 0.1.14) |
| `tokio-util` | `shuttle-tokio-util` | `0.7` | `0.7` | `sync`, `codec`, `io`, `task` (fork of 0.7.11) |
| `tokio-retry` | `shuttle-tokio-retry` | `0.3` | `0.3` | `Retry`, `RetryIf`, `strategy` (uses `shuttle-rand` for jitter) |
| `parking_lot` | `shuttle-parking_lot` | `0.12` | `0.12` | `Mutex`, `RwLock`, raw locks, mapped/`Arc`/upgradable guards |
| `rand` (0.8) | `shuttle-rand` | `0.8` | `0.8.6` | `thread_rng`, `Rng`, `distributions`, `seq`, `SmallRng` |
| `dashmap` | `shuttle-dashmap` | `6` | `6.1.0` | `DashMap`, `DashSet`, `Ref`/`RefMut`, iterators |
| `lazy_static` | `shuttle-lazy_static` | `1.5` | `1` | the `lazy_static!` macro |
| `async-stream` | `shuttle-async-stream` | `0.3` | `0.3` | `stream!`, `try_stream!` |
| `std::sync` | `shuttle-sync` | `0.1` | — | `shuttle_sync::sync::*` |
| `std::collections` | `determinizable_collections` | `0.1` | — | `HashMap`, `HashSet` with switchable hasher |

Most wrappers deliberately carry the version number of the crate they wrap, which is why
`shuttle-parking_lot` is at `0.12`, `shuttle-dashmap` at `6.1` and `shuttle-tokio` at `1`. The
exceptions are the two crates that wrap `std` rather than a crates.io dependency,
`shuttle-sync` and `determinizable_collections`, which carry their own `0.1`.

Each wrapper forwards the wrapped crate's own feature flags, so `features = ["full"]` on
`shuttle-tokio` or `features = ["serde"]` on `shuttle-parking_lot` behave as you expect in
both builds.

You will also see crates named `*-impl` and `*-inner` (`shuttle-tokio-impl`,
`shuttle-tokio-impl-inner`, `shuttle-parking_lot-impl`, `shuttle-rand_0_8-inner`,
`shuttle-dashmap-impl`, ...). Those hold the Shuttle-backed code and exist so the wrapper can
stay a three-line `cfg_if!`. Do not depend on them directly; some are not published at all.

## Feature plumbing at scale

The `shuttle` feature list is the fragile part. Add a wrapped dependency, forget its
`"newdep/shuttle"` line, and your Shuttle build silently links the *real* crate again — back to
the under-testing failure from the top of this chapter, with no error to tell you.

`wrappers/README.md` proposes a `shuttle_enabler` crate as the fix: one dependency whose own
`shuttle` feature turns on `shuttle` for every wrapper, so your feature list collapses to
`shuttle = ["shuttle_enabler/shuttle"]` with the wrapper dependencies listed as usual. The
caveat is that it compiles every crate `shuttle_enabler` references once the flag is on.
**That crate does not exist in this repository yet** — the README documents the intended
design, not something you can `cargo add` today. Until it lands, write the list out by hand and
put it on your review checklist.

The other half of the problem — keeping versions consistent — has a fix you can use now: create
a dedicated `versions` crate that declares the dependency versions you want, and have all your
crates depend on it. For projects spanning multiple workspaces, use workspace dependencies with
`workspace = true` so a single edit propagates.

## Pinning the wrapped crate

The wrappers use *minimal* version constraints on what they wrap: crates at 1.x or higher
(`tokio`, `lazy_static`) are constrained to `1`, and crates at 0.x (`rand`, `parking_lot`) to
the minor version, e.g. `0.8`, `0.12`. So Cargo picks the newest compatible release by default,
exactly as if you had written `tokio = "1"` yourself. To pin a specific version, add a second
dependency entry that pulls in the real crate under a name nothing imports:

```toml
[dependencies]
tokio = { package = "shuttle-tokio", version = "1" }
tokio-version-importer-do-not-use-directly = { package = "tokio", version = "=1.36.0" }
```

Both entries resolve to the same `tokio` in the dependency graph, so the `=1.36.0` constraint
wins and the wrapper gets the version you asked for. The alias name means nothing to Cargo; it
is deliberately unusable so nobody imports it by accident. (The wrapper crates' own rustdoc
uses a shorter spelling, `tokio-version-import-dont-use`; either works.)

The more robust alternative is the `versions` crate from the previous section: put the pinning
entries there once, depend on it everywhere, and there is no importer alias floating around
your leaf crates to misuse.

## Collections and determinism

`HashMap` and `HashSet` seed their hasher from the OS on every construction, so iteration order
differs between processes. Shuttle's replay depends on your program making the *same sequence of
decisions* given the same schedule — if a loop over a `HashMap` visits keys in a different order
on replay, the schedule no longer lines up and the failure will not reproduce. This is one of
the most common causes of "the schedule string doesn't replay"; see
[pitfalls](./pitfalls.md).

Three crates address this. **`determinizable_collections`** is the one you depend on; its
`deterministic` feature picks the implementation. **`deterministic_collections`** provides
`HashMap`/`HashSet` with a fixed `RandomState`, so iteration order is reproducible.
**`std_collections_reexport`** re-exports the std types (hasher fixed to `RandomState`) for the
feature-off path; it is separate purely so it can be updated without bumping the front end.

```toml
[features]
shuttle = ["determinizable_collections/deterministic"]

[dependencies]
determinizable_collections = "0.1"
```

The feature is `deterministic`, not `shuttle` — this crate is useful outside Shuttle too. A
common setup is to enable it unconditionally for tests, so ordinary `cargo test` failures are
reproducible as well:

```toml
[dev-dependencies]
determinizable_collections = { version = "0.1", features = ["deterministic"] }
```

Determinism is not free. A fixed hasher gives up HashDoS resistance and removes the accidental
benefit that order-dependent bugs sometimes disappear on retry. Enabling it in pre-production to
make failures reproducible is reasonable; enabling it in production is a deliberate trade-off.

## Per-crate notes

### tokio

`shuttle-tokio` is the largest wrapper and has the most caveats.

* `spawn`, `JoinHandle`, `runtime::Handle`, `yield_now`, `select!` and the whole `sync` module
  (`Mutex`, `RwLock`, `Semaphore`, `Notify`, `OnceCell`, `mpsc`, `oneshot`, `broadcast`,
  `watch`) are reimplemented on Shuttle.
* `#[tokio::test]` does the right thing in both builds: without the feature it is tokio's
  macro; with it, it wraps your async body in a Shuttle check running **100 iterations** by
  default, with `stack_size = 0x000F_0000` and `max_steps = FailAfter(10_000_000)`. Since you
  do not get to build that `Config` yourself, the harness reads `SHUTTLE_ITERATIONS`,
  `SHUTTLE_TIMEOUT_SECS`, `SHUTTLE_SCHEDULER`, `SHUTTLE_PCT_MAX_DEPTH`, `SHUTTLE_TRACE_DIR`,
  `SHUTTLE_TRACE_FILE` and `SHUTTLE_HIDE_TRACE` instead — see [Read by the `shuttle-tokio` test
  harness](./configuration.md#read-by-the-shuttle-tokio-test-harness). Needs the `macros` feature.
* `io`, `net` and `fs` are plain re-exports of real tokio. They exist so your code *compiles*
  under the feature; using them inside a Shuttle test will misbehave.
* Time is not modeled. `time::pause()` is a no-op and timers do not advance; the impl instead
  offers hooks for forcing specific timeouts to fire. `Interval::tick()` returns immediately and
  forever, so `SHUTTLE_INTERVAL_TICKS` exists to bound the number of ticks an `Interval` hands
  out. Do not write tests whose logic depends on a `sleep` completing — see [Async code and
  futures](./async.md).
* `task_local!` is known to be wrong: all tasks currently share one slot.
* The tracked list of unsupported constructs is
  [awslabs/shuttle#241](https://github.com/awslabs/shuttle/issues/241).

`shuttle-tokio-stream`, `shuttle-tokio-util` and `shuttle-tokio-retry` are forks of those
crates with their tokio imports pointed at `shuttle-tokio-impl`; add them alongside the tokio
wrapper with the same pattern. `shuttle-tokio-retry` routes its backoff jitter through
`shuttle-rand`, so retry timing becomes part of the schedule Shuttle controls.

### parking_lot

`shuttle-parking_lot` is built the way real `parking_lot` is: raw locks implementing the
`lock_api` traits, with the generic `lock_api` containers on top. The raw locks use Shuttle's
`BatchSemaphore`, so blocking routes through the scheduler, and the full guard surface —
mapped guards, `Arc` guards (`arc_lock`), upgradable read guards — comes along for free.
`send_guard` behaves exactly as upstream.

Not present: `Condvar`, `Once`, `ReentrantMutex`, `FairMutex`. A dependency using those will
fail to compile under the feature rather than silently mislead you.

### rand

Use `shuttle::rand` directly in code you own; use the `shuttle-rand` wrapper for dependency
code you cannot edit.

```toml
[features]
shuttle = ["rand/shuttle"]

[dependencies]
rand = { package = "shuttle-rand", version = "0.8" }
```

Under the feature, `thread_rng()` returns a single Shuttle-seeded generator shared by all tasks
(not actually thread-local, but indistinguishable since it cannot be re-seeded). `SmallRng` and
`StdRng`, behind their usual features, are thin forwards to that same generator, so
`SeedableRng::from_seed` ignores your seed. Only `rand` 0.8 is wrapped; there is no 0.9 wrapper.

### dashmap

`shuttle-dashmap` replaces `DashMap` with one `shuttle::sync::RwLock` over a
`deterministic_collections::HashMap`. That is coarser than real dashmap's per-shard locking, so
it serializes operations that would run concurrently in production — but it makes every pair of
concurrent operations visible to the scheduler, which is the point. Guards store a clone of the
key and look the value up through the lock, so `get`/`get_mut`/`entry` cost an extra clone.

### lazy_static

`shuttle-lazy_static` forwards to `shuttle::lazy_static`, which re-initializes statics per
execution — necessary, since every iteration must start from the same state. One behavioral
difference: Shuttle *drops* the static at the end of each execution while the real crate never
does. If that produces false positives, silence the warning with `SHUTTLE_SILENCE_WARNINGS` or
`Config::silence_warnings`.

### async-stream

`shuttle-async-stream` provides `stream!` and `try_stream!` on Shuttle's future machinery, so a
generator-style stream yields at points the scheduler can interleave.

### std::sync

`shuttle-sync` is the odd one out: `std` is not a Cargo dependency, so there is no name to
hijack and this wrapper *does* require a source change. Import `shuttle_sync::sync::{Mutex,
Arc}` instead of `std::sync::{Mutex, Arc}` and the feature flag takes over from there.

Note that `std::sync` is a superset of `shuttle::sync`, so mechanically replacing every
`std::sync` import will produce "not found" errors once the feature is on. Keep importing the
unmodeled items from `std` directly, or add support upstream.

## A worked example

A crate with one tokio-based component. The bug is a lost update: the lock is released between
the read and the write.

```toml
[package]
name = "bumper"
version = "0.1.0"
edition = "2021"

[features]
shuttle = ["tokio/shuttle"]

[dependencies]
tokio = { package = "shuttle-tokio", version = "1", features = ["rt", "sync", "macros"] }
```

```rust,ignore
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Clone, Default)]
pub struct Counter(Arc<Mutex<u64>>);

impl Counter {
    pub async fn bump(&self) {
        let seen = *self.0.lock().await; // guard dropped at the end of this statement
        *self.0.lock().await = seen + 1; // ... and reacquired here
    }

    pub async fn get(&self) -> u64 {
        *self.0.lock().await
    }
}

#[tokio::test]
async fn concurrent_bumps_do_not_lose_updates() {
    let counter = Counter::default();
    let (a, b) = (counter.clone(), counter.clone());
    let ha = tokio::spawn(async move { a.bump().await });
    let hb = tokio::spawn(async move { b.bump().await });
    ha.await.unwrap();
    hb.await.unwrap();
    assert_eq!(counter.get().await, 2);
}
```

Two builds, two behaviors:

```sh
cargo test                     # real tokio: uncontended locks don't yield, usually passes
cargo test --features shuttle  # Shuttle: 100 iterations of controlled interleavings
```

Under the feature, `tokio::sync::Mutex` and `tokio::spawn` resolve to Shuttle implementations,
`#[tokio::test]` becomes a Shuttle check, and the scheduler puts one task's read between the
other's read and write. The failure comes with a replayable schedule:

```text
Task failed, serializing schedule
test panicked in task 'main-thread'
failing schedule:
"
<hex schedule>
"
pass that string to `shuttle::replay` to replay the failure
```

See [Debugging failures: schedules and replay](./debugging.md) for what to do with that
string.

## When there is no wrapper

First decide whether the dependency's concurrency is in scope for your test at all. Ask: does
it spawn threads or tasks that outlive your call? Does it hold a lock across a callback into
*your* code? Do two of your tasks reach shared state inside it? If all three are no — it locks,
mutates, returns, no reentrancy — its internals are effectively atomic from the test's point of
view, and leaving it unwrapped costs you only a serialization point. If any is yes, Shuttle is
not testing the thing you care about, and you have three options.

**Vendor a shim behind your own cfg.** If you touch a small part of the dependency's API, wrap
just that in a module you control and apply the same trick:

```rust,ignore
pub mod cache {
    #[cfg(feature = "shuttle")]
    pub use crate::shuttle_shims::cache::*;
    #[cfg(not(feature = "shuttle"))]
    pub use real_cache::*;
}
```

Cheap for a handful of types, unpleasant for a large surface.

**Narrow the test.** Test the component you own, with the dependency replaced by a hand-written
stub built on `shuttle::sync`. You lose coverage of the interaction, but the test says something
true.

**Contribute a wrapper.** The pattern is small and mechanical: a front-end crate that is nothing
but a `cfg_if!`, an `-impl` crate holding the Shuttle-backed code, feature forwarding for every
upstream feature, and a minimal version constraint on the wrapped crate. `shuttle-lazy_static`
and `shuttle-parking_lot` are the smallest templates to copy; `shuttle-parking_lot-impl` shows
how to reuse an upstream generic layer (`lock_api`) instead of reimplementing an API by hand.
See `CONTRIBUTING.md` in the repository root, and open an issue before starting anything large.
