# Fault injection and failure modeling

Almost every concurrency bug that survives to production lives on an error path. The happy path is
exercised by every test you write; the branch that runs when the third retry fails while another
thread holds a lock has never run under contention, and that is where lost updates, leaked guards
and deadlocks accumulate. Shuttle is good at this for a non-obvious reason: once "does this
operation fail?" is a value drawn from `shuttle::rand`, the failure becomes a *scheduling
decision*, recorded in the same schedule string as the interleaving. Injecting a fault and
exploring an interleaving are one problem solved by one mechanism, and when "this call failed *and*
the other task ran here" breaks an invariant you get a schedule that replays it. This chapter
assumes [Writing Shuttle tests](./writing-tests.md), [Async code and futures](./async.md), and
[replay](./debugging.md).

## Failures are scheduling decisions

The technique rests on one small helper: draw a boolean from Shuttle's RNG, turn it into an error.

```rust
# extern crate shuttle;
use shuttle::rand::{thread_rng, Rng};
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::sync::Arc;

/// Drawn from `shuttle::rand`, so the decision is part of the schedule and replays exactly.
fn maybe_fail(probability: f64) -> Result<(), &'static str> {
    if thread_rng().gen_bool(probability) { Err("injected") } else { Ok(()) }
}

let failures = Arc::new(AtomicUsize::new(0));
let f = Arc::clone(&failures);
shuttle::check_random(move || { if maybe_fail(0.5).is_err() { f.fetch_add(1, SeqCst); } }, 100);
let n = failures.load(SeqCst); // Shuttle explored failure *and* success.
assert!(n > 0 && n < 100, "saw {n} failures out of 100");
```

**It must be `shuttle::rand`, not `rand`.** The real `rand::thread_rng` is seeded from the OS, so
the failure pattern differs on every execution and the schedule reproduces nothing — the
ambient-nondeterminism failure mode in [Determinism rules and common pitfalls](./pitfalls.md). For
dependency code you cannot edit, the `shuttle-rand` wrapper makes the same substitution behind a
feature flag; see [Third-party crates and wrappers](./wrappers.md).

**`shuttle::rand` is a small surface.** It re-exports `Rng` and `RngCore` from `rand` 0.8 plus its
own `thread_rng()` and `rngs::ThreadRng`, giving you `gen_bool`, `gen_ratio`, `gen_range`, `gen`
and `fill_bytes`. It does *not* re-export `rand::random`, `rand::distributions` or `rand::seq`, and
has no `StdRng` or `SmallRng`; those exist only in the wrapper, where they ignore any seed you give
them.

## Injecting operation errors

The first thing worth injecting is "this call returned an error", because the interesting question
is never whether the caller compiles but whether the *recovery* is correct — and the usual recovery
is a retry loop, which is only correct if the operation is idempotent.

```rust,should_panic
# extern crate shuttle;
use shuttle::rand::{thread_rng, Rng};
use shuttle::sync::{Arc, Mutex};

shuttle::check_random(|| {
    let balance = Arc::new(Mutex::new(0u64));
    // The backend commits the write and *then* fails, as when a response is lost after landing.
    let deposit = |amount| {
        *balance.lock().unwrap() += amount;
        if thread_rng().gen_bool(0.5) { Err("unavailable") } else { Ok(()) }
    };
    while deposit(10).is_err() {} // "just retry until it works"
    assert_eq!(*balance.lock().unwrap(), 10, "deposit applied more than once");
}, 100);
```

This fails within a handful of executions with `left: 40, right: 10`, and prints a schedule
replaying that exact run of failures. Note the assertion: not "the retry eventually succeeded", the
easy half, but "the effect was applied exactly once". The fix is an idempotency key, whose check
has to share the critical section with the write:

```rust
# extern crate shuttle;
use shuttle::rand::{thread_rng, Rng};
use shuttle::sync::{Arc, Mutex};

shuttle::check_random(|| {
    let ledger = Arc::new(Mutex::new((0u64, Vec::<u64>::new()))); // (balance, applied ids)
    let deposit = |id, amount| {
        {
            let mut state = ledger.lock().unwrap(); // one critical section, not two
            if !state.1.contains(&id) { state.0 += amount; state.1.push(id); }
        }
        if thread_rng().gen_bool(0.5) { Err("unavailable") } else { Ok(()) }
    };
    while deposit(1, 10).is_err() {}
    assert_eq!(*ledger.lock().unwrap(), (10, vec![1]));
}, 100);
```

An idempotency check is itself a read-modify-write, so splitting it across two `lock()` calls
reopens the race. Run that body from two threads with distinct ids, assert `(20, vec![0, 1])`, and
you are testing retries and interleaving at once.

## Injecting a panic: lock poisoning

A panic while holding a lock is what an `unwrap` inside a critical section does when it meets bad
data. `shuttle::sync::Mutex` models it faithfully because it keeps a real `std::sync::Mutex`
inside: the mutex is poisoned, later `lock()` calls return `Err(PoisonError)`, and `clear_poison`
recovers. Contain the injected panic with `catch_unwind` so it does not fail the test outright:

```rust
# extern crate shuttle;
use shuttle::rand::{thread_rng, Rng};
use shuttle::sync::{Arc, Mutex};
use shuttle::thread;
use std::panic::catch_unwind;

shuttle::check_random(|| {
    let state = Arc::new(Mutex::new(0u64));
    let s = Arc::clone(&state);
    thread::spawn(move || {
        let _ = catch_unwind(|| {
            let mut guard = s.lock().unwrap();
            *guard += 1;
            if thread_rng().gen_bool(0.5) { panic!("injected panic holding the lock"); }
        });
    }).join().unwrap();
    match state.lock() {
        // The update did land; poisoning only says "someone panicked mid-update".
        Ok(guard) => assert_eq!(*guard, 1),
        Err(poisoned) => { assert_eq!(*poisoned.into_inner(), 1); state.clear_poison(); }
    };
}, 20);
```

Assert that the code *recovers*, not that no panic happened: a `state.lock().unwrap()` downstream
turns one poisoned mutex into a cascade of panicking tasks. Note that Shuttle's panic hook is
global and fires for panics you catch too, so each injected panic prints `Task failed, serializing
schedule` to stderr, and `FailurePersistence::None` hides the schedule but not those lines.

## Injecting a broken channel

A channel gives you two failures for free, both one dropped endpoint away. "The producer died
before it sent anything" is a `return` in the right place:

```rust,should_panic
# extern crate shuttle;
use shuttle::rand::{thread_rng, Rng};
use shuttle::sync::mpsc::channel;
use shuttle::thread;

shuttle::check_random(|| {
    let (tx, rx) = channel::<u64>();
    let producer = thread::spawn(move || {
        // The producer dies before sending, dropping `tx` here.
        if thread_rng().gen_bool(0.5) { return; }
        tx.send(1).unwrap();
    });
    let value = rx.recv().unwrap(); // BUG: assumes a value always arrives
    producer.join().unwrap();
    assert_eq!(value, 1);
}, 100);
```

`recv()` returns `Err(RecvError)` as soon as the last sender is dropped, so the consumer does not
hang — it panics on the `unwrap`. Treat disconnection as a normal end of stream instead:
`rx.iter()` stops when every sender is gone. The mirror image deserves its own test: drop the
`Receiver` early and every `send` returns `Err(SendError)`, so `tx.send(x).unwrap()` turns consumer
shutdown into a panic while a producer that ignores the error may silently drop queued work.

## Injecting cancellation

