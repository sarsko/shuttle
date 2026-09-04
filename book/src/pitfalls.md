# Determinism rules and common pitfalls

Almost everything that goes wrong with a Shuttle test is a violation of one of two rules. (If you have not
run a Shuttle test yet, start with [your first test](./getting-started.md).)

**Rule 1: all concurrency must go through Shuttle's primitives.** Shuttle only knows a switch is
possible where your code calls into one. A `std::sync::Mutex`, a raw `std::sync::atomic` operation, or
an OS thread is invisible to the scheduler, so Shuttle neither explores the interleavings around it
nor records them in the schedule.

**Rule 2: an execution must be a pure function of its schedule.** Replay is nothing more than making
the same scheduling and random choices again in the same order. If the body also reads the clock, an
OS thread id, a pointer address, or an uncontrolled RNG, the same schedule can produce a different
execution, and the schedule string that "reproduces" your failure will not.

[How Shuttle works](./internals.md) explains the mechanism behind both: tasks are continuations on a
single OS thread, and a schedule is the sequence of choices made at each scheduling point. What
follows is a catalog of ways those rules get broken, by symptom.

## A `std` primitive leaked into the test

**Symptom:** the test passes far too easily, or it hangs forever with no output.

Shuttle's primitives are drop-in replacements, so a stray `use std::sync::...` still compiles; what
happens next depends on which primitive it was.

**Atomics, `Cell`, and `UnsafeCell` fail silently.** They never block, so the test runs to completion
— but with no scheduling point between the operations, Shuttle treats a whole read-modify-write as one
atomic step. This test passes every time, on any scheduler:

```rust
# extern crate shuttle;
# use shuttle::sync::Arc;
# use shuttle::thread;
use std::sync::atomic::{AtomicUsize, Ordering}; // std, not shuttle

// No scheduling point between the load and the store, so this is one atomic step.
fn bump(c: &AtomicUsize) {
    let v = c.load(Ordering::SeqCst);
    c.store(v + 1, Ordering::SeqCst);
}

shuttle::check_random(
    || {
        let counter = Arc::new(AtomicUsize::new(0));
        let c = counter.clone();
        let handle = thread::spawn(move || bump(&c));
        bump(&counter);
        handle.join().unwrap();
        assert_eq!(counter.load(Ordering::SeqCst), 2); // never fails
    },
    100,
);
```

Change one path — `std::sync::atomic` to `shuttle::sync::atomic` — and every load and store becomes a
scheduling point, so the first schedule that interleaves the two `bump` calls fails the assertion.
Shuttle's own test suite exploits this asymmetry deliberately, using a `std` `AtomicUsize` to smuggle
a counter across tasks *without* perturbing the interleavings under test.

**Locks and channels can hang the whole test.** Every Shuttle task is a continuation on one OS thread.
A `std::sync::Mutex` held across a scheduling point blocks that thread when another task tries to
acquire it, and nothing will ever unblock it: no deadlock report, no schedule string, no timeout, the
binary just stops. If a Shuttle test hangs with no output at all, grep the code under test for `std::sync`
and `parking_lot` first.

**Fix:** route every primitive through one import site so a feature flag swaps the whole set at once, as
described in [Writing Shuttle tests](./writing-tests.md).

## `ExecutionState is not set`

**Symptom:**

```text
`ExecutionState::with` panicked because `ExecutionState` is not set. Are you accessing a Shuttle primitive outside of a Shuttle test?
```

Shuttle keeps its execution state in a thread-local of the one thread running the test, so this means a
Shuttle primitive was touched somewhere that thread is not: a `std::thread::spawn` in the body, a real
`tokio` runtime or `rayon` pool created inside it, a `Drop` impl or background worker that outlives the
execution, or a plain `#[test]` that forgot its check function. Real threads and runtimes cannot be made to
work here even if you keep Shuttle types out of them: they run outside the scheduler, so their interleavings
are neither explored nor replayable. Use `shuttle::thread`, `shuttle::future::spawn`, or a wrapper crate —
see [Third-party crates and wrappers](./wrappers.md) for `shuttle-tokio`, `shuttle-parking_lot`,
`shuttle-dashmap` and friends, and [Async code and futures](./async.md) for Shuttle's executor.

