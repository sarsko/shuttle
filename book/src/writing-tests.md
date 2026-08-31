# Writing Shuttle tests

Getting a Shuttle test to *find* bugs is usually the easy part. The work is getting your crate into
a shape where Shuttle is in control of every scheduling decision it needs to make, without forking
your codebase into a "Shuttle version" and a "real version".

## The one rule

Inside a Shuttle test, every synchronization primitive and every thread must come from Shuttle
rather than `std`.

Shuttle does not run your threads on real OS threads. It runs them as coroutines on a single OS
thread and decides itself which one makes progress at each step, which is what makes a failing run
reproducible from a schedule string. Two consequences follow:

- **`std::sync::Mutex` is invisible to Shuttle.** It is not a yield point, so the scheduler never
  considers switching threads there, and any interleaving that depends on that lock is unreachable.
  Worse, if a task blocks on a `std` lock, it blocks the one OS thread that Shuttle is using to run
  *all* tasks, so nothing can ever release the lock and the test hangs for real.
- **`std::thread::spawn` escapes Shuttle entirely.** The new thread runs concurrently with the
  Shuttle execution, outside the scheduler's control, so the run is neither deterministic nor
  replayable.

[How Shuttle works](./internals.md) covers the mechanism. For now, the practical consequence is that
you need a way to switch the imports in your crate.

Not everything has to change. `Arc`, `Weak`, and the lock-result types are re-exported from `std`
unchanged, and ordinary data structures, `RefCell`, `Cell`, and pure computation are all fine as-is.
Only things that block, communicate, or synchronize need swapping.

## Wiring Shuttle into a crate

### A single `sync` module behind a feature flag

The pattern that scales is to funnel all concurrency imports in your crate through one module, and
make that module's contents depend on a Cargo feature:

```toml
[dev-dependencies]
shuttle = "0.9"

[features]
shuttle = []
```

```rust,ignore
// src/sync.rs
#[cfg(all(feature = "shuttle", test))]
pub use shuttle::{sync::*, thread};

#[cfg(not(all(feature = "shuttle", test)))]
pub use std::{sync::*, thread};
```

The rest of your crate then writes `use crate::sync::{Arc, Mutex, thread};` and never mentions
`std::sync` or `std::thread` directly. Enforcing that is the only real discipline required; a
`#[deny]`-style lint or a grep in CI for `std::sync` outside `src/sync.rs` is worth the trouble.

The `test` in the `cfg` keeps Shuttle out of your shipped code: the feature only rewires imports
when the crate is compiled as a test target, where `dev-dependencies` are available. This works for
unit tests written in `#[cfg(test)] mod` blocks inside the crate.

If your Shuttle tests live in `tests/` instead, note that `cfg(test)` is *not* set when the library
is compiled for an integration test — the library would still get the `std` branch. For that layout,
drop `test` from the `cfg` and make `shuttle` an optional regular dependency:

```toml
[dependencies]
shuttle = { version = "0.9", optional = true }

[features]
shuttle = ["dep:shuttle"]
```

Either way, tests are gated on the same feature:

```rust,ignore
#[cfg(all(feature = "shuttle", test))]
mod shuttle_tests {
    use crate::sync::{Arc, Mutex, thread};

    #[test]
    fn concurrent_increment() {
        shuttle::check_random(
            || {
                // ... test body, using crate::sync ...
            },
            1000,
        );
    }
}
```

and run with:

```sh
cargo test --features shuttle
```

### Or use the wrapper crates

The workspace also ships wrapper crates that apply exactly this pattern for you, so you can leave
your imports alone. `shuttle-sync` exposes a `sync` module that is `std::sync` without the `shuttle`
feature and `shuttle::sync` with it, and there are wrappers for `tokio`, `parking_lot`, `rand`,
`lazy_static`, `dashmap`, and the std collections. One caveat, straight from `shuttle-sync`'s own
docs: it re-exports all of `std::sync`, including items Shuttle does not implement, so turning the
feature on can surface "not found" errors for those items. See
[Third-party crates and wrappers](./wrappers.md).

## What to swap

