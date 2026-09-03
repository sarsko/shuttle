# Introduction

Shuttle is a library for testing concurrent Rust code. You write an ordinary `#[test]`, hand the
body to Shuttle, and Shuttle runs it many times while controlling exactly which thread or task gets
to make progress at each step. When a run fails, Shuttle prints a schedule string that replays that
exact interleaving, so the failure becomes a deterministic test case you can put in a debugger.

## The problem with concurrency tests

A concurrency bug is a bug that only shows up in some orderings of concurrent operations. Consider
this code:

```rust,no_run
use std::sync::{Arc, Mutex};
use std::thread;

let lock = Arc::new(Mutex::new(0u64));
let lock2 = lock.clone();

thread::spawn(move || {
    *lock.lock().unwrap() = 1;
});

assert_eq!(0, *lock2.lock().unwrap());
```

There is an obvious race here: if the spawned thread acquires the lock first, the assertion fails.
But writing a unit test that *finds* that execution is another matter. Under normal execution, the
OS scheduler almost always lets the main thread reach the assertion before the spawned thread has
even started, so the test passes. The usual workaround is a stress test — run the body in a loop
thousands of times and hope one iteration gets unlucky. That approach has three problems:

- **It finds bugs by luck.** Whether an interleaving occurs depends on the OS scheduler, machine
  load, core count, and optimization level. A test that fails on your laptop may never fail in CI.
- **Failures are not reproducible.** You get a stack trace and no way to get back to the same
  execution, which makes debugging with a breakpoint nearly impossible.
- **You cannot tell when you have fixed it.** After a change, all you can do is run the loop again
  and wait, and a passing run tells you very little.

## Shuttle's answer: control the scheduler

Shuttle replaces the concurrency primitives your code uses — `std::thread`, `std::sync`, atomics,
channels, async executors — with its own implementations that hand control back to a *scheduler* at
every point where a real program could have switched threads. Shuttle then runs your test body many
times, and each time the scheduler makes different, randomized choices about which task runs next.