## Ambient nondeterminism

**Symptom:** the failure is real, but the schedule string reproduces nothing.

Anything the body reads that is not derived from the schedule breaks Rule 2:

| Source | Problem | Fix |
|---|---|---|
| `rand::thread_rng` | an RNG Shuttle does not control | `shuttle::rand::thread_rng`, or the `shuttle-rand` wrapper |
| `Instant::now`, `SystemTime::now` | changes every run | derive the value, or `cfg` it out under Shuttle |
| `std::thread::current().id()`, `process::id()` | all Shuttle tasks share one OS thread | `shuttle::thread::current().id()`, derived from the `TaskId` |
| pointer addresses | nothing guarantees the same allocation twice | key on an id you assign |
| `env::var`, files, sockets, DNS | outside the test's control entirely | stub it |
| `HashMap`/`HashSet` iteration | random per-process hash seed | see below |

`shuttle::rand` is a drop-in replacement for `rand` v0.8 whose `thread_rng()` draws from the scheduler's
data source, so draws are recorded in the schedule and replayed; unlike the real `rand`, its `ThreadRng`
is shared by all tasks and cannot be re-seeded. Time is not modelled at all: `shuttle::thread::sleep`
ignores its duration and is exactly a context switch, and `shuttle-tokio`'s `sleep` likewise yields
rather than waiting. Code that *branches* on elapsed time is therefore both nondeterministic and
untested — `shuttle-tokio` emits a tracing warning if you ask an `Interval` for its period:

```text
`period` suggests code dependent on real time, which means that it will behave
nondeterministically under Shuttle, meaning that failing schedules would not be replayable.
The suggested solution is to have some different handling under Shuttle, by using `cfg(feature = "shuttle")`.
```

## `HashMap` and `HashSet` iteration order

**Symptom:** replay fails, or the test fails on some runs of the binary and never on others.

`std`'s hash collections seed their hasher randomly per process, so iteration order differs between runs
of the test binary. If that order decides which task you spawn, which lock you take, or how many random
numbers you draw, the execution is no longer a function of the schedule. Shuttle's own test suite contains
exactly this case, and the nondeterminism checker below catches it.

**Fix:** iterate a `BTreeMap`/`BTreeSet` or a sorted `Vec` of keys, or use the workspace's
`determinizable_collections` crate, which re-exports `std`'s `HashMap` and `HashSet` by default and swaps
in fixed-seed versions under its `deterministic` feature:

```toml
[features]
shuttle = ["determinizable_collections/deterministic"]

[dependencies]
determinizable_collections = "0.1.0"
```

Deterministic hashing gives up HashDoS resistance, so gate it behind the feature.

## Finding the source: `check_uncontrolled_nondeterminism`

When you suspect Rule 2 is broken but do not know where, run the body under
`shuttle::check_uncontrolled_nondeterminism(f, iterations)`. It generates a random schedule, then
immediately *replays* it and checks that the second run asks the scheduler the same questions: the same
chosen task, the same set of runnable tasks, the same `is_yielding` flag, and random draws at the same
positions. Any divergence panics with a message starting `possible nondeterminism` — `set of runnable
tasks is different than expected`, followed by the two task lists, or one of:

```text
possible nondeterminism: current execution ended earlier than expected (expected length 42 but ended after 17)

possible nondeterminism: next step was context switch, but recording expected random number generation
```

The counts are schedule positions, which you can line up against the tracing output (see
[Debugging failures](./debugging.md)) to find where the runs diverged. Two caveats: it is a bug finder,
not a proof, and it only detects nondeterminism that changes what the scheduler is *asked* — a test that
reads the wall clock into a value it never synchronizes on sails straight through and still fails to
replay.

## Spin loops and busy waiting

**Symptom:** an iteration never finishes, or the test fails with:

```text
exceeded max_steps bound 1000000. this might be caused by an unfair schedule (e.g., a spin loop)?
```

Shuttle's schedulers are not fair. Nothing obliges the scheduler to ever pick the task you are waiting
for, so a wait loop can spin arbitrarily long. Two distinct outcomes:

- **The loop body contains a Shuttle operation** (a `shuttle::sync` atomic load, say). Each iteration
  is a step, the default `MaxSteps::FailAfter(1_000_000)` eventually trips, and you get the message
  above. That bound is a livelock detector, not a fix.
- **The loop body contains no Shuttle operation at all.** The step bound is only checked when the
  scheduler is consulted, so it never fires: the iteration spins forever and the test hangs.

**Fix:** put a scheduling point in the loop that also signals you are making no progress. Replace the body
of a `while !flag.load(..) {}` wait loop with `shuttle::hint::spin_loop()`, which emits the real
`std::hint::spin_loop` hint and then yields to the scheduler; `shuttle::thread::yield_now()` does the same
without the hint. Fairness-aware schedulers use that yield as a signal — PCT deprioritizes a yielding task,
which is what lets its peers make progress and lets the loop terminate.

The same loop with `thread::sleep` in place of the yield still hits the step bound under PCT, because
sleeping is not a yield. If the body is legitimately long, `MaxSteps::ContinueAfter(n)` abandons an
over-long iteration and starts the next instead of failing, and `shuttle::current::reset_step_count()`
zeroes the counter when you know real progress was made. See
[Configuring test runs](./configuration.md).

## Tests that pass vacuously

**Symptom:** a green Shuttle test that later turns out to have had the bug all along. Shuttle is unsound
by design; a pass is evidence, not proof. The ways a test proves nothing:

- **The assertion is unreachable in most schedules.** If the interesting check sits after a `join` that
  serializes everything, or in a branch needing a specific interleaving to enter, most iterations never
  evaluate it. Assert invariants where they must hold, not only at the end.
- **Too few iterations.** `check_random(f, 10)` on a body with hundreds of interleavings is a coin flip.
- **The bug needs more preemptions than the scheduler explores.** `check_pct(f, iterations, depth)`
  bounds preemptions at `depth`; a bug needing three well-placed preemptions will not be found at
  `depth = 1`. Vary the depth, run a portfolio, and use `check_dfs` on the smallest primitives. See
  [Schedulers and check functions](./schedulers.md).
- **Nothing in the body is a scheduling point** — see the `std` atomics case above.

## A deadlock report that is really a test bug

**Symptom:**

```text
deadlock! blocked tasks: [main-thread (task TaskId(0)), <unknown> (task TaskId(1))]
```

Shuttle reports a deadlock when no task can be scheduled but some unfinished, non-detached task
remains. Often that is the bug. Just as often it is the harness:

- **A sender you meant to drop.** A `Receiver` blocks while any `Sender` is alive, so keeping the
  original `tx` in scope after cloning it for the workers makes the final `recv()` block forever.
  (Dropping *all* senders is fine: `recv` then returns `Err(RecvError)`.)

  ```rust,should_panic
  # extern crate shuttle;
  # use shuttle::sync::mpsc::channel;
  # use shuttle::thread;
  shuttle::check_dfs(|| {
      let (tx, rx) = channel::<u32>();
      let tx2 = tx.clone();
      thread::spawn(move || tx2.send(1).unwrap());
      rx.recv().unwrap();
      rx.recv().unwrap(); // `tx` is still alive, so this blocks forever
  }, None);
  ```

- **A bounded or rendezvous channel with no reader.** `sync_channel(0)` blocks the sender until someone
  receives; `sync_channel(n)` blocks on the `n+1`th send.
- **A condvar predicate that is never true**, or one `notify_one` for two waiters. Note that Shuttle's
  `Condvar::wait` does *not* model spurious wakeups, so a predicate checked with `if` instead of
  `while` is a bug Shuttle will not find. `thread::park` does allow spurious wakeups.
