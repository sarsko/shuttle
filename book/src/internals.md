# How Shuttle works

Shuttle runs your concurrent test on a single OS thread, interleaving tasks only at scheduling points
it controls, so an execution is a sequence of choices that can be recorded and replayed. Everything
else in this book follows from that sentence. This chapter explains the machinery: what a task is, how
a "thread" is resumed, which operations give the scheduler a chance to switch, how the choices are
recorded, and what Shuttle therefore cannot see.

## The model

A test runs inside a `Runner`, which owns a `Scheduler` and a `Config` and runs the test body many
times. Each run is an `Execution`, and within it exactly one thing runs at a time. When the running
task reaches a scheduling point it returns control to the executor, which asks the scheduler what to
run next and resumes that task.

```text
Runner::run: for each iteration
  scheduler.new_execution()  ──►  None: stop the test / Some(schedule): fresh Execution
  Execution::run, looping in run_to_completion:
    1. ExecutionState::schedule()   ask the scheduler for the next task
    2. advance_to_next_task()       record the choice in the schedule
    3. continuation.resume()        run that task to its next switch point
    4. task finished / yielded / panicked
```

Two consequences: there is no parallelism at all, so a bug requiring genuine physical overlap is out of
reach; and because *every* interleaving decision passes through step 1, the scheduler's answers
completely describe the execution, which is what makes replay possible.

## Tasks

A `Task` (`shuttle-engine/src/runtime/task/mod.rs`) is Shuttle's reflection of one user-level unit of
concurrency. The file opens with a terminology note worth internalizing:

- a **thread** is a user-level unit of concurrency created with `shuttle::thread::spawn`;
- a **future** is the other user-level unit, polled by Shuttle's executor;
- a **task** is the executor's bookkeeping for either — an id, a state, and a continuation;
- a **continuation** is the low-level green thread that actually holds the user code.

Threads and futures unify because both become a `Task` wrapping a continuation. For a thread the
continuation runs the closure directly; for a future, `Task::from_future` wraps it in a closure that
is, in essence:

```rust,ignore
while future.as_mut().poll(cx).is_pending() {
    ExecutionState::with(|state| state.current_mut().sleep_unless_woken());
    thread::switch();
}
```

So "polling a future" and "running a thread for one step" are the same operation to the scheduler:
resume the continuation, let it run to its next switch point.

### Task state

`TaskState` has exactly four variants:

| State | Meaning | How it is left |
|---|---|---|
| `Runnable` | Available to be scheduled. | Chosen by the scheduler. |
| `Blocked { allow_spurious_wakeups }` | Blocked in a synchronization operation. | Another task calls `unblock`, or — if the flag is set — the scheduler wakes it spuriously. |
| `Sleeping` | A future returned `Pending`; waiting for its waker. | `Waker::wake`. |
| `Finished` | The task's function returned. Terminal. | Never. |

Only `Runnable` tasks are offered to the scheduler, with one exception: a task in
`Blocked { allow_spurious_wakeups: true }` is also offered, because `std::thread::park` may return
spuriously. Such a task does not count as runnable for deadlock purposes, since nothing guarantees a spurious
wakeup happens. The bias is towards more interleavings: `sleep_unless_woken` carries the comment *"A
synchronous Task should never call this, because we want threads to be enabled-by-default to avoid bugs where
Shuttle incorrectly omits a potential execution."*

### Ids and labels

