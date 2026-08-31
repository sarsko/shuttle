# Installation and your first test

Shuttle finds concurrency bugs by taking over the scheduler. Instead of letting the operating system
decide when your threads run, Shuttle runs all of them on a single OS thread and picks which one
takes the next step. That gives it two things a normal stress test cannot have: it can steer the
execution towards interleavings you would rarely hit by chance, and it can *record* the choices it
made so that a failing execution can be replayed exactly.

## Adding Shuttle to your project

Shuttle is only needed when compiling tests, so it belongs in `[dev-dependencies]`:

```toml
[dev-dependencies]
shuttle = "0.9.3"
```

Shuttle is doing real work on every step of your program — bookkeeping which tasks are runnable,
switching between them, and recording the schedule — so debug builds are slow. Run Shuttle tests in
release mode:

```sh
cargo test --release
```

Shuttle's own CI runs `cargo nextest run --release --workspace` for exactly this reason. See
[Performance and continuous integration](./ci-and-performance.md) for more on keeping Shuttle tests
fast enough to run on every commit.

## Your first Shuttle test

A Shuttle test is an ordinary `#[test]` function. The concurrent code under test goes into a
closure, and the closure is handed to one of Shuttle's *check* functions, which runs it many times
under different schedules.

Start with something that is actually correct: two threads each incrementing a counter, holding the
lock for the whole read-modify-write.

```rust
# extern crate shuttle;
use shuttle::sync::{Arc, Mutex};
use shuttle::thread;

shuttle::check_random(
    || {
        let counter = Arc::new(Mutex::new(0u64));

        let handles: Vec<_> = (0..2)
            .map(|_| {
                let counter = counter.clone();
                thread::spawn(move || {
                    *counter.lock().unwrap() += 1;
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        assert_eq!(*counter.lock().unwrap(), 2);
    },
    100,
);
```

Two things are different from a plain Rust test:

- The imports come from `shuttle::sync` and `shuttle::thread` instead of `std::sync` and
  `std::thread`. These are drop-in replacements that report every lock acquisition, every spawn, and
  every join to Shuttle's scheduler. Code that uses `std`'s primitives is invisible to Shuttle, and
  a `std::thread` spawned inside a Shuttle test really does create an OS thread outside Shuttle's
  control. Swapping the imports is the whole trick; [Writing Shuttle
  tests](./writing-tests.md) shows how to do it in a real crate without forking your source.
- The body is wrapped in [`shuttle::check_random`], which runs it under a randomized scheduler.

The second argument, `100`, is the number of *iterations*: how many times Shuttle runs the closure,
each time making fresh random scheduling decisions. Every iteration starts from scratch, so all
shared state must be created *inside* the closure — that is also why the closure is required to be
`Fn`, not `FnMut`. If you build the `Arc` outside, later iterations will see whatever the earlier
ones left behind.

This test passes. Now break it.

## Finding a race

Here is the same shape of program, but the assertion assumes the spawned thread has not run yet:

```rust,should_panic
# extern crate shuttle;
use shuttle::sync::{Arc, Mutex};
use shuttle::thread;

shuttle::check_random(
    || {
        let lock = Arc::new(Mutex::new(0u64));
        let lock2 = lock.clone();

        thread::spawn(move || {
            *lock.lock().unwrap() = 1;
        });

        assert_eq!(0, *lock2.lock().unwrap());
    },
    100,
);
```

The bug is obvious once you see it: nothing orders the spawned thread's write against the main
thread's read. Under the real scheduler this test would pass almost every time, because the parent
thread usually keeps running after a spawn. Under Shuttle, the spawn is a scheduling decision point,
and about half the time the scheduler picks the child.

That is what the iteration count buys you. If a single iteration has probability `p` of hitting the
bug, then `n` iterations find it with probability `1 - (1 - p)^n`. Here `p` is roughly one half, so
100 iterations find the failure with probability over 99.9999%. Shuttle is not proving anything —
a passing Shuttle test does not mean your code is correct — but for bugs that need only a couple of
unlucky choices, "near-certainly" is close enough to be useful in CI.

Raising the iteration count raises the probability at linear cost. Lowering it makes the test
cheaper and flakier. For bugs that need a very specific interleaving, changing the *scheduler*
rather than the iteration count is usually the better move; see [Schedulers and check
functions](./schedulers.md).

## Reading the failure output

When an iteration panics, Shuttle prints the information needed to reproduce it before letting the
panic continue on its way:

```text
Task failed, serializing schedule
test panicked in task 'main-thread'
failing schedule:
"
910106fd8194f7f9e9d9df14a002
"
pass that string to `shuttle::replay` to replay the failure

thread 'racy_read' panicked at tests/counter.rs:14:9:
assertion `left == right` failed
  left: 0
 right: 1
test panicked in task 'main-thread'
failing seed:
"
1495027192976179453
"
To replay the failure, either:
    1) pass the seed to `shuttle::check_random_with_seed, or
    2) set the environment variable SHUTTLE_RANDOM_SEED to the seed and run `shuttle::check_random`.
