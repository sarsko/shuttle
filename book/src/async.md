# Async code and futures

If your concurrency comes from tasks rather than threads, Shuttle works the same way — but it takes
over a job tokio normally does for you. Shuttle *is* the executor: it decides when each future gets
polled, and it makes that decision the same way it decides which thread runs next, by asking the
scheduler. Everything in this chapter lives in
[`shuttle::future`](https://docs.rs/shuttle/latest/shuttle/future/index.html), roughly a drop-in
replacement for `futures::executor`.

## Shuttle's async execution model

Three facts explain most of the behavior you will see.

**A spawned future is a task, exactly like a thread.** `future::spawn(fut)` creates a Shuttle task
with its own id and its own stack whose body is a loop that polls `fut` until it is ready. There is
no worker pool and no central executor loop: `future::spawn(fut)` is very nearly
`thread::spawn(|| block_on(fut))` plus a join handle. Deadlock detection, vector clocks, tracing,
labels and step bounds treat async tasks and threads identically.

**A task can be preempted anywhere it touches Shuttle.** Because each poll runs on the task's own
stack, Shuttle can suspend a task *in the middle of a `poll`* and resume it later. It does so at
every operation it controls: locking a Shuttle mutex, any atomic load or store, sending on a channel,
spawning, `thread::yield_now`, `future::yield_now`. So `.await` points are scheduling points, but
they are not the only scheduling points inside an async task — the opposite of your mental model from
a real executor, where a poll is atomic with respect to its worker thread.

**Returning `Pending` puts the task to sleep, and only a waker wakes it.** After a poll returns
`Pending`, Shuttle puts the task to sleep unless its waker was already called during that poll. A
sleeping task is not runnable, so if every remaining task is asleep or blocked, Shuttle reports a
deadlock instead of hanging.

Since Shuttle runs one task at a time on one OS thread, blocking inside a poll blocks only *that*
task and Shuttle schedules something else. That is convenient for testing, but it also means Shuttle
will not tell you that you have stalled a real tokio worker by blocking in a poll.

## The `shuttle::future` API

[`spawn`](https://docs.rs/shuttle/latest/shuttle/future/fn.spawn.html) takes any
`Future + Send + 'static` and returns a
[`JoinHandle<T>`](https://docs.rs/shuttle/latest/shuttle/future/struct.JoinHandle.html), which is
itself a future resolving to `Result<T, JoinError>`.
[`block_on`](https://docs.rs/shuttle/latest/shuttle/future/fn.block_on.html) drives a future to
completion on the *current* task, sleeping and handing control to the scheduler on each `Pending`.
It is how you get from synchronous test-body code into async code, and it works from the main test
task or from any Shuttle thread.

```rust
# extern crate shuttle;
use shuttle::future;

shuttle::check_dfs(|| {
    let handle = future::spawn(async { 1 + 1 });
    assert_eq!(future::block_on(handle).unwrap(), 2);
}, None);
```

`JoinError` has exactly one variant, `JoinError::Cancelled`. Unlike tokio there is no
`JoinError::Panic`: a task panic fails the whole test and prints a failing schedule rather than being
reported to whoever joins. `JoinHandle` also offers `abort()`, `is_finished()` and `abort_handle()`,
which returns a cloneable `AbortHandle` you can pass elsewhere — see
[Cancellation and drop](#cancellation-and-drop).

`spawn_local` is `spawn` without the `Send` bound, mirroring `tokio::task::spawn_local`. Use it when
your future holds something non-`Send` across an await point, such as a Shuttle `MutexGuard` or an
`Rc`:

```rust
# extern crate shuttle;
use shuttle::future;
use std::rc::Rc;

shuttle::check_dfs(|| {
    let handle = future::spawn_local(async {
        let value = Rc::new(7);
        future::yield_now().await; // holding `Rc` across the await makes this future !Send
        *value
    });
    assert_eq!(future::block_on(handle).unwrap(), 7);
}, None);
```

[`yield_now`](https://docs.rs/shuttle/latest/shuttle/future/fn.yield_now.html) wakes the current task
and returns `Pending` once, forcing a scheduling point without blocking. Prefer it to hand-rolling
the same pattern: it also tells Shuttle the task *asked* to yield, which lets schedulers such as PCT
deprioritize a task that is spinning.

All of these panic if called outside a Shuttle execution.

## Your first async test

Here is a lost update between two async tasks sharing a `shuttle::sync::Mutex`. Each reads the
counter, hits an await point, then writes back what it read:

```rust,should_panic
# extern crate shuttle;
use shuttle::future;
use shuttle::sync::{Arc, Mutex};

shuttle::check_random(|| {
    let counter = Arc::new(Mutex::new(0usize));
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let counter = Arc::clone(&counter);
            future::spawn(async move {
                let value = *counter.lock().unwrap(); // read, then release the lock
                future::yield_now().await;            // another task may run here
                *counter.lock().unwrap() = value + 1; // write back a possibly stale value
            })
        })
        .collect();

    future::block_on(async move {
        for handle in handles {
            handle.await.unwrap();
        }
    });

    assert_eq!(*counter.lock().unwrap(), 2, "lost an increment");
}, 100);
```

`check_random` finds this almost immediately: the await point in the middle of the critical section
is exactly the window the scheduler needs.

Now delete the await point. The two statements are still not atomic, because Shuttle treats every
atomic operation as a scheduling point, so the bug survives — but finding it now needs a *preemption*
in the middle of a poll rather than a task voluntarily suspending. That is what
[`check_pct`](./schedulers.md) is built for; its third argument bounds how many preemptions a
schedule may contain:

```rust,should_panic
# extern crate shuttle;
# use shuttle::future;
# use shuttle::sync::atomic::{AtomicUsize, Ordering};
# use shuttle::sync::Arc;
shuttle::check_pct(|| {
    let counter = Arc::new(AtomicUsize::new(0));
    let spawn_increment = |counter: Arc<AtomicUsize>| {
        future::spawn(async move {                      // no await point anywhere
            let value = counter.load(Ordering::SeqCst); // preemptible here...
            counter.store(value + 1, Ordering::SeqCst); // ...and here
        })
    };

    let t1 = spawn_increment(Arc::clone(&counter));
    let t2 = spawn_increment(Arc::clone(&counter));
    future::block_on(async move {
        t1.await.unwrap();
        t2.await.unwrap();
    });

    assert_eq!(counter.load(Ordering::SeqCst), 2, "lost an increment");
}, 500, 2);
```

## Mixing threads and async tasks

Threads and async tasks coexist with no ceremony: a Shuttle thread can `block_on` a future, an async
task can call `thread::yield_now` or lock a `shuttle::sync::Mutex`, and either can spawn the other.
`block_on` inside a Shuttle thread blocks *that* Shuttle thread; everything else keeps running.

```rust
# extern crate shuttle;
use shuttle::sync::{Arc, Mutex};
use shuttle::{future, thread};

shuttle::check_random(|| {
    let counter = Arc::new(Mutex::new(0usize));
    let c = Arc::clone(&counter);
    let worker = thread::spawn(move || future::block_on(async move { *c.lock().unwrap() += 1 }));
    let c = Arc::clone(&counter);
    let task = future::spawn(async move { *c.lock().unwrap() += 1 });

    worker.join().unwrap();
    future::block_on(task).unwrap();
    assert_eq!(*counter.lock().unwrap(), 2);
}, 100);
```

## Async synchronization and the batch semaphore

`shuttle::sync` offers only blocking APIs, matching `std::sync`. Calling `lock()` from inside an
async task is allowed and correctly modelled — the task blocks and the scheduler runs something else
— but it is a blocking wait, not an `.await`, so it can deadlock just as it would in production. For
an async lock, use `shuttle-tokio`'s `sync::Mutex` (see [below](#testing-real-tokio-code)).

Underneath async waiting is
[`BatchSemaphore`](https://docs.rs/shuttle/latest/shuttle/future/batch_semaphore/struct.BatchSemaphore.html),
exposed at `shuttle::future::batch_semaphore`. It is a counting semaphore that can acquire several
permits at once, with both an async `acquire` and a blocking `acquire_blocking`, plus `try_acquire`,
`release`, `close` and `available_permits`. It is the primitive Shuttle uses to model handing out and
reclaiming permission to proceed: Shuttle's own `Mutex` and `RwLock` are built on it, as are the
`shuttle-tokio` `Mutex`, `RwLock`, `Semaphore` and channels. You mostly meet it indirectly, but it is
the right tool when you are modelling your own primitive:

```rust
# extern crate shuttle;
use shuttle::future::{self, batch_semaphore::{BatchSemaphore, Fairness}};

shuttle::check_dfs(|| {
    let semaphore = BatchSemaphore::new(2, Fairness::StrictlyFair);
    future::block_on(async move {
        semaphore.acquire(2).await.unwrap(); // takes *both* permits
        assert_eq!(semaphore.available_permits(), 0);
        semaphore.release(2);
    });
}, None);
```

`BatchSemaphore::new` takes a fairness mode: `StrictlyFair` makes earlier requesters win, while
`Unfair` lets waiters be woken in any order, so one can be starved. Choosing `Unfair` is how you get
Shuttle to explore starvation orderings a fair queue would hide.

## Wakers and lost wakeups

Shuttle's waker is about as simple as a waker can be: the `Waker`'s data pointer *is* the task id,
and waking sets a flag on that task, making it runnable again if it was asleep. Three consequences:

- **Wakers are cheap, cloneable and sendable anywhere.** Nothing is allocated and nothing has to be
  kept alive, so stashing a `Waker` in a shared `Mutex` and waking it from another task is fine.
- **A wake during a poll is not lost.** If your waker fires before `poll` returns `Pending`, Shuttle
  sees the flag and does not put the task to sleep, so it will be polled again.
- **A wake does not unblock a blocked task.** If a task is blocked acquiring a Shuttle lock, calling
  its waker does not shortcut that; it only stops the *next* `Pending` from sleeping.

The flip side is that Shuttle holds you to the contract. A future that returns `Pending` without
registering the waker will never be polled again, and Shuttle turns that into a deadlock panic:

```rust,should_panic
# extern crate shuttle;
use shuttle::future;
use shuttle::sync::atomic::{AtomicBool, Ordering};
use shuttle::sync::Arc;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

struct WaitForFlag(Arc<AtomicBool>);

impl Future for WaitForFlag {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
        if self.0.load(Ordering::SeqCst) {
            Poll::Ready(())
        } else {
            Poll::Pending // BUG: never registers `cx.waker()`, so nobody can wake us
        }
    }
}

shuttle::check_dfs(|| {
    let flag = Arc::new(AtomicBool::new(false));
    let setter = {
        let flag = Arc::clone(&flag);
        future::spawn(async move { flag.store(true, Ordering::SeqCst) })
    };

    future::block_on(WaitForFlag(flag));
    future::block_on(setter).unwrap();
}, None);
```

`check_dfs` explores the ordering where `WaitForFlag` is polled first, sees `false` and sleeps
forever. The panic lists every unfinished task, annotating one that is asleep inside a `Pending`
future with `pending future` (and a detached one with `detached`):

```text
deadlock! blocked tasks: [main-thread (task main-thread(0), pending future)]
```

The fix is to publish the waker where the waker-upper can find it, and to actually call it:

```rust,ignore
// in poll(), holding a `Mutex<(bool, Option<Waker>)>`:
let mut state = self.0.lock().unwrap();
if state.0 {
    Poll::Ready(())
} else {
    state.1 = Some(cx.waker().clone()); // register before returning Pending
    Poll::Pending
}

// in the task that makes the future ready:
let waker = { let mut state = state.lock().unwrap(); state.0 = true; state.1.take() };
if let Some(waker) = waker {
    waker.wake(); // drop this line and the deadlock above comes straight back
}
```

Note how narrow correctness is here. Taking the waker out of the slot and then dropping it instead
of waking it — the classic "took the waker and forgot to call it" bug — produces exactly the same
deadlock, and `check_dfs` finds it the same way.

The opposite failure is a future that wakes *itself* and never finishes, starving everything else.
Shuttle catches that with a step bound rather than a deadlock, because the task is technically
running:

```rust,should_panic
# extern crate shuttle;
use shuttle::scheduler::RandomScheduler;
use shuttle::{future, Config, MaxSteps, Runner};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

struct SpinForever;

impl Future for SpinForever {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        cx.waker().wake_by_ref(); // "poll me again"
        Poll::Pending             // ...but never make progress
    }
}

let mut config = Config::new();
config.max_steps = MaxSteps::FailAfter(100);
Runner::new(RandomScheduler::new(1), config).run(|| future::block_on(SpinForever));
```

The default bound is `MaxSteps::FailAfter(1_000_000)`, so a spin loop fails eventually even if you
configure nothing; lowering it makes such tests fail fast. See
[Configuring test runs](./configuration.md).

## Cancellation and drop

`JoinHandle::abort()` (or `AbortHandle::abort()`) requests cancellation. The call is itself a
scheduling point, and cancellation takes effect the next time Shuttle polls the task, so:

- Code after the await point where the task was suspended never runs.
- The inner future is dropped, so its destructors run and anything it held — including Shuttle lock
  guards — is released.
- Awaiting the handle yields `Err(JoinError::Cancelled)`.
- If the task had already finished, `abort` is a no-op and you still get `Ok(value)`. Aborting twice
  is harmless.
- If the task is blocked in a Shuttle synchronization operation, the abort waits for that operation
  to finish, since there is no await point to cancel at yet.

Because `abort` is a scheduling point, both outcomes are usually reachable and Shuttle explores both,
so write assertions that accept either:

```rust
# extern crate shuttle;
use shuttle::future::{self, JoinError};

shuttle::check_dfs(|| {
    let handle = future::spawn(async {
        future::yield_now().await;
        42
    });
    handle.abort();

    match future::block_on(handle) {
        Ok(value) => assert_eq!(value, 42),  // finished before the abort landed
        Err(JoinError::Cancelled) => {}      // cancelled at the await point
    }
}, None);
```

**Dropping a `JoinHandle` does not cancel the task; as in tokio it *detaches* it.** That matters at
the end of a test. The test body returning does not end the execution: Shuttle keeps scheduling until
no *attached* task has work left. Detached tasks are excluded from deadlock detection and from that
decision, so once the last attached task finishes, the execution ends and whatever the detached tasks
had left to do is truncated. An attached task still asleep when nothing else can run is a deadlock; a
detached one is dropped silently, and its remaining assertions never run.

So `future::spawn(...)` with the handle discarded is genuinely "spawn and maybe forget", and Shuttle
explores both possibilities:

```rust
# extern crate shuttle;
use shuttle::future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

// Counters live outside the test body, so they accumulate across executions.
let executions = Arc::new(AtomicUsize::new(0));
let task_runs = Arc::new(AtomicUsize::new(0));

let (e, r) = (Arc::clone(&executions), Arc::clone(&task_runs));
shuttle::check_dfs(move || {
    e.fetch_add(1, Ordering::SeqCst);
    let r = Arc::clone(&r);
    // The JoinHandle is dropped immediately, which detaches the task.
    future::spawn(async move { r.fetch_add(1, Ordering::SeqCst) });
}, None);

assert_eq!(executions.load(Ordering::SeqCst), 2);
assert_eq!(task_runs.load(Ordering::SeqCst), 1); // in the other execution it never ran at all
```

One related knob: when a task panics, Shuttle by default keeps scheduling until that task has fully
unwound. Setting `Config::ungraceful_shutdown_config.immediately_return_on_panic` stops scheduling as
soon as a task panics instead, which matters if unwinding an async task runs destructors that panic
again. See [Configuring test runs](./configuration.md).

## Testing real tokio code

Everything above uses Shuttle's own executor. Production code says `tokio::spawn`,
`tokio::sync::Mutex` and `#[tokio::test]`, none of which Shuttle can see. The wrapper crates fix that
by shadowing tokio: you depend on `shuttle-tokio` under the name `tokio`, and a feature flag picks
the real implementation or the Shuttle-modelled one.

```toml
[features]
shuttle = ["tokio/shuttle"]

[dependencies]
# Renamed so `use tokio::...` in your code resolves to the wrapper. `shuttle-tokio` mirrors tokio's
# features but enables none by default, so carry over the ones you were using.
tokio = { package = "shuttle-tokio", version = "VERSION_NUMBER", features = ["rt", "sync", "macros"] }
```

Without `--features shuttle` your crate builds against real tokio and behaves exactly as before. With
it, `tokio::spawn` becomes a Shuttle task, `tokio::sync` primitives are modelled on the batch
semaphore, `tokio::select!` is Shuttle-aware, and `#[tokio::test]` expands into a Shuttle run of the
test body instead of starting a runtime:

```rust,ignore
use std::sync::Arc;

// Unchanged source: this compiles and runs under both real tokio and Shuttle.
#[tokio::test]
async fn transfers_are_atomic() {
    let account = Arc::new(tokio::sync::Mutex::new(100));
    let task = tokio::spawn({
        let account = Arc::clone(&account);
        async move { *account.lock().await -= 10; }
    });
    *account.lock().await -= 10;
    task.await.unwrap();
    assert_eq!(*account.lock().await, 80);
}
```

That runs the body 100 times by default; `SHUTTLE_ITERATIONS` and `SHUTTLE_SCHEDULER` override the
count and the scheduler without touching the test. When you want finer control, drop back to
`shuttle::check_dfs(|| future::block_on(async { ... }), None)` — the wrapper types still work.

Coverage is good but not complete: `io`, `net` and `fs` are re-exported from real tokio only so that
code compiles, `sync::broadcast` is a stub, `time::pause`/`time::resume` are no-ops, `task_local` is
not modelled correctly, and a few methods are unimplemented.
[Third-party crates and wrappers](./wrappers.md) has the full picture, the sibling wrappers
(`shuttle-tokio-util`, `shuttle-tokio-stream`, `shuttle-tokio-retry`) and how to write your own.

## Async-specific pitfalls

Most of [Determinism rules and common pitfalls](./pitfalls.md) applies unchanged. These bite
specifically in async tests:

- **Do not use another executor.** `futures::executor::block_on` inside a Shuttle test parks the one
  real OS thread Shuttle owns, so nothing can run to wake it and the test hangs with no schedule to
  replay. Always use `shuttle::future::block_on`.
- **Do not block a task with something Shuttle cannot see.** A `std::sync::Mutex`, `std::thread::sleep`
  or `std::sync::mpsc` inside an async task gives Shuttle no scheduling point, and if what you are
  waiting for is produced by another Shuttle task the test hangs instead of reporting a deadlock. Real
  sleeps also break replay. Use `shuttle::sync`, and `shuttle-tokio`'s `time` for anything timed.
- **Do not discard `JoinHandle`s you care about.** A detached task can be truncated when the last
  attached task finishes, so an `assert!` inside it may silently never run. Keep the handle, await it,
  and treat "this task ran" as something to assert rather than assume.
- **Beware tasks that can never complete.** A task waiting for a signal that only arrives after the
  test body returns deadlocks if it is attached and vanishes if it is detached. Make the end of the
  test body the point where everything has been joined.
- **Don't hand-roll `yield_now`.** `cx.waker().wake_by_ref(); Poll::Pending` looks equivalent, but it
  does not tell the scheduler you yielded, so PCT will not deprioritize the spinning task and fair
  schedules may go unexplored.
- **Guards held across await points need `spawn_local`.** That compiler error is telling you something
  real: a lock held across an await is a deadlock risk in production too.
