# Debugging failures: schedules and replay

Finding a concurrency bug is only half of what Shuttle gives you. The other half is the *schedule
string*: a compact encoding of every choice the scheduler made during the failing execution. Feed it
back to Shuttle and you get the same failure again, on demand, single-threaded, under a debugger.
This chapter walks that loop in the order you hit it — read the failure, grab the schedule, replay it,
add backtraces and tracing until you can see what happened, then narrow it down.

## Anatomy of a Shuttle failure

Shuttle installs a panic hook the first time you run a test, so Shuttle's own output always appears
*before* the normal Rust panic message.

### A task panicked or an assertion failed

```rust,should_panic
# extern crate shuttle;
use shuttle::sync::Mutex;
use shuttle::{check_dfs, thread};
use std::sync::Arc;

fn concurrent_increment_buggy() {
    let lock = Arc::new(Mutex::new(0usize));

    let threads = (0..2)
        .map(|_| {
            let lock = Arc::clone(&lock);
            thread::spawn(move || {
                let curr = *lock.lock().unwrap();
                *lock.lock().unwrap() = curr + 1;
            })
        })
        .collect::<Vec<_>>();

    for thd in threads {
        thd.join().unwrap();
    }

    // Both threads can read 0 and then both write 1.
    assert_eq!(*lock.lock().unwrap(), 2, "counter is wrong");
}

check_dfs(concurrent_increment_buggy, None);
```

The interesting part of the output is:

```text
Task failed, serializing schedule
test panicked in task 'main-thread'
failing schedule:
"
91021000904092940400
"
pass that string to `shuttle::replay` to replay the failure
```

followed by the ordinary `thread '<unnamed>' panicked at ...: counter is wrong` message and, if
`RUST_BACKTRACE` is set, its backtrace. The name in `test panicked in task '...'` is the task's
*spawn-time* name: `main-thread` for the test body itself, whatever you passed to
`thread::Builder::name` for a named thread, and `task-N` (the task id) for a thread or future spawned
without a name.

You may see the whole report more than once for one failure. By default Shuttle keeps scheduling
after a panic until the failing task has finished unwinding, and unwinding itself takes scheduling
steps (dropping a `MutexGuard` is one), so the schedule grows and is printed again. Use the **last**
one: it is the longest, so a replay will not run out of schedule part-way through the unwind. You can
switch this off with `Config::ungraceful_shutdown_config.immediately_return_on_panic` — see
[Configuring test runs](./configuration.md).

### The execution deadlocked

A deadlock is not a panic in your code: Shuttle detects that no task can make progress and panics on
your behalf.

```rust,should_panic
# extern crate shuttle;
use shuttle::sync::Mutex;
use shuttle::{check_dfs, thread};
use std::sync::Arc;

check_dfs(|| {
    let lock1 = Arc::new(Mutex::new(0usize));
    let lock2 = Arc::new(Mutex::new(0usize));
    let (l1, l2) = (Arc::clone(&lock1), Arc::clone(&lock2));
    thread::Builder::new().name("writer".to_string()).spawn(move || {
        let _g1 = l1.lock().unwrap();
        let _g2 = l2.lock().unwrap();
    }).unwrap();
    let _g2 = lock2.lock().unwrap();
    let _g1 = lock1.lock().unwrap();
}, None);
```

```text
Test deadlocked, and SHUTTLE_CAPTURE_BACKTRACE is not set. If either of those are set then the backtrace of each task will be collected and printed as part of the panic message.
deadlock! blocked tasks: [main-thread (task main-thread(0)), writer (task writer(1))]
```