```

Reading it from the top:

- `main-thread` is the task that panicked. That is the fixed name Shuttle gives the closure you
  passed to `check_random`. Tasks you spawn without naming them are reported by id instead —
  `task-1`, `task-2` — so naming threads through [`shuttle::thread::Builder`] pays off as soon as
  a failure involves more than one of them.
- The quoted hex blob is the *schedule*: the complete sequence of scheduling decisions, plus any
  random values Shuttle handed out, for the iteration that failed. It is what makes the failure
  reproducible. Long schedules are wrapped across several lines between the quotes; whitespace
  inside them is ignored, so you can copy the whole block.
- In between is the ordinary Rust panic message, from your assertion.
- The `failing seed` is a second, coarser reproduction handle: it re-runs the whole randomized
  search rather than one execution, and is only printed by the random scheduler. Prefer the
  schedule; reach for the seed when the failure comes out of a search you want to repeat wholesale.

Two details are worth knowing before they confuse you: Shuttle keeps scheduling other tasks while the
panicking task unwinds, so the `test panicked in task ...` message and the schedule can appear more
than once in a single failure — the strings differ by the steps taken during unwinding, and any of
them replays the bug; and only the first failing iteration is reported, since `check_random` stops
there rather than continuing through the remaining iterations.

By default Shuttle prints the schedule to stderr. If your schedules are long enough to be awkward
to copy, Shuttle can write them to files instead, in which case the output points at
[`shuttle::replay_from_file`] rather than [`shuttle::replay`]. See [Configuring test
runs](./configuration.md).

## Replaying the failure

Copy the schedule string into [`shuttle::replay`], with the same test body:

```rust,no_run
# extern crate shuttle;
use shuttle::sync::{Arc, Mutex};
use shuttle::thread;

shuttle::replay(
    || {
        let lock = Arc::new(Mutex::new(0u64));
        let lock2 = lock.clone();

        thread::spawn(move || {
            *lock.lock().unwrap() = 1;
        });

        assert_eq!(0, *lock2.lock().unwrap());
    },
    // Paste the schedule string from your own failure output here. This one is
    // the schedule printed above, and reproduces that exact interleaving.
    "910106fd8194f7f9e9d9df14a002",
);
```

`replay` runs the body *exactly once*, following the recorded decisions instead of making new ones,
and reproduces the same failure every time. This is the run you attach a debugger to: a single
execution on a single OS thread, so breakpoints fire where you expect, `println!` output appears in a
stable order, and you can add instrumentation without changing whether the bug reproduces. The usual
workflow is to keep the `check_random` test in your suite and edit a scratch `replay` test while you
investigate. [Debugging failures: schedules and replay](./debugging.md) covers persisting schedules
to files, capturing task backtraces, and getting a readable `tracing` log of the execution.

## A more realistic bug: check-then-act

Races that survive code review usually do not look like the example above. They look like this: a
worker checks the state of a shared collection, and then acts on it — correctly, under a lock, both
times — without holding the lock across both.

```rust,should_panic
# extern crate shuttle;
use shuttle::sync::{Arc, Mutex};
use shuttle::thread;

shuttle::check_random(
    || {
        let queue = Arc::new(Mutex::new(vec![42u64]));

        let handles: Vec<_> = (0..2)
            .map(|_| {
                let queue = queue.clone();
                thread::spawn(move || {
                    if queue.lock().unwrap().is_empty() {
                        return None;
                    }
                    // The lock was released at the end of the `if` condition, so the
                    // other worker may have drained the queue by the time we get here.
                    Some(queue.lock().unwrap().pop().unwrap())
                })
            })
            .collect();

        let popped = handles.into_iter().filter_map(|h| h.join().unwrap()).count();
        assert_eq!(popped, 1);
    },
    100,
);
```

Both workers can observe a non-empty queue, and then both try to pop from it. The loser gets `None`
and the `unwrap` panics inside a spawned task, so the reported task will be `task-1` or `task-2`
rather than `main-thread`. Note that the two lock acquisitions are individually fine; nothing here is a
data race in the Rust sense, and the compiler is perfectly happy. The invariant that breaks is
"non-empty implies I can pop", which only holds if the lock is held across both operations.

The fix is to make the check and the act a single critical section — here, by letting `pop` be the
check:

```rust
# extern crate shuttle;
use shuttle::sync::{Arc, Mutex};
use shuttle::thread;

shuttle::check_random(
    || {
        let queue = Arc::new(Mutex::new(vec![42u64]));

        let handles: Vec<_> = (0..2)
            .map(|_| {
                let queue = queue.clone();
                thread::spawn(move || queue.lock().unwrap().pop())
            })
            .collect();

        let popped = handles.into_iter().filter_map(|h| h.join().unwrap()).count();
        assert_eq!(popped, 1);
        assert!(queue.lock().unwrap().is_empty());
    },
    100,
);
```

This version passes all 100 iterations, and if you still have the failing schedule you can run it
through `replay` to confirm that the exact execution which used to break is now fine.

## Where to go next

- [Writing Shuttle tests](./writing-tests.md) — wiring Shuttle into an existing crate behind a
  `cfg` or feature flag, so production builds keep using `std` and only tests use Shuttle.
- [Schedulers and check functions](./schedulers.md) — when to reach for `check_pct` instead of
  `check_random`, and when exhaustive `check_dfs` is tractable.
- [Determinism rules and common pitfalls](./pitfalls.md) — replay only works if the test body is
  deterministic apart from the choices Shuttle controls. Wall-clock time, real thread IDs, hashing
  by address, and `std` synchronization primitives all break that property.
- [Async code and futures](./async.md) and [Third-party crates and
  wrappers](./wrappers.md) — testing `async` code and code that depends on crates like `tokio`.

[`shuttle::check_random`]: https://docs.rs/shuttle/latest/shuttle/fn.check_random.html
[`shuttle::replay`]: https://docs.rs/shuttle/latest/shuttle/fn.replay.html
[`shuttle::replay_from_file`]: https://docs.rs/shuttle/latest/shuttle/fn.replay_from_file.html
[`shuttle::thread::Builder`]: https://docs.rs/shuttle/latest/shuttle/thread/struct.Builder.html