- **A task blocked on a lock the panicking task still holds** — see the next section.

Set `SHUTTLE_CAPTURE_BACKTRACE` for a backtrace of every blocked task in the panic message; Shuttle says so
when it is unset:

```text
Test deadlocked, and SHUTTLE_CAPTURE_BACKTRACE is not set. If either of those are set then the backtrace of each task will be collected and printed as part of the panic message.
```

Note that *not* joining a `shuttle::thread` handle does not deadlock: threads stay attached and Shuttle runs
them to completion. What you get instead is an assertion that runs before the work is done — a real failure,
but not the one you were testing for.

## Panics and unwinding inside tasks

**Symptom:** a schedule string appears in the output for a panic your test deliberately caught.

Shuttle installs a global panic hook, so *every* panic in the process prints `Task failed, serializing
schedule` and the failing schedule — including a panic you wrap in `catch_unwind` and handle. That
output is informational, not necessarily a test failure.

When a panic is *not* caught, Shuttle serializes the schedule and then, by default, keeps scheduling
until the panicking task has finished unwinding before resuming the unwind on the main thread. Drop
handlers therefore run, which is usually what you want — but a second panic during unwinding aborts the
process, taking the schedule with it. `Config::ungraceful_shutdown_config` offers
`immediately_return_on_panic` to stop scheduling as soon as a task panics, narrowing that window at the
cost of skipping the rest of the unwind.

Shuttle's `Mutex` and `RwLock` poison exactly like `std`'s: a panic while holding the guard poisons that
lock and only that lock, later `lock()` calls return `Err(PoisonError { .. })`, and `clear_poison` is
available. `Once` poisons too, and `call_once_force` ignores it. So a test that panics inside a critical
section typically produces a *second*, confusing failure — an `unwrap()` on a `PoisonError` in whichever
task touches the lock next.

## Thread-locals and statics that carry state across executions

**Symptom:** iteration 1 passes, iteration 2 fails. Or the failure moves when you change the iteration
count. Or replaying the schedule passes, because replay only runs one iteration.

Every iteration runs on the same OS thread, in a loop. Anything anchored to the OS thread or the
process rather than to the execution survives into the next iteration: `std::thread_local!` (one slot
shared by all tasks *and* all iterations), `lazy_static::lazy_static!`, `once_cell`,
`std::sync::OnceLock`, `static mut`, and any `static` counter, cache, or registry. The classic version,
which fails on the second iteration:

```rust,should_panic
# extern crate shuttle;
# use std::cell::RefCell;
std::thread_local! {
    static SEEN: RefCell<Vec<u32>> = RefCell::new(Vec::new());
}

shuttle::check_random(|| {
    SEEN.with(|s| s.borrow_mut().push(1));
    assert_eq!(SEEN.with(|s| s.borrow().len()), 1);
}, 10);
```

**Fix:** swap `std::thread_local!` for `shuttle::thread_local!`. The declaration is otherwise identical,
but the value lives in per-task storage dropped when the task ends, so each execution — and each task —
starts fresh, and the test above passes. For lazy statics, `shuttle::lazy_static!` replaces the
`lazy_static` crate's macro (or use the `shuttle-lazy_static` wrapper to swap it by feature flag) and
re-initializes per execution. It differs from the real crate in one way, warned about once per process:

```text
WARNING: Shuttle runs the `Drop` method of `lazy_static` values at the end of an execution, unlike the actual `lazy_static` implementation. This difference may cause false positives.
```

Set `SHUTTLE_SILENCE_WARNINGS` or `Config::silence_warnings` to suppress that. There is no Shuttle
replacement for `OnceLock`/`OnceCell` statics: move the state into the test body and pass it in via an
`Arc`, or convert the static to `shuttle::lazy_static!`. If a global must persist, reset it explicitly at
the top of the body — and remember that anything surviving an iteration must not affect scheduling, or
you have broken Rule 2.

## The test body is too expensive