Everything below is what Shuttle actually provides today; see
[docs.rs/shuttle](https://docs.rs/shuttle) for signatures.

| Instead of | Use | Notes |
| --- | --- | --- |
| `std::thread` | `shuttle::thread` | `spawn`, `scope`, `Builder`, `JoinHandle`, `ScopedJoinHandle`, `current`, `yield_now`, `sleep`, `park`, `park_timeout`, `Thread`, `ThreadId` |
| `std::sync::Mutex` | `shuttle::sync::Mutex` | plus `MutexGuard`; `const fn new` |
| `std::sync::RwLock` | `shuttle::sync::RwLock` | plus `RwLockReadGuard`, `RwLockWriteGuard` |
| `std::sync::Condvar` | `shuttle::sync::Condvar` | plus `WaitTimeoutResult` |
| `std::sync::Barrier` | `shuttle::sync::Barrier` | plus `BarrierWaitResult` |
| `std::sync::Once` | `shuttle::sync::Once` | plus `OnceState` |
| `std::sync::mpsc` | `shuttle::sync::mpsc` | `channel`, `sync_channel`, `Sender`, `SyncSender`, `Receiver`, and the iterators |
| `std::sync::atomic` | `shuttle::sync::atomic` | `AtomicBool`, `AtomicPtr`, `AtomicI8`…`AtomicI128`, `AtomicU8`…`AtomicU128`, `AtomicIsize`, `AtomicUsize`, `fence` |
| `std::hint::spin_loop` | `shuttle::hint::spin_loop` | yields to the scheduler, so spin loops make progress |
| `std::thread_local!` | `shuttle::thread_local!` | see below |
| `lazy_static::lazy_static!` | `shuttle::lazy_static!` | see below |
| `rand::{thread_rng, Rng}` | `shuttle::rand` | see below |
| `tokio::spawn`, `block_on` | `shuttle::future` | covered in [Async code and futures](./async.md) |

`shuttle::sync` re-exports `Arc`, `Weak`, `LockResult`, `PoisonError`, `TryLockError`, and
`TryLockResult` from `std` untouched, and `shuttle::sync::atomic` re-exports `Ordering` and
`compiler_fence`. Because `Arc` is std's, Shuttle does not model its internal reference-count
atomics, so races that depend on `Arc`'s own implementation are not explored.

Shuttle has no equivalent of `std::sync::OnceLock` or `std::sync::LazyLock`; use
`shuttle::sync::Once` or `shuttle::lazy_static!` instead.

A few things exist under the same name but behave differently, which matters when you write the
test:

- `thread::sleep` and `park_timeout` do not sleep. `sleep` is just a yield point, and
  `park_timeout` behaves like `park`.
- `Condvar::wait_timeout`, `Condvar::wait_timeout_while`, and `Receiver::recv_timeout` never time
  out. They block until they are notified.
- Time is not modelled at all: `Instant` and `SystemTime` still read the real clock.

So any code whose correctness relies on a timeout firing will block forever under Shuttle. More of
these are collected in [Determinism rules and common pitfalls](./pitfalls.md).

## Globals: `thread_local!` and `lazy_static!`

Since every Shuttle task runs on the same OS thread, `std::thread_local!` gives *one* value shared
by all your tasks, and that value survives from one execution to the next. Both halves of that are
wrong. `shuttle::thread_local!` stores the value in the execution state, so each Shuttle thread gets
its own copy and everything is discarded when the execution ends:

```rust
# extern crate shuttle;
use std::cell::RefCell;
use shuttle::thread;

shuttle::thread_local! {
    static SEEN: RefCell<usize> = RefCell::new(0);
}

shuttle::check_dfs(
    || {
        let t = thread::spawn(|| {
            SEEN.with(|s| *s.borrow_mut() += 1);
            SEEN.with(|s| assert_eq!(*s.borrow(), 1));
        });

        SEEN.with(|s| assert_eq!(*s.borrow(), 0));
        t.join().unwrap();
    },
    None,
);
```

The macro accepts the same declaration syntax as std's, including `const { ... }` initializers, but
the resulting key only offers `with` and `try_with` — none of the `Cell`-specialized helpers such as
`set`, `get`, or `take`.

The same reasoning applies to plain `static`s and the real `lazy_static` crate: a value initialized
during the first execution keeps whatever it accumulated for every later execution in the process,
so executions stop being independent and replay no longer reproduces the original run.
`shuttle::lazy_static!` re-initializes once per execution:

```rust
# extern crate shuttle;
use std::collections::HashMap;

shuttle::lazy_static! {
    static ref TABLE: HashMap<u32, u32> = {
        let mut m = HashMap::new();
        m.insert(1, 1);
        m
    };
}

shuttle::check_dfs(|| assert_eq!(TABLE.get(&1), Some(&1)), None);
```

One difference from the real crate: Shuttle drops the value at the end of each execution, so its
`Drop` impl runs, which the real `lazy_static` never does. If that produces a false positive, silence
the warning with the `SHUTTLE_SILENCE_WARNINGS` environment variable or `Config::silence_warnings`.

This also applies to Shuttle's own primitives, which keep part of their state inside the object. A
`static` Shuttle `Mutex` carries lock state across executions. Prefer building your state *inside*
the test closure and handing `Arc` clones to the threads — the closure has to be `Fn + Send + Sync +
'static` anyway, so it cannot capture much else.

## Data nondeterminism: `shuttle::rand`

`shuttle::rand` is a drop-in replacement for the parts of `rand` 0.8 that most tests use:
`thread_rng`, and the `Rng`/`RngCore` traits re-exported from `rand` itself. Every value it produces
comes from Shuttle's schedule, so random choices are recorded alongside the interleaving and both
are reproduced exactly by [replay](./debugging.md). (Despite the name, the generator is not really
thread-local — all threads share one Shuttle-seeded RNG that cannot be re-seeded.)

This is what makes a stress test worth writing as a Shuttle test. Let each thread pick its next
operation randomly, and you get coverage of both the operation sequence and the interleaving, with a
schedule string that reproduces any failure:

```rust
# extern crate shuttle;
use shuttle::rand::{thread_rng, Rng};
use shuttle::sync::{Arc, Mutex};
use shuttle::thread;

shuttle::check_random(
    || {
        let buffer = Arc::new(Mutex::new(Vec::new()));

        let workers: Vec<_> = (0..2)
            .map(|_| {
                let buffer = Arc::clone(&buffer);
                thread::spawn(move || {
                    for _ in 0..3 {
                        let mut buffer = buffer.lock().unwrap();
                        if thread_rng().gen_bool(0.5) {
                            buffer.push(1u32);
                        } else {
                            buffer.pop();
                        }
                        // The invariant holds after every operation, whatever we chose.
                        assert!(buffer.len() <= 6);
                    }
                })
            })
            .collect();

        for worker in workers {
            worker.join().unwrap();
        }
    },
    50,
);
```

If your test still picks up randomness Shuttle does not control — `std`'s RNG, a `HashMap`'s
iteration order, addresses, wall-clock time — replay will silently diverge.
`shuttle::check_uncontrolled_nondeterminism` runs each random schedule twice and reports when the
two runs differ, which is a quick way to find the leak.

## What makes a good test body

The body runs hundreds or thousands of times, and each run has to be able to fail on its own. That
shapes the test more than anything else:

- **Assert invariants, not statistics.** An assertion must be checkable from inside a single
  execution. "The counter is never negative", "`take` returns one of the values that was `put`",
  "every joined thread observed a consistent view" are all fine. "Both orderings occur" is not —
  no single execution can know that.
- **You get deadlock detection for free.** If every unfinished thread is blocked, Shuttle panics
  with `deadlock! blocked tasks: [...]`, so you do not need to write assertions for it. Panics in
  *any* thread fail the test, joined or not.
- **Join your handles anyway** (or use `thread::scope`). An execution keeps running spawned threads
  after the closure returns, so without joins a final assertion may simply run too early to observe
  anything.
- **Keep it small and fast.** Total test time is roughly per-execution cost times iterations, so a
  millisecond in the body is a second at 1000 iterations. No real I/O, no file or network access, no
  sleeps, no timing assertions.
- **A handful of threads, not dozens.** Two to four threads doing two or three operations each is
  usually enough to expose a bug, and the interleaving space is small enough that a randomized
  scheduler covers a meaningful fraction of it. Bigger tests dilute every execution.
- **Seed state deterministically.** Build the state inside the closure with fixed values; get any
  randomness from `shuttle::rand` so it is replayable.

Here is that shape catching a real bug: the read-modify-write is split across two critical sections,
so two threads can both read the same value and one increment is lost. `check_dfs` explores
interleavings exhaustively, so for a test this small the failure is found deterministically:

```rust,should_panic
# extern crate shuttle;
use shuttle::sync::{Arc, Mutex};
use shuttle::thread;

shuttle::check_dfs(
    || {
        let counter = Arc::new(Mutex::new(0usize));

        let threads: Vec<_> = (0..2)
            .map(|_| {
                let counter = Arc::clone(&counter);
                thread::spawn(move || {
                    let value = *counter.lock().unwrap();
                    *counter.lock().unwrap() = value + 1;
                })
            })
            .collect();

        for thread in threads {
            thread.join().unwrap();
        }

        assert_eq!(*counter.lock().unwrap(), 2);
    },
    None,
);
```

## Organizing tests in practice

Keeping a `std` version of each Shuttle test is worth it. The `std` version runs in your normal test
job and catches compile errors and plain logic bugs; the Shuttle version runs under
`--features shuttle` and explores interleavings. If the workload and the invariant live in a helper
function written against `crate::sync`, both versions call the same code and cannot drift apart:

```rust,ignore
// tests/concurrency/mod.rs
pub fn concurrent_increment(threads: usize) {
    // uses crate::sync, so it compiles against either std or Shuttle
}

#[test]
fn concurrent_increment_std() {
    concurrent_increment(3);
}

#[cfg(feature = "shuttle")]
#[test]
fn concurrent_increment_shuttle() {
    shuttle::check_random(|| concurrent_increment(3), 1000);
}
```

A few conventions that pay off:

- Name the pair after the workload with a `_shuttle` suffix on the Shuttle one, so it is obvious
  which job a failure came from.
- One `#[test]` per scheduler configuration. The same body under `check_random` and under
  `check_pct` finds different bugs, and separate tests keep the iteration counts independently
  tunable.
- Put schedule files from failures next to the test and load them with
  `shuttle::replay_from_file` so a regression stays checked.

From here: [Schedulers and check functions](./schedulers.md) for choosing between `check_random`,
`check_pct`, `check_dfs`, and friends; [Configuring test runs](./configuration.md) for `Config`,
`Runner`, step bounds, and time limits; [Debugging failures: schedules and
replay](./debugging.md) for what to do when a test does fail; and [Performance and continuous
integration](./ci-and-performance.md) for picking iteration counts that fit a CI budget.