The list contains every task that had not finished, formatted as `<name> (task <TaskId>)`, where
`<name>` is the spawn-time name or `<unknown>`, and `<TaskId>` uses the task's `TaskName` *label*
(hence `writer(1)`; see [Identifying who did what](#identifying-who-did-what)). Two extra markers can
appear inside the parentheses: `, detached` for a task whose handle was dropped, and
`, pending future` for a future that returned `Poll::Pending` and was never woken — usually the
smoking gun for a lost wakeup.

### The step bound was exceeded

```text
exceeded max_steps bound 1000000. this might be caused by an unfair schedule (e.g., a spin loop)?
```

Shuttle bounds the steps a single execution may take (`MaxSteps::FailAfter(1_000_000)` by default) so
that livelocks terminate, and it persists a schedule for this failure too. The usual cause is a
busy-wait loop that never yields — use
[`shuttle::hint::spin_loop`](https://docs.rs/shuttle/latest/shuttle/hint/fn.spin_loop.html), which
yields to the scheduler — or a test that legitimately needs more steps. A third message,
`no task was scheduled\nThis indicates an issue with the scheduler.`, means a custom `Scheduler`
returned a task Shuttle could not run: a bug in the scheduler, not in the code under test.

## The schedule string

The string is a hex-encoded, line-wrapped (at 76 characters) binary blob holding a version byte, the
number of bits per task id, the number of steps, the seed for the data source, and then one densely
packed entry per step. Each entry is either "run task *n*" or "produce the next random value". That
is all a Shuttle execution is — scheduling decisions plus a seed for `shuttle::rand` — which is why
the string is a complete reproduction. Nothing in it is human-readable; see
[Annotations and Shuttle Explorer](./explorer.md) for a timeline you can look at.

Whitespace is ignored by the deserializer, so a wrapped schedule pastes straight from your terminal
into a string literal. But a schedule is tied to **one exact test body**: steps name tasks by numeric
id, assigned in creation order, and scheduling points by position. Add a lock acquisition, spawn a
task earlier, change a loop bound, or otherwise shift where the yieldpoints fall, and the string no
longer describes the execution you care about. Strings from older Shuttle releases are rejected with
`invalid schedule`. Schedules are debugging artifacts and regression tests, not portable bug reports.

## Replaying a failure

[`replay`](https://docs.rs/shuttle/latest/shuttle/fn.replay.html) takes the test body and a schedule
string and runs the body exactly once.

```rust,no_run
# extern crate shuttle;
# fn concurrent_increment_buggy() { /* the body from the section above */ }
shuttle::replay(concurrent_increment_buggy, "91021000904092940400");
```

Errors you may hit while replaying:

| Message | Cause |
|---|---|
| `invalid schedule` | Not valid hex, or produced by an older Shuttle version. |
| `schedule ended early` | The schedule ran out of steps while tasks were still runnable — a truncated string, or a body that now takes more steps. |
| `scheduled task is not runnable, expected to run TaskId(1), but choices were [...]` | The body diverged: the task the schedule wants does not exist or is blocked. |
| `expected context switch but next schedule step is random choice` | The body consumed randomness where the recording did not. |
| `expected random choice but next schedule step is context switch` | The body skipped a random draw the recording made. |

The last three mean either that you edited the test since capturing the schedule, or that the body
contains nondeterminism Shuttle does not control (real `rand`, `Instant::now`, address-dependent
iteration order, I/O). See [Determinism rules and common pitfalls](./pitfalls.md) and
[`check_uncontrolled_nondeterminism`](https://docs.rs/shuttle/latest/shuttle/fn.check_uncontrolled_nondeterminism.html),
which hunts for exactly this.

### Persisting schedules to a file

Long schedules are unpleasant to paste into source. `FailurePersistence` decides where they go:
`Print` (the default) writes them inline as shown above; `File(None)` writes them to the current
directory and `File(Some(dir))` to `dir`; `None` persists nothing, which is what you want when a
harness such as `proptest` is minimizing and would otherwise emit a schedule per attempt.

```rust,no_run
# extern crate shuttle;
use shuttle::scheduler::RandomScheduler;
use shuttle::{Config, FailurePersistence, Runner};

# fn my_test() {}
let mut config = Config::new();
config.failure_persistence = FailurePersistence::File(None);
Runner::new(RandomScheduler::new(1000), config).run(my_test);
```

Shuttle takes the first unused name of the form `schedule000.txt`, `schedule001.txt`, …, creating the
file atomically so concurrent tests never collide, and writes nothing into it but the schedule string:

```text
failing schedule persisted to file: /path/to/crate/schedule000.txt
pass that path to `shuttle::replay_from_file` to replay the failure
```

```rust,no_run
# extern crate shuttle;
# fn my_test() {}
shuttle::replay_from_file(my_test, "schedule000.txt");
```

If the file cannot be written you get `failed to persist schedule to file (error: ...), falling back
to printing the schedule`, so a read-only working directory never costs you a reproduction.

### More control over replay

For anything beyond a straight replay, build a
[`ReplayScheduler`](https://docs.rs/shuttle/latest/shuttle/scheduler/struct.ReplayScheduler.html)
yourself:

```rust,no_run
# extern crate shuttle;
use shuttle::scheduler::{ReplayScheduler, Schedule};
use shuttle::Runner;

# fn my_test() {}
// Run task 0 twice, then task 1, then task 2.
let schedule = Schedule::new_from_task_ids(0, vec![0usize, 0, 1, 2]);
let mut scheduler = ReplayScheduler::new_from_schedule(schedule);
scheduler.set_allow_incomplete(); // stop quietly when the schedule runs out
Runner::new(scheduler, Default::default()).run(my_test);
```

`Schedule::new_from_task_ids` is the most readable way to commit a minimal reproduction as a
regression test. `set_allow_incomplete` turns `schedule ended early` and `scheduled task is not
runnable` into a clean stop, which is what you want after truncating a schedule by hand.
`set_target_clock` takes a [vector clock](./internals.md) and skips steps concurrent with (not
causally before) the failure, trimming unrelated interleaving out of the trace.

## Getting a stack trace

Set the `SHUTTLE_CAPTURE_BACKTRACE` environment variable (exported as the constant
`shuttle::CAPTURE_BACKTRACE`) and Shuttle records a backtrace every time a task blocks or a future
returns `Pending`. On a deadlock, each entry in the `blocked tasks` list is followed by the backtrace
captured at the moment that task got stuck — usually the exact line you need.

```sh
SHUTTLE_CAPTURE_BACKTRACE=1 cargo test --features shuttle my_deadlocking_test
```

The variable is checked for *existence*, not value, so `SHUTTLE_CAPTURE_BACKTRACE=0` enables it too.
Capture uses `Backtrace::force_capture`, so `RUST_BACKTRACE` is not needed for these — but it is
expensive, so turn it on while debugging and never in CI. For an ordinary panic, `RUST_BACKTRACE=1`
still gives the usual Rust backtrace, with a layer of `shuttle_engine::runtime` frames beneath yours.

### Attaching a debugger

This is where replay earns its keep. Shuttle runs one task at a time on a single OS thread, resuming
tasks as continuations, and a replay performs exactly one execution making exactly the recorded
choices — so breakpoints fire in the same order every time, stepping over a `lock()` lands you in
whichever task the schedule says runs next, and conditional breakpoints on iteration counters
actually work. Build the binary with `cargo test --features shuttle --no-run` and run it under
`rust-gdb` or `rust-lldb` with `<replay_test_name> --exact --nocapture`. The corollary: do not debug
a `check_random` run, convert it to a `replay` first.

## Tracing

Shuttle is instrumented with [`tracing`](https://docs.rs/tracing), and its span structure tells you
which task was running when each event was emitted. Install any subscriber in your test: the Shuttle
repository's own tests add `use test_log::test;`, which shadows `#[test]` with
[`test-log`](https://docs.rs/test-log)'s version (its `trace` feature installs a
`tracing-subscriber` per test and wires it to `RUST_LOG`); calling
`let _ = tracing_subscriber::fmt::try_init();` at the top of the test works just as well.

Three spans come from Shuttle, all at `ERROR` level so they survive aggressive filters:

- `execution{i=N}` wraps each iteration, `N` counting from 0 — this is how you tell which of your
  1000 executions produced a line.
- `step{task="TaskId(1)"}` wraps each step of a task, and is re-entered along with any of your own
  spans that were active when the task last yielded. The `task` field is the `Debug` form of the
  `TaskId`, so it shows the task's `TaskName` label when one is set.
- `new_task{parent=Some(TaskId(0)) i=3}` is emitted at task creation, containing a `DEBUG` event
  `created task` with the new task's id and signature.

A typical line reads `execution{i=7}:step{task="writer(1)"}: my_crate: acquired the lock`, so you can
follow one task through an execution by grepping for its `step` span. Setting
`Config::record_steps_in_span` to `true` additionally records the current step count into the span's
`i` field each time the task is scheduled. With a subscriber that overwrites on `record()` you get one
span per step:

```text
step{task=1 i=3}
step{task=1 i=9}
step{task=1 i=12}
```

With `tracing_subscriber::fmt`, which *appends* on `record()`, you get
`step{task=1 i=3 i=9 i=12}` growing as the task is rescheduled — which is why it is off by default.
Shuttle's spans and events come from the `shuttle_engine` and `shuttle_schedulers` crates:

```sh
RUST_LOG=shuttle_engine=trace cargo test --features shuttle my_test -- --nocapture --test-threads=1
```

Use `--test-threads=1`: `tracing`'s subscriber is global, and interleaved output from several tests
is unreadable. Filtering to your own crate (`RUST_LOG=my_crate=debug`) still gives you Shuttle's
spans as context, because spans are recorded regardless of whether their events pass the filter.

## Identifying who did what

`TaskId`s are small integers assigned in creation order, and on their own they are hard to keep
straight. `shuttle::current` makes output legible:

```rust
# extern crate shuttle;
use shuttle::current::{get_name_for_task, me, set_name_for_task};
use shuttle::{check_dfs, thread};

check_dfs(|| {
    let _ = set_name_for_task(me(), "coordinator");
    thread::Builder::new().name("writer".to_string()).spawn(|| {
        let name = get_name_for_task(me()).map(String::from);
        assert_eq!(name, Some("writer".to_string()));
    }).unwrap().join().unwrap();
}, None);
```

- `me()` returns the current `TaskId` (`get_current_task()` if you might be outside an execution), and
  `context_switches()` returns a monotonically increasing count usable as a global timestamp for
  events across tasks.
- `TaskName` is a reserved label. Setting it changes the `Debug` output of that `TaskId` everywhere —
  deadlock reports, `tracing` spans, scheduler output — from `TaskId(1)` to `writer(1)`.
- Prefer `thread::Builder::new().name(...)`, which names the task at spawn time, over a later
  `set_name_for_task`: a later name is also recorded into the already-created `step` span, so with an
  appending subscriber the span ends up with two `task` fields, like
  `step{task="main-thread(2)" task="Child"}`.
- **Labels are inherited by children.** A thread spawned without a name inherits its parent's
  `TaskName`, so an unnamed child of `main-thread` shows up as `main-thread(1)`. Name your tasks, or
  use `ChildLabelFn` to have the parent name children at spawn time.
- Labels are not just names: `set_label_for_task` / `get_label_for_task` store one value per type on a
  task, a convenient place to hang a role or shard id you want to see in a failure.

## Narrowing a failure

A schedule for a big test is a reproduction, not an explanation. To turn one into the other:

- **Shrink the test body.** Delete threads and operations until the failure stops reproducing, then
  put the last thing back. Every edit invalidates the schedule string, so re-find the failure with
  `check_random` after each change rather than reusing the old one.
- **Lower `max_steps`.** Setting `config.max_steps = MaxSteps::FailAfter(10_000)` turns "this test
  hangs" into a fast failure with a schedule attached. If the test legitimately takes many steps but
  makes visible progress, call `shuttle::current::reset_step_count()` at each progress point so the
  bound can stay low.
- **Isolate the primitive and switch to `check_dfs`.** Once the suspect is a single queue, semaphore
  or refcount, an exhaustive test over two or three tasks is both tractable and conclusive. See
  [Schedulers and check functions](./schedulers.md).
- **Characterize it with PCT depth.** `check_pct(f, iterations, depth)` bounds the preemptions it
  inserts. Run at depth 1, then 2, then 3: the lowest depth that reproduces the failure tells you how
  many badly timed preemptions the bug needs. A two-lock deadlock needs two and is invisible at
  depth 1 — both a description of the bug and a check that your regression test exercises it.
- **Visualize it.** With the `annotation` feature, `annotate_replay` records an annotated trace of a
  replayed schedule that the Shuttle Explorer VS Code extension renders as a task-by-task timeline.
  See [Annotations and Shuttle Explorer](./explorer.md).

## Silencing noise

Shuttle prints two warnings, each at most once per process, because both describe ways its model
differs from reality. The first fires when you use a non-`SeqCst` atomic ordering: Shuttle treats
every ordering as `SeqCst`, so bugs that need a weaker one are *missed*. The second fires when a
`lazy_static` value is dropped: Shuttle runs `Drop` at the end of each execution, unlike the real
crate, which can cause *false positives*. Both are suppressed by setting the
`SHUTTLE_SILENCE_WARNINGS` environment variable (exported as `shuttle::SILENCE_WARNINGS`; any value,
including empty) or `Config::silence_warnings` to `true`. Silence them once you have read them — the
first is a statement about which bugs your test cannot find.

For the rest of `Config` and the full list of `SHUTTLE_*` variables see
[Configuring test runs](./configuration.md); when the failure turns out to be the test's own
nondeterminism rather than a bug in your code, see
[Determinism rules and common pitfalls](./pitfalls.md).