The Shuttle version of the test above wraps the body in
[`check_random`](https://docs.rs/shuttle/latest/shuttle/fn.check_random.html) and imports the
primitives from `shuttle` instead of `std`:

```rust,should_panic
# extern crate shuttle;
use shuttle::sync::{Arc, Mutex};
use shuttle::thread;

shuttle::check_random(|| {
    let lock = Arc::new(Mutex::new(0u64));
    let lock2 = lock.clone();

    thread::spawn(move || {
        *lock.lock().unwrap() = 1;
    });

    assert_eq!(0, *lock2.lock().unwrap());
}, 100);
```

The second argument is the number of executions to run. Each execution explores a different
interleaving, and the test fails if any of them fails. With 100 executions, this test finds the
assertion failure with probability over 99.9999% — no stress loop, no luck.

The other half of the story is reproducibility. Because the scheduler makes every choice, an
execution is fully described by the sequence of choices it made, and Shuttle prints that sequence
when the test fails:

```text
test panicked in task 'main-thread'
failing schedule:
"
910106fd8194f7f9e9d9df14a002
"
pass that string to `shuttle::replay` to replay the failure
```

Passing that string to [`replay`](https://docs.rs/shuttle/latest/shuttle/fn.replay.html) runs the
test body exactly once and reproduces the same execution, every time. That is the workflow Shuttle
is built around: randomize to find the bug, replay to fix it.
[Getting started](./getting-started.md) walks through this end to end, and
[Debugging failures](./debugging.md) covers schedules and replay in depth.

## Randomized, not exhaustive

Shuttle is heavily inspired by [Loom](https://github.com/tokio-rs/loom), which explores schedules
*exhaustively*: if a Loom test passes, no interleaving it considered can fail. Shuttle instead
samples schedules randomly. This is a deliberate soundness–scalability trade-off:

- Shuttle is **not sound**. A passing Shuttle test is evidence that your code is correct, not proof.
  Increasing the number of executions increases your confidence but never reaches certainty.
- Shuttle **scales**. Exhaustive search is exponential in the number of scheduling points, so it is
  only tractable for very small tests. Randomized search costs whatever you choose to spend on it,
  which means you can point Shuttle at integration-sized tests that spawn several threads and do
  real work.

Empirically this trades away very little, because concurrency bugs tend not to be adversarial: most
of them are triggered by a small number of preemptions at the wrong moment. Shuttle exploits that
with the [PCT scheduler](./schedulers.md), which biases towards schedules with few preemptions.
Shuttle does also ship an exhaustive depth-first scheduler for the cases where you want it — see
[Schedulers and check functions](./schedulers.md).

## What Shuttle does and does not do

Shuttle explores:

- **Thread and task interleavings**, including anything that blocks: mutexes, rwlocks, condvars,
  barriers, `Once`, channels, atomics, `thread::yield_now`, and async tasks awaiting each other.
- **Data nondeterminism**, via `shuttle::rand`, a drop-in replacement for the `rand` crate whose
  values are chosen and recorded by Shuttle so they replay along with the schedule. This is what
  makes it possible to write a randomized stress test that is still reproducible.

Shuttle deliberately does not do:

- **Weak or relaxed memory behavior.** Shuttle models *all* atomic operations as if they used
  `SeqCst` ordering, so a bug that requires a relaxed load to observe a stale value is invisible to
  it. Loom has support for reasoning about `Acquire`/`Release` orderings if you need that.
- **Real parallelism.** Shuttle runs exactly one task at a time on a single OS thread, resuming
  tasks as continuations. This is what makes schedules reproducible, and it means Shuttle tests
  cannot find bugs that only appear with genuine physical parallelism — but it also means Shuttle
  can preempt at points a real machine rarely would.
- **Concurrency it cannot see.** If your code calls `std::sync::Mutex` or `std::thread::spawn`
  directly, Shuttle has no idea those operations happened, gets no scheduling point, and will not
  explore interleavings around them. The same applies to nondeterminism Shuttle does not control,
  such as `std::time::Instant`, real I/O, address-dependent hashing, or `rand` from the real crate:
  it silently breaks replay. See [Determinism rules and common pitfalls](./pitfalls.md), and
  [`check_uncontrolled_nondeterminism`](https://docs.rs/shuttle/latest/shuttle/fn.check_uncontrolled_nondeterminism.html)
  for a way to detect the latter class.

## How Shuttle compares

|  | Technique | Soundness | Scale | Typically finds | Cost |
|---|---|---|---|---|---|
| **Shuttle** | Randomized scheduling of your primitives | Unsound; a pass is evidence | Large tests, many threads | Races, deadlocks, lost wakeups, ordering assumptions | You choose: executions × test cost |
| **Loom** | Exhaustive scheduling, with a memory model | Sound over what it explores | Small tests only | The above, plus relaxed-atomics bugs | Grows quickly with test size |
| **Miri** | Interpreting your program; can run a test under many seeds | Unsound for concurrency; sound for the UB it detects | Small to medium | Undefined behavior, data races, leaks | Interpretation is much slower than native |
| **Kani** | Bounded model checking / proof | Proof within its bounds | Small, focused properties | Panics, assertion violations, arithmetic overflow | Verification time; needs harnesses and bounds |
| **Stress test** | Run it a lot on the real scheduler | Unsound | Anything you can run | Whatever the OS happens to schedule | Cheap to write, expensive to debug |

These tools are complementary rather than competing. A common setup is Shuttle for behavioral
concurrency testing, Miri for undefined behavior in `unsafe` code, and Loom for the handful of
primitives that depend on relaxed atomics.

## When to reach for Shuttle

Shuttle earns its keep when correctness depends on ordering:

- **Lock-free and custom synchronization primitives** — your own queue, semaphore, refcount, or
  work-stealing structure, where the bug is a missed wakeup or a window between two atomics.
- **Async state machines** — futures that hold locks across `await` points, cancellation and drop
  ordering, tasks that must not deadlock against each other. See [Async code and futures](./async.md).
- **Integration-style stress tests** — several threads driving a component through a random sequence
  of operations. Shuttle both broadens the interleavings you cover and makes the failures replayable.
- **Regression tests for concurrency bugs you have already fixed** — a saved schedule is a fast,
  deterministic test.

It is a poor fit when concurrency is not where the risk lies: single-threaded logic (use ordinary
tests or `proptest`), correctness that hinges on relaxed atomics (Loom), memory-safety of `unsafe`
code (Miri), or code whose concurrency comes from processes, the network, or real timing rather than
threads. Shuttle also cannot help with code you cannot get to use its primitives — if a
dependency hard-codes `std::sync`, see [Third-party crates and wrappers](./wrappers.md).

## How to read this book

The chapters are ordered so you can read straight through, but each stands alone:

- [Installation and your first test](./getting-started.md) — add Shuttle to a crate and take a
  failing test from discovery to replay.
- [Writing Shuttle tests](./writing-tests.md) — structuring code so the same source builds against
  `std` and Shuttle, and the shape of a good Shuttle test.
- [Schedulers and check functions](./schedulers.md) — `check_random`, `check_pct`, `check_dfs` and
  friends, what each explores, and which to pick.
- [Configuring test runs](./configuration.md) — `Config`, step bounds, timeouts, failure
  persistence, and the `SHUTTLE_*` environment variables.
- [Debugging failures: schedules and replay](./debugging.md) — reading schedule strings, replaying
  from a string or file, and the tracing output.
- [Async code and futures](./async.md) — Shuttle's executor, `spawn` and `block_on`, and what
  changes when your concurrency is tasks rather than threads.
- [Third-party crates and wrappers](./wrappers.md) — the wrapper crates for `tokio`, `rand`,
  `parking_lot`, `dashmap`, and others, and how to write your own.
- [Annotations and Shuttle Explorer](./explorer.md) — the `annotation` feature and the VS Code
  extension for visualizing a failing schedule.
- [How Shuttle works](./internals.md) — continuations, tasks, scheduling points, vector clocks, and
  how a schedule is encoded.
- [Determinism rules and common pitfalls](./pitfalls.md) — the rules your test body must follow, and
  the mistakes that make Shuttle miss bugs or fail to replay.
- [Performance and continuous integration](./ci-and-performance.md) — making Shuttle tests fast
  enough to run on every commit, and how to wire them into CI.

Full API documentation lives at [docs.rs/shuttle](https://docs.rs/shuttle).