`TaskId` is a `usize` handed out sequentially as `tasks.len()`, never reused within an execution; task 0 is
the main thread. Ids are stable across executions only to the extent that your program creates tasks in the
same order — exactly the property replay depends on. Tasks also carry **labels**: a typed map (one value per
type, modelled on `http`'s `Extensions`) in `runtime/task/labels.rs`, reachable through `shuttle::current`.
The reserved `TaskName` label makes `TaskId`'s `Debug` print `"worker"(3)` instead of `TaskId(3)`. Children
inherit their parent's labels at spawn, and a parent can install a `ChildLabelFn` to rewrite them there,
which matters because labels set inside the child closure only take effect once the child first runs. Each
task also has a `TaskSignature`, a hash of its creation stack that is *mostly* stable across executions;
that is what lets URW carry statistics between runs.

## Continuations

A Shuttle "thread" is a [corosensei](https://crates.io/crates/corosensei) coroutine (`corosensei = "0.3.1"`).
`Continuation::new` allocates a stack and starts a coroutine whose body is a loop: suspend, receive a
`Box<dyn FnOnce()>`, run it, suspend again. Switching away is `Yielder::suspend`, a stack switch with no OS
involvement and no locks — which is why Shuttle can afford a scheduling point at every synchronization
operation. Continuations are pooled (`ContinuationPool`, a thread-local `VecDeque`) and reused across tasks
and executions, since allocating one means `mmap` plus `mprotect`; only a *reusable* continuation, one that
finished its function and awaits a new one, goes back to the pool, because a continuation stopped inside user
code cannot be handed new work without leaking the live frame. Three practical consequences:

- **`Config::stack_size` is a real limit.** Each task gets a fixed 0xf000-byte (60 KiB) stack by default,
  with no growth, so deep recursion overflows it. Raise it in [`Config`](./configuration.md).
- **A task is not an OS thread.** `std::thread_local!`, TLS destructors,
  `std::thread::current().id()`, OS-visible thread names — anything keyed on the OS thread is shared by
  every Shuttle task. Hence Shuttle's own `thread_local!` (see [below](#per-task-storage)).
- **Blocking syscalls block everything.** A real `std::sync::Mutex::lock`, a `std::thread::sleep`, or a
  socket read blocks the single OS thread running the whole execution, and Shuttle cannot run another
  task. Best case the test is slow; common case it hangs.

## Scheduling points

`thread::switch()` in `runtime/thread/continuation.rs` is the only way control leaves a task. Its doc
comment states the design rule: it *"should be called before any visible operation. If each visible
operation has a scheduling point before it, then there will be a potential context switch in between any
pair of visible operations, which is a necessary condition for completeness."*

Switching *before* the operation lets the scheduler decide knowing what is about to happen. The cost is
that a blocking operation would switch twice — before it, and again when it blocks. Hence the
**double-yield optimization**: the pre-blocking switch may be omitted only if the act of blocking
commutes with every other operation on that resource. `Acquire::poll` in `future/batch_semaphore.rs`
applies exactly that argument: for an *unfair* semaphore, blocking inserts into an unordered waiter set
and commutes, so the pre-switch is skipped; for a *strictly fair* semaphore, blocking appends to a queue
where `[T1 T2]` differs from `[T2 T1]`, so it is mandatory. `mpsc` and `Barrier` reason the same way.

Operations that are scheduling points:

- **Locks**: `Mutex::lock`/`try_lock`/`into_inner`, `RwLock::read`/`write`/`try_*`, and the guard `Drop`
  handlers that release them (a release is a `BatchSemaphore::release`, which switches).
- **Condvars**: `wait`, `wait_while`, `wait_timeout`, `notify_one`, `notify_all`; also `Barrier::wait`,
  `Once::call_once`, and `send`/`recv`/`try_send`/`try_recv` on `shuttle::sync::mpsc`.
- **Semaphores**: every `BatchSemaphore` entry point (`acquire`, `try_acquire`, `release`, `close`) — the
  primitive the locks and async primitives are built on. And every atomic operation: `load`, `store`, `swap`,
  `compare_exchange`, `fetch_*`.
- **Futures**: each `poll` returning `Pending`, `block_on`'s loop, `future::yield_now`, and
  `JoinHandle::abort` (so interleavings where the target finishes first are explored).
- **Task lifecycle**: `thread::spawn` and `future::spawn` switch *before* creating the task; `JoinHandle::join`;
  and a task's own exit when that would truncate the execution (if the last attached task exits while detached
  tasks are live, their remaining events never happen, so exiting is itself a visible operation).
- **Explicit yields**: `thread::yield_now`, `thread::park`/`park_timeout`, `thread::sleep` (Shuttle does
  not model time, so a sleep is just a switch), and `hint::spin_loop`, which calls `yield_now`. A
  `shuttle::rand` draw also consumes a step of the schedule, though not a task switch.

Operations that are **not** scheduling points, and so are invisible to Shuttle:

- **Plain memory access.** A `usize`, a struct field, a `Cell`, a `RefCell` — no switch, no record, no
  interleaving explored. Shuttle is not a data-race detector.
- **`std` primitives.** `std::sync::Mutex`, `std::sync::atomic`, `std::thread::spawn`, `std::sync::mpsc`,
  `std::thread_local!`, `std::sync::OnceLock`. This is the most common reason a Shuttle test finds nothing.
- **Compiler and CPU reordering.** Every atomic is modelled as `SeqCst`; weak memory is not modelled.
- **`unsafe` code that reimplements synchronization**, such as a hand-rolled spinlock over a raw
  pointer: no switch points, so Shuttle runs it straight through.
- **Anything outside the process's cooperative primitives**: I/O, time, other processes.

Every rule in [Determinism rules and common pitfalls](./pitfalls.md) is a consequence of this list.

## The execution loop

`Execution::run_to_completion` is the whole executor. Each iteration:

1. **`ExecutionState::schedule()`** — bumps the context-switch counter, checks the step bound, then walks
   `live_tasks` collecting runnable tasks. If nothing is runnable, or every runnable task is detached and
   no attached task is unfinished, it sets `Finished`; otherwise it passes the runnable slice, the current
   task id, and the `is_yielding` hint to `Scheduler::next_task`, where a `None` answer means `Stopped`.
2. **`advance_to_next_task()`** — makes the chosen task current and pushes its id onto the schedule.
3. **`continuation.resume()`**, inside `panic::catch_unwind`, wrapped in the task's tracing span.
4. The result: `true` means the task's function returned (mark it `Finished`), `false` means it
   switched out, `Err` means it panicked.

There is a fast path: when a task calls `thread::switch()`, `ExecutionState::maybe_yield` runs the
scheduler *from inside the task*, and if the same task is picked again it keeps running with no stack
switch. The step is still recorded, so this is invisible to the schedule.

**Deadlock detection** falls out of step 1: if the scheduler reports `Finished` but some attached task has
not, Shuttle panics with the list of stuck tasks (`format_for_deadlock`, including their backtraces when
`SHUTTLE_CAPTURE_BACKTRACE` is set). Detached tasks are excluded — one that never finishes is not an error.

**`max_steps`** bounds one execution. A "step" is an atomic region, all the code between two scheduling
points, and the bound is checked against schedule length minus `steps_reset_at`, so
`shuttle::current::reset_step_count()` lets a long test declare progress and restart the budget. The
default is `MaxSteps::FailAfter(1_000_000)`; `MaxSteps::ContinueAfter(n)` abandons the execution quietly
instead, which suits tests with intentional spin loops. See [Configuring test runs](./configuration.md).

## Determinism and the schedule

A `Schedule` (`shuttle-engine/src/scheduler/mod.rs`) is deliberately tiny:

```rust,ignore
pub struct Schedule {
    pub seed: u64,
    pub steps: Vec<ScheduleStep>,
}
pub enum ScheduleStep {
    Task(TaskId),
    Random,
}
```

That is all an execution is: a seed, plus a sequence of "run task *n*" and "draw a random value" decisions.
A `Random` step records only *that* a draw happened, not the value; values are regenerated by re-seeding
the same PCG RNG from `seed`. The schedule being built lives in a `thread_local!` (`CURRENT_SCHEDULE`)
deliberately kept *outside* `ExecutionState`: if we panicked while `ExecutionState` was borrowed and then
panicked again serializing the schedule, the double panic would abort the process and lose it.

`scheduler/serialization.rs` compacts a schedule into the string you paste into `replay`: a magic byte
`0x91`, three varints (the bit width needed for the largest task id, the step count, and the seed), then
the steps bit-packed — one leading bit per step, `1` for `Random`, `0` followed by `bitwidth` bits of
task id — hex-encoded and wrapped at 76 columns.

Because a schedule is just "run task *n*", it means nothing on its own. Replay requires that the same code
under the same configuration create the same task ids in the same order and reach scheduling points in the
same order; change the test body, add a synchronization operation, or introduce nondeterminism Shuttle does
not control, and `ReplayScheduler` will either fail an assertion ("scheduled task is not runnable") or replay
something else. [Debugging failures](./debugging.md) covers replay in practice.

## Data nondeterminism

`shuttle::rand` is a drop-in replacement for `rand` 0.8 whose `ThreadRng::next_u64` calls
`ExecutionState::next_u64`, which pushes a `Random` step onto the schedule and then asks the scheduler for a
value. Random choices are therefore part of the schedule and replay with it. Schedulers get their values
from a `DataSource` (`scheduler/data/`): `RandomDataSource` wraps a `Pcg64Mcg` whose `reinitialize()` draws a
fresh seed, re-seeds the RNG, and returns the seed then stored in the `Schedule` — that is how one `u64`
reproduces a whole stream of values. `FixedDataSource` instead re-seeds from the *same* constant every
execution, which is what `DfsScheduler` uses: DFS enumerates schedules, not data, so it pins the data to
keep the search deterministic. Note that `ThreadRng` is not actually thread-local: all tasks share one
stream, drawn in schedule order, which keeps draws ordered against context switches.

## Vector clocks and causality

With the `vector-clocks` feature enabled, each task carries a `VectorClock`: per-task counters with the
usual happens-before partial order (incomparable clocks mean concurrent events). Shuttle maintains it at
synchronization points — a spawn extends and increments, a `join` merges the finished thread's clock into
the joiner, a channel message carries the sender's clock to the receiver, a semaphore release stamps the
released permits so a later acquire inherits the releaser's clock. `mpsc.rs` and `batch_semaphore.rs`
have the most detailed comments on the reasoning.

Be precise about what this is for. Vector clocks are **not** a data-race detector: Shuttle does not observe
plain memory accesses, so it has nothing to check for races. They are consumed by exactly three things:
`shuttle::current::clock()`/`clock_for(task_id)`, for tools built on Shuttle that need a happens-before
relation; `ReplayScheduler::set_target_clock`, which prunes a replay to the events the target failure
causally depends on and skips concurrent ones (schedule minimization); and annotations, where the
[Shuttle Explorer](./explorer.md) draws causal edges between events.

The cost is real: a clock is a `SmallVec<[u32; 16]>` per task, cloned and merged at every synchronization
point, growing with the number of tasks. With the feature off, `VectorClock` compiles to a zero-sized
struct whose methods are no-ops. The feature is **off by default** in the `shuttle` crate, though
Shuttle's own test suite turns it on via a dev-dependency on itself. See
[Configuring test runs](./configuration.md) and [CI and performance](./ci-and-performance.md).

## Per-task storage

`std::thread_local!` keys the OS thread, and there is only one, so under Shuttle every task would share a
slot. Shuttle therefore provides `shuttle::thread_local!`, which expands to a `LocalKey<T>` holding only an
initializer; lookups go through `ExecutionState` into the current `Task`'s own `StorageMap`, keyed by the
address of the `LocalKey` static. `shuttle::lazy_static!` is the same idea one level up: the value lives in
the *execution*'s `StorageMap`, mediated by a `shuttle::sync::Once`, so each execution gets a fresh static.

`StorageMap` (`runtime/storage.rs`) preserves insertion order so destruction is deterministic, and
`Option`-wraps values so they can be destructed incrementally. TLS destructors are awkward, as the
comment on `Task::pop_local` explains: a destructor can synchronize (so it must run outside an
`ExecutionState` borrow — hence `pop_local` hands ownership to the caller) and it may *initialize* another
TLS slot (so destruction is a loop, and re-initializing an already-destructed slot is forbidden to
prevent an infinite one). One user-visible difference: Shuttle's `lazy_static` values *are* dropped at the
end of an execution, whereas the real `lazy_static` crate never drops them.

## The scheduler side

From the engine's point of view a scheduler is three methods:

```rust,ignore
pub trait Scheduler {
    fn new_execution(&mut self) -> Option<Schedule>;
    fn next_task(&mut self, runnable: &[&Task], current: Option<TaskId>, is_yielding: bool)
        -> Option<TaskId>;
    fn next_u64(&mut self) -> u64;
}
```

The scheduler outlives individual executions, so it can carry state between them. `new_execution` returning
`None` ends the test; `next_task` returning `None` abandons the current execution. `next_task` receives the
full runnable `Task` values, so it can read ids, names, labels, signatures, and clocks, and `is_yielding`
says the current task asked to yield and should be deprioritized. The engine always wraps the user's
scheduler in a `MetricsScheduler`, which counts steps, context switches, preemptions (a switch away from a
still-runnable task), and random choices, logging min/max/average at `info` level via `tracing`.

### Random

`RandomScheduler` re-seeds its RNG from the data source each execution, then
`runnable.choose(&mut self.rng)` — uniform over runnable tasks at each decision, which is *not* uniform
over interleavings. It keeps the seed in a drop guard so a panicking test prints one usable with
`check_random_with_seed`.

### PCT

`PctScheduler` implements Probabilistic Concurrency Testing from *"A Randomized Scheduler with
Probabilistic Guarantees of Finding Bugs"*, Burckhardt et al., ASPLOS 2010
([PDF](https://www.microsoft.com/en-us/research/wp-content/uploads/2016/02/asplos277-pct.pdf)), in
[Coyote](https://github.com/microsoft/coyote)'s variant rather than the paper exactly. Each task gets a
distinct random priority and the highest-priority runnable task runs; `d-1` random *change points* are
sampled from the observed step count, and when the counter reaches one the running task's priority drops to
the lowest, forcing a preemption. The first iteration runs oldest-task-first purely to learn a step count,
revised upward whenever a longer execution appears, and steps are only counted when more than one task is
runnable, following the paper's point that priority changes during sequential execution are unnecessary.
The parameter `d` is the "bug depth": PCT guarantees a probability of finding any bug of that depth.

### DFS

`DfsScheduler` enumerates schedules exhaustively, depth-first, keeping `levels: Vec<(TaskId, bool)>` —
per decision point, the choice made and whether it was the last option. It repeats the previous choice
while any deeper level has unexplored options, otherwise advances to the next runnable task and
truncates below; when no level has options left, `new_execution` returns `None`. Because it explores
schedules and not data, `next_u64` panics unless `allow_random_data` was set.

### URW

`UrwRandomScheduler` implements Uniform Random Walk from *"Selectively Uniform Concurrency Testing"*,
Zhao, Wolff, Mathur and Roychoudhury, ASPLOS 2025
([ACM](https://dl.acm.org/doi/abs/10.1145/3669940.3707214)). Plain random walk is biased towards
interleavings that keep short tasks alive; URW weights each runnable task by the number of events it has
*remaining*, sampling interleavings closer to uniformly. It estimates those counts from one trial execution
scheduled by vanilla random walk, accumulating per-`TaskSignature` event counts and then propagating them
from children to parents in reverse spawn order. Unseen signatures fall back to the minimum count seen.

### Uncontrolled nondeterminism

`UncontrolledNondeterminismCheckScheduler` wraps another scheduler and runs each schedule *twice*, once
recording and once checking. During the replay it asserts that the schedule ends at the same step, the
runnable ids at each decision are identical, `is_yielding` matches, and random draws occur in the same
positions; any divergence means nondeterminism Shuttle does not control. Its doc comment is careful to say
the converse does not hold: passing does not prove your test is deterministic, even with an exhaustive inner
scheduler. [Schedulers and check functions](./schedulers.md) covers choosing between all of these.

## Failure handling

**Capture.** `continuation.resume()` runs inside `panic::catch_unwind`, so a panicking task becomes
`StepError::TaskFailure(payload)` rather than unwinding out of the executor. Separately, `init_panic_hook`
installs a process panic hook (once, via `Once`) that prints the failing task's name and calls
`persist_failure` before delegating to the original hook; it runs *before* unwinding does any damage.

**Reporting.** `persist_failure` serializes the current schedule and, per `Config::failure_persistence`,
prints it (`Print`, the default), writes it to a numbered `scheduleNNN.txt` (`File`, created with `create_new`
so concurrent tests cannot collide), or does nothing (`None`). Shuttle then re-raises the original payload
with `resume_unwind`, so the failure looks like a normal test failure with a schedule attached.

**Backtraces.** Capture is off by default because it is expensive. Set `SHUTTLE_CAPTURE_BACKTRACE` and
`Task::block`/`Task::sleep` call `Backtrace::force_capture()`, storing it on the task; the deadlock message
then prints every stuck task's backtrace, and tells you when the variable is unset. Note that by default
Shuttle keeps scheduling after a panic until the panicking task has fully unwound, running `Drop` handlers
that may hit scheduling points; `UngracefulShutdownConfig::immediately_return_on_panic` stops instead.

## Crate map for contributors

| Crate | Contents |
|---|---|
| `shuttle` | Public API and documentation surface: re-exports, the `thread_local!` and `lazy_static!` macros, `rand`, and feature plumbing (`vector-clocks`, `annotation`). |
| `shuttle-engine` | The runtime: `Execution`/`ExecutionState`, `Task`, continuations, storage, failure reporting, the `Scheduler` trait and `Schedule`, `BatchSemaphore`, clocks, labels, `Config`. |
| `shuttle-schedulers` | Scheduler implementations (random, PCT, DFS, URW, round-robin, replay, annotation, uncontrolled-nondeterminism) and the `check_*` / `replay*` functions. |
| `shuttle-std` | Replacements for `std`: `sync` (mutex, rwlock, condvar, barrier, once, mpsc, atomics), `thread`, `future`. |
| `wrappers/*` | Drop-in replacements for third-party crates (`tokio`, `rand`, `parking_lot`, `dashmap`, `lazy_static`, `async-stream`, collections), each behind a `shuttle` feature. |
| `shuttle-explorer` | The VS Code extension that visualizes annotated schedules. |

- **A new synchronization primitive** goes in `shuttle-std/src/sync/` and should almost always be built on
  `BatchSemaphore`, which already handles fairness, waiter queues, wakers, clock propagation, and switch
  placement. If you place switches yourself, re-read the doc comment on `thread::switch` and record the
  commutativity argument in a comment.
- **A new scheduler** goes in `shuttle-schedulers/src/`, is exported from that crate's `lib.rs`, and
  usually gets a `check_*` wrapper in `check.rs`. Implementing `Scheduler` is all that is required.
- **A new wrapper** goes in `wrappers/<crate>/`: a facade crate named `shuttle-<crate>` with a `shuttle`
  feature that `cfg-if`s between the real crate and a Shuttle-backed implementation. See
  [Third-party crates and wrappers](./wrappers.md) and `wrappers/README.md`.

## Limitations that fall out of the design

None of these are bugs; each is the price of a decision above.

- **No real parallelism.** One task runs at a time, so bugs needing genuine simultaneity cannot occur. In
  exchange, Shuttle preempts at points a real machine almost never would.
- **No weak-memory modeling.** Atomics are effectively `SeqCst`, with no store buffer and no reordering,
  so a bug needing a `Relaxed` load to observe a stale value is invisible. Loom is the tool for that.
- **Wall-clock behavior differs.** `thread::sleep` is a context switch, not a delay, and there is no clock to
  advance, so timeout-based logic will not behave as it does in production.
- **Unsafe or `std`-based synchronization is invisible.** Raw atomics, assembly, FFI, and `std::sync` get no
  scheduling points, so Shuttle runs that code straight through.
- **Exhaustive checking does not scale.** Interleavings are exponential in the number of scheduling
  points, so `check_dfs` suits only tiny tests: the trade-off is randomized search and replay, not proof.

[Determinism rules and common pitfalls](./pitfalls.md) turns this mechanism into a checklist.