**Symptom:** the test takes minutes, or CI times out, and the concurrency logic in it is tiny. The body
runs once per iteration — hundreds or thousands of times — so everything expensive inside it is multiplied
by that count: file and network I/O, deserializing large fixtures, cryptography, allocation-heavy setup,
real sleeps. Debug builds make it worse, since Shuttle does per-step bookkeeping; run with `--release`.
Move setup that does not need re-testing outside the closure (capture it in an `Arc` and clone it in),
shrink fixtures to the smallest thing that exhibits the bug, and use `Config::max_time` to cap wall-clock
cost — noting that it is only checked *between* iterations and will not interrupt one already running. See
[Performance and continuous integration](./ci-and-performance.md).

## A schedule string that no longer reproduces

**Symptom:** a schedule that used to reproduce a failure now panics during replay:

```text
schedule ended early

scheduled task is not runnable, expected to run TaskId(2), but choices were [...]

expected context switch but next schedule step is random choice
```

A schedule is a sequence of task ids and random draws indexed by scheduling point, tied to the exact
structure of the code that produced it. Adding a lock acquisition, reordering two spawns, changing how
many random numbers you draw, or upgrading a wrapper crate that adds a scheduling point all shift every
subsequent step. There is no version negotiation beyond a one-byte format tag, so a stale schedule fails
loudly rather than silently replaying something else — which is the behavior you want. Treat schedule
strings as debugging artifacts with a short shelf life, valid against one revision of the code. If a
failing interleaving is worth keeping as a regression test, keep the *test*, not the schedule: shrink it
and run it under `check_dfs` so the interleaving is rediscovered every time.

## `unsafe`, raw atomics, and weak memory orderings

**Symptom:** Shuttle passes; the bug is real and ships anyway. These are hard limits, not tuning knobs.

- **Shuttle models every atomic operation as `SeqCst`.** Passing `Relaxed`, `Acquire`, or `Release` to a
  `shuttle::sync::atomic` operation changes nothing about how Shuttle explores it. A bug that requires one
  store to be reordered past another cannot be found, because Shuttle never reorders. It warns once per
  process on seeing a non-`SeqCst` ordering:

  ```text
  WARNING: Shuttle only correctly models SeqCst atomics and treats all other Orderings as if they were SeqCst. Bugs caused by weaker orderings like Relaxed may be missed.
  ```

  (`fence(Ordering::Relaxed)` panics with `there is no such thing as a relaxed fence`.) If your correctness
  argument rests on acquire/release pairs, test that primitive with [Loom](https://crates.io/crates/loom).
- **Shuttle is not a data race detector.** Reads and writes through an `UnsafeCell` or a raw pointer are
  not scheduling points and are not tracked. Shuttle will explore a program with a genuine data race and
  report nothing; use Miri for that.
- **`std::sync::atomic` used directly inside `unsafe` code is invisible**, with all the coverage
  consequences described at the top of this chapter. This is the most common reason a hand-written
  lock-free structure "passes" Shuttle.

## Pre-flight checklist

Before trusting a green Shuttle test:

1. **No `std` concurrency reaches the code under test** — grep for `std::sync`, `std::thread`,
   `parking_lot`, `tokio::`, `rayon`, `lazy_static`.
2. **No ambient nondeterminism** — no `rand::thread_rng`, `Instant::now`, OS thread ids, pointer-ordered
   iteration, environment or filesystem reads, order-sensitive `HashMap` iteration.
3. **`check_uncontrolled_nondeterminism` passes** on the body.
4. **The test can fail** — break the code on purpose and confirm Shuttle catches it.
5. **The assertions are reachable** in a typical iteration, not only a rare one.
6. **The iteration count is justified**, and you ran more than one scheduler or PCT depth.
7. **No unbounded spinning**: every wait loop yields, and nothing relies on the step bound to terminate.
8. **No state survives an iteration** — `shuttle::thread_local!`, `shuttle::lazy_static!`, globals reset.
9. **You read the warnings**, so you know which bugs the ordering and `lazy_static` caveats rule out.
10. **You know what this test does not cover** — weak memory, data races, real time — and something else
    does.