Cancellation is fault injection the runtime does to you, and the injection most likely to find
something, because it can stop a task *between* two writes that were meant to be atomic. [Async
code and futures](./async.md#cancellation-and-drop) has the mechanics; what matters here is that
`JoinHandle::abort()` (or `AbortHandle::abort()`) is the one to reach for. The call is a scheduling
point, the abort lands at the task's next await point, and Shuttle then *drops* the inner future,
so destructors run and anything it held, including lock guards, is released; awaiting the handle
yields `Err(JoinError::Cancelled)`. Dropping a future mid-`await` is the same thing by hand.
Dropping the `JoinHandle` cancels nothing — as in tokio it *detaches* the task, excluding it from
deadlock detection, so assertions inside it can silently never run.

```rust,should_panic
# extern crate shuttle;
use shuttle::future::{self, JoinError};
use shuttle::sync::{Arc, Mutex};

shuttle::check_dfs(|| {
    let account = Arc::new(Mutex::new((100u64, 0u64))); // (from, to)
    let a = Arc::clone(&account);
    let transfer = future::spawn_local(async move {
        let mut a = a.lock().unwrap();
        a.0 -= 10;
        future::yield_now().await; // cancellation can land here, guard still held
        a.1 += 10;
    });
    transfer.abort();
    match future::block_on(transfer) { Ok(()) | Err(JoinError::Cancelled) => {} }
    let account = account.lock().unwrap();
    assert_eq!(account.0 + account.1, 100, "money vanished: {account:?}");
}, None);
```

DFS reports `(90, 0)` immediately. That the final `lock()` succeeds rather than deadlocking
confirms the guard was released when the future was dropped: the resource is fine, the *invariant*
is not. (`spawn_local` is required because a guard is held across the await point; the compiler
error you get from `spawn` is telling you something true.) The fix is a rollback that runs on drop,
so the cancellation path is not a second code path somebody must remember to write: debit the
source, then build a `Debit { account, amount, committed: false }` whose `Drop` puts the money back
unless `committed` was set after the credit landed. Release the lock *before* the await, or the
destructor cannot retake it — a `Drop` impl that locks a mutex the future still holds is a
re-entrant lock, which Shuttle reports. That version survives an exhaustive DFS.

## Injecting shutdown: closing a semaphore

Shutdown-under-load deserves its own injection point: a pool, queue or rate limiter torn down while
requests are queued behind it. `BatchSemaphore::close()` models this, marking the semaphore closed
and waking every queued waiter so its `acquire` returns `Err(AcquireError)`.

```rust
# extern crate shuttle;
use shuttle::future::batch_semaphore::{BatchSemaphore, Fairness};
use shuttle::rand::{thread_rng, Rng};
use shuttle::sync::Arc;
use shuttle::thread;

shuttle::check_random(|| {
    // A pool of two connections, both checked out by the main task.
    let pool = Arc::new(BatchSemaphore::new(2, Fairness::StrictlyFair));
    pool.try_acquire(2).unwrap();
    let clients: Vec<_> = (0..2)
        .map(|_| Arc::clone(&pool))
        .map(|p| thread::spawn(move || p.acquire_blocking(1).is_ok()))
        .collect();
    // Injected failure: the pool is torn down while requests are queued behind it.
    let shut_down = thread_rng().gen_bool(0.5);
    if shut_down { pool.close() } else { pool.release(2) }
    let got: Vec<bool> = clients.into_iter().map(|c| c.join().unwrap()).collect();
    assert!(got.iter().all(|&ok| ok != shut_down), "a closed pool handed out a permit");
}, 100);
```

Half the value is what you do *not* assert: if `close()` failed to wake a waiter, that task stays
blocked and Shuttle reports a deadlock for free. `close()` is itself a scheduling point, so both
"the waiter queued, then we closed" and "we closed, then the waiter tried" get explored, and
`Fairness::Unfair` lets waiters wake out of order.

## Timeouts are not time

The one failure you cannot inject by waiting is a timeout, because **Shuttle does not model time at
all**, and the API looks like it should work. `shuttle::thread::sleep` and `park_timeout` ignore
their durations and are just yield points; `Condvar::wait_timeout` and `Receiver::recv_timeout`
never time out; `Instant::now()` reads the real clock, so any duration computed from it is ambient
nondeterminism. In `shuttle-tokio`, `time::pause()` and `time::resume()` are no-ops,
`time::advance()` is a bare `yield_now`, `sleep` yields once and resolves (unless its deadline is
over a year out, when it never resolves), and `timeout(duration, fut)` ignores the duration
entirely. `Interval::tick()` returns immediately, forever, so `SHUTTLE_INTERVAL_TICKS` exists to
bound how many ticks each `Interval` produces (default `usize::MAX`, `0` means none).

So a timeout has to be modeled as an injected error: nothing observable happens inside the window,
and outside it all that matters is whether your code takes the `Err(Elapsed)` branch — a decision,
not a duration. For raw `shuttle::future` code that is `maybe_fail` under another name. For
`shuttle-tokio` the wrapper provides a hook: `time::trigger_timeouts(predicate)` expires every
current *and future* `timeout()` in a task whose [labels](./debugging.md) match, and
`time::clear_triggers()` undoes it.

```rust,ignore
// `trigger_timeouts` exists only in the wrapper, so gate the test on the `shuttle` feature.
use shuttle::current::{me, set_label_for_task, Labels};
use tokio::time::{clear_triggers, timeout, trigger_timeouts, Duration};

// In the task you want to give up (`Impatient` is any `Clone + Debug` label type). The duration
// is ignored; the label is what decides whether this expires.
set_label_for_task(me(), Impatient);
let gave_up = timeout(Duration::from_secs(1), resource.lock()).await.is_err();

// In the test, once the waiters are spawned and before they are awaited. Triggers live in a
// thread-local, so clear them at the start of every execution.
clear_triggers();
trigger_timeouts(|labels: &Labels| labels.get::<Impatient>().is_some());
```

This is how Shuttle's own suite turns a deadlocking dining-philosophers instance into a live one;
see `wrappers/tokio/impls/tokio/inner/tests/time.rs`, which labels each philosopher and triggers
timeouts on one of them. Targeting a *subset* of tasks matters, because expiring everybody's
timeouts usually makes the interesting contention disappear.

## Budgeting the search

Every injection point multiplies the state space, and injected failures add retries, which add
operations, which add interleavings. Three strategies, in increasing order of rigour. **Bound the
injections per execution:** a budget shared by all injection sites keeps retry loops converging and
each schedule short, turning "retries eventually succeed" into an exact range; without one, retries
can run an execution into the step bound from [Configuring test runs](./configuration.md).

```rust
# extern crate shuttle;
use shuttle::rand::{thread_rng, Rng};
use shuttle::sync::{Arc, Mutex};
use shuttle::thread;

shuttle::check_random(|| {
    let budget = Arc::new(Mutex::new(2usize)); // at most two injected errors per execution
    let attempts = Arc::new(Mutex::new(0usize));
    let workers: Vec<_> = (0..2)
        .map(|_| (Arc::clone(&budget), Arc::clone(&attempts)))
        .map(|(budget, attempts)| thread::spawn(move || loop {
            *attempts.lock().unwrap() += 1;
            let mut left = budget.lock().unwrap();
            if *left > 0 && thread_rng().gen_bool(0.9) {
                *left -= 1;
                continue; // injected failure: retry
            }
            break;
        }))
        .collect();
    for worker in workers { worker.join().unwrap(); }
    // Two successes plus at most two injected failures: an assertable bound.
    assert!((2..=4).contains(&*attempts.lock().unwrap()), "unexpected attempt count");
}, 100);
```

The budget's mutex is itself a scheduling point and serializes the injection sites; if that
distorts the workload, give each task its own budget. **Bias the probability:** `0.5` is fine for a
single site, but with five sites in a request path the all-succeed case is almost never explored.
Bias low (`0.05`-`0.2`) for mostly-working executions with occasional faults, high when hammering a
recovery path, as `0.9` does above to make sure the budget is spent.

**Shrink the configuration until you can enumerate it.** The strongest option, and it needs one
correction to an assumption people make: **`check_dfs` cannot be combined with `shuttle::rand`.**
It builds its scheduler with random data disabled, so the first draw panics with `requested random
data from DFS scheduler with allow_random_data = false`. Constructing `DfsScheduler::new(n, true)`
by hand permits draws but does not enumerate them: DFS uses a *fixed* data source, reseeded
identically for every execution, so it explores all interleavings of exactly one failure pattern.
To cover both dimensions, lift the pattern out of the schedule, make it a parameter, and enumerate
the patterns in an ordinary Rust loop outside Shuttle:

```rust
# extern crate shuttle;
use shuttle::sync::{Arc, Mutex};
use shuttle::thread;

fn check_with_plans(plans: Vec<Vec<bool>>) {
    shuttle::check_dfs(move || {
        let applied = Arc::new(Mutex::new(0u64));
        let workers: Vec<_> = plans.clone().into_iter().map(|plan| {
            let applied = Arc::clone(&applied);
            // The plan always ends in a success, so the retry loop terminates.
            thread::spawn(move || for fails in plan.into_iter().chain(std::iter::once(false)) {
                if !fails { *applied.lock().unwrap() += 10; break; }
            })
        }).collect();
        for worker in workers { worker.join().unwrap(); }
        assert_eq!(*applied.lock().unwrap(), 20);
    }, None);
}

for pattern in 0..4u32 {
    check_with_plans(vec![vec![pattern & 1 != 0], vec![pattern & 2 != 0]]);
}
```

A bounded number of boolean decisions *is* a finite set, so this is exhaustive over both
dimensions, and at this size it costs milliseconds. It is the same "plan" trick [Reference
tests](./reftests.md) uses for operation sequences, and composes with it: one plan for what each
task does, one for which of those operations fail. Otherwise [Schedulers](./schedulers.md) applies
as usual — `check_random` is the default, since injected failures widen the space of *values*
rather than needing a rare preemption, and `check_pct` is for bugs needing a failure *and* a
preemption.

## Proving your injection is actually controlled

An injection point is only useful if it replays, and that is easy to break by accident: a
`rand::random()` that slipped past the wrapper, a retry counter in a `static`, a decision keyed on
`Instant::now()`. The symptom is nasty, because the test still fails — with a schedule that
reproduces nothing. `check_uncontrolled_nondeterminism(f, iterations)` runs each random schedule
and immediately replays it, panicking if the second run asks a different question:

```rust,should_panic
# extern crate shuttle;
use shuttle::sync::{Arc, Mutex};
use shuttle::thread;
use std::sync::atomic::{AtomicUsize, Ordering};

// BUG: the failure decision comes from process-global state, not from the schedule.
static ATTEMPTS: AtomicUsize = AtomicUsize::new(0);
fn maybe_fail() -> bool { ATTEMPTS.fetch_add(1, Ordering::SeqCst) % 3 == 0 }

shuttle::check_uncontrolled_nondeterminism(|| {
    let retries = Arc::new(Mutex::new(0));
    let r = Arc::clone(&retries);
    let worker = thread::spawn(move || while maybe_fail() { *r.lock().unwrap() += 1; });
    worker.join().unwrap();
}, 20);
```

The retry loop takes a different number of trips on the replay, so the runs disagree about the
tasks:

```text
possible nondeterminism: set of runnable tasks is different than expected.
Expected:
[main-thread(1)]
but got:
[main-thread(0)]
```

The other messages are `current execution ended earlier than expected` and `next step was context
switch, but recording expected random number generation` — the latter is the signature of a draw
that moved, which is what a leaked injection decision looks like. It is a bug finder, not a proof:
nondeterminism that never changes what the scheduler is asked slips through. Note also that the
panic printed just *above* the real message is spurious: the check fires from inside the scheduler,
so an `ExecutionState ... AlreadyBorrowed` panic lands first, with an unrelated schedule. Read past
it, and see [pitfalls](./pitfalls.md#finding-the-source-check_uncontrolled_nondeterminism).

## A checklist

- Failure decisions come from `shuttle::rand`, never from `rand`, the clock, or a `static`.
- Failures are `Result`s where the real code allows; injected panics use `catch_unwind`.
- The assertion is about the effect, not the outcome: applied exactly once, invariant restored.
- Retries are bounded, by a budget or by a plan, so the schedule stays short.
- Cancellation is injected with `abort()`, and invariants spanning an await are restored on `Drop`.
- Nothing waits for time to pass; timeouts are injected errors aimed at a labeled subset of tasks.
- Shutdown is injected too, and deadlock detection asserts nobody is left waiting afterwards.
- The test has run under `check_uncontrolled_nondeterminism`, and shrunk under `check_dfs`.
