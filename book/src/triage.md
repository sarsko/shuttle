# Minimizing and triaging a failure

Someone hands you a red CI job or a bug report that says "this fails about one run in fifty".
[Debugging failures](./debugging.md) explains the tools; this chapter is the order to use them in, and
where the wrong order costs you the reproduction. The destination is a small deterministic test,
checked in, that fails before your fix and passes after it.

One rule shapes everything below: **a schedule is only valid for the exact test body that produced
it.** Every step names a task by numeric id and a scheduling point by position, so adding a lock
acquisition, deleting a thread, or enabling a log statement that takes a lock makes it describe
a different execution. Minimizing a Shuttle failure is therefore not shrinking the schedule; it is
re-finding the bug with a smaller body, re-capturing a schedule each round.

The running example is a hand-rolled "initialize once" whose check and act happen under two separate
lock acquisitions, so two callers can both observe `None` and both allocate:

```rust,should_panic
# extern crate shuttle;
use shuttle::sync::{Arc, Mutex};
use shuttle::{check_random_with_seed, thread};

fn get_or_init(cell: &Mutex<Option<usize>>, next_id: &Mutex<usize>) -> usize {
    if let Some(id) = *cell.lock().unwrap() { return id; }
    let mut next = next_id.lock().unwrap();
    *next += 1;
    let id = *next;
    *cell.lock().unwrap() = Some(id);
    id
}

fn reported_failure() {
    let cell = Arc::new(Mutex::new(None));
    let next_id = Arc::new(Mutex::new(0usize));
    let seen = Arc::new(Mutex::new(Vec::new()));

    let workers = (0..3)
        .map(|_| {
            let (cell, next_id, seen) = (cell.clone(), next_id.clone(), seen.clone());
            thread::spawn(move || {
                let id = get_or_init(&cell, &next_id);
                seen.lock().unwrap().push(id);
            })
        })
        .collect::<Vec<_>>();
    for w in workers { w.join().unwrap(); }

    let seen = seen.lock().unwrap();
    assert!(seen.iter().all(|id| *id == seen[0]), "handed out {seen:?}");
}

check_random_with_seed(reported_failure, 3695137018313461029, 100);
```

## 1. Capture the artifact before you touch anything

A failing run emits several artifacts of very different durability, and the useful ones are destroyed
by the most natural next action — editing the test. Collect them first. The run above prints, with the
repeated "pass that string to `shuttle::replay`" hints trimmed:

```text
Task failed, serializing schedule
failing schedule:
"
910227a59ae1e7a5f1f0a333002498b249d23269d3366d49920000
"
thread 'main' panicked at src/lib.rs:31:5:
handed out [1, 2, 3]
failing schedule:
"
910228a59ae1e7a5f1f0a333002498b249d23269d3366d49920000
"
failing seed:
"
3695137018313461029
"
```

Three artifacts, in increasing order of how well they survive a code change:

- **The schedule string.** Exact, and worthless the moment the body changes. Note there are *two*,
  differing by one step (`910227…` versus `910228…`): the panic hook serializes when the task panics,
  the runner again once unwinding finishes. Take the last, longest one.
- **The seed.** For a `RandomScheduler` run, `failing seed` is the seed of the execution that failed,
  and re-running with it puts that execution *first*, so the failure reproduces on iteration 1 with a
  byte-identical schedule. One number, and it survives log truncation, chat clients and bug trackers.
- **The panic message and the assertion that fired.** The only thing that survives a rewrite of the
  test, and therefore the thing to put in the commit message.

The seed trick works for `RandomScheduler` and `UrwRandomScheduler`, whose per-execution seeds come
from a data source re-seeded each iteration. `PctScheduler` carries one RNG across the whole run, so a
PCT failure at iteration 40,000 is only reproducible by re-running all 40,000 under the same
`SHUTTLE_RANDOM_SEED` — under PCT the schedule string is the only cheap artifact.

### Write schedules to files, and expect more than one per failure

`FailurePersistence::File` is the right setting for any unattended run;
[Performance and continuous integration](./ci-and-performance.md#making-ci-failures-actionable) covers
the job wiring and the upload. Two details matter only here. Because Shuttle serializes twice for a
panic that unwinds, `File` mode produces **two files** per failure — `schedule000.txt` and
`schedule001.txt` — of which the higher-numbered one is complete. And the numbering is global to the
directory, so a multi-threaded `cargo test` interleaves unrelated failures: pass `--test-threads=1`.

Tests going through `shuttle-tokio`'s `#[shuttle_tokio::test]` need no code edit to switch persistence
on: that harness reads `SHUTTLE_TRACE_DIR` and writes schedules there instead of to stderr.

```sh
mkdir -p ./failures
SHUTTLE_TRACE_DIR=./failures cargo test --release --features shuttle my_test
```

It also reads `SHUTTLE_ITERATIONS`, `SHUTTLE_TIMEOUT_SECS`, `SHUTTLE_SCHEDULER` and
`SHUTTLE_PCT_MAX_DEPTH`, which turn a PR check into a soak run from the command line; see
[Third-party crates and wrappers](./wrappers.md#tokio).

### For runs that die without unwinding

None of it happens if the process never unwinds: a `SIGKILL` from an OOM killer, a CI step timeout, a
task overflowing its `stack_size` (see [Configuring test runs](./configuration.md)), a second panic
during unwinding. The panic hook never runs, so nothing is printed and you are left with "the nightly
job died somewhere in 200,000 iterations".

`SHUTTLE_ALWAYS_PERSIST_SEED` is the insurance policy, and like the printed seed it is a
`RandomScheduler` feature only. Set it to a path and the scheduler writes the current execution's seed
there at the *start* of every iteration, overwriting the last one; whatever kills the process, the
file holds the seed of the execution that was in flight:

```sh
SHUTTLE_ALWAYS_PERSIST_SEED=./failures/last-seed.txt cargo test --release --features shuttle my_test
cat ./failures/last-seed.txt   # feed this to check_random_with_seed
```

### When all you have is scrollback

Schedule strings are hex, wrapped at 76 characters, and the deserializer strips all whitespace, so a
folded or line-numbered log pastes back in unchanged. Truncation does not: an odd number of hex
characters gives `invalid schedule`; an even number decodes into a header promising more steps than
are present, which panics inside Shuttle's deserializer rather than reporting a clean error.
`ReplayScheduler::set_allow_incomplete` rescues neither — it tolerates a schedule that *ends early*,
not a string that was cut off. Fall back to the seed.

## 2. Reproduce, then change nothing

Before forming any theory, confirm the failure is real and deterministic. Paste the last schedule into
`replay`, or point `replay_from_file` at the artifact you downloaded:

```rust,no_run
# extern crate shuttle;
# fn reported_failure() {}
shuttle::replay(reported_failure, "910228a59ae1e7a5f1f0a333002498b249d23269d3366d49920000");
shuttle::replay_from_file(reported_failure, "failures/schedule001.txt");
```

Both run the body once, making exactly the recorded choices. Check two things. **The failure is
the same one** — same assertion, same message, same task name. And **the schedule Shuttle prints
matches what you fed it**: `replay` uses `Config::default()`, so persistence is still `Print` and a
failing replay re-serializes what actually ran. Anything but byte-identical output, skip to
[step 5](#5-when-replay-does-not-reproduce).

Run it twice. Then stop, and resist every urge to add a `println!`.

## 3. Get information out without perturbing the execution

You now have one execution you can run at will. The point is to see inside it; the constraint is that
some diagnostics are free and others silently invalidate the schedule.

**Free: backtraces.** `RUST_BACKTRACE=1` is ordinary Rust and touches nothing.
`SHUTTLE_CAPTURE_BACKTRACE=1` makes Shuttle capture a backtrace whenever a task blocks or a future
sleeps, and print one per entry in a deadlock report. The capture happens inside Shuttle's own
`block`/`sleep` bookkeeping and never consults the scheduler, so it adds no scheduling points — safe
to switch on for a replay of a schedule recorded without it.

**Free: Shuttle's own tracing.** `RUST_LOG=shuttle_engine=trace` gives a `scheduling decision` event
per step, inside the spans described in [Debugging failures](./debugging.md#tracing). Shuttle's
instrumentation takes no locks, so its verbosity does not move the schedule.

**Not free: your tracing.** A log statement is evaluated only when its level is enabled, so this line
contributes two lock acquisitions — two scheduling points — at `RUST_LOG=debug` and none at
`RUST_LOG=off`:

```rust,ignore
// Two extra scheduling points, but only when DEBUG is enabled for this target.
tracing::debug!("cell is {:?}", cell.lock().unwrap());
```

Wrapper crates do this, and so do libraries you did not write. The consequence is that **a schedule
recorded at one verbosity level does not replay at another.** Replaying the example's schedule against
a body carrying one extra `cell.lock()` gives:

```text
scheduled task is not runnable, expected to run main-thread(1), but choices were [Task { id: main-thread(0), ... }]
```

That dumps the `Debug` of every runnable task, which is enormous but includes each task's
`signature.task_creation_stack`, so the source `Location` each candidate was spawned from is in there.

### `SHUTTLE_HIDE_TRACE`

The fix is not to give up logging; it is to decouple *whether the instrumentation runs* from *whether
you have to read it*. `SHUTTLE_HIDE_TRACE`, honoured by the `shuttle-tokio` harness behind
`#[shuttle_tokio::test]`, installs a subscriber whose `EnvFilter` still comes from `RUST_LOG`
but whose writer is `std::io::sink`. Every callsite `RUST_LOG` enables is still enabled, so every lock
inside a log statement is still taken and every scheduling point still happens; only the output is
thrown away, so the search runs at full verbosity without gigabytes of log:

```sh
RUST_LOG=trace SHUTTLE_HIDE_TRACE=1 SHUTTLE_TRACE_DIR=./failures \
  cargo test --release --features shuttle my_test
```

and the schedule it finds replays under the *same* filter with the output turned back on, which is
where you actually read it:

```sh
RUST_LOG=trace SHUTTLE_TRACE_FILE=./failures/schedule001.txt \
  cargo test --release --features shuttle my_test -- --nocapture
```

Two caveats. It is matched by value, not presence: only `SHUTTLE_HIDE_TRACE=true` and
`SHUTTLE_HIDE_TRACE=1` do anything, and `SHUTTLE_HIDE_TRACE=yes` silently does nothing — unlike
`SHUTTLE_CAPTURE_BACKTRACE` and `SHUTTLE_SILENCE_WARNINGS`, which are checked for presence. And it
goes in with `try_init`, so if a `test-log` attribute got there first, the sink has no effect.

Tests outside that harness have no equivalent switch, but the mechanism is three lines you can write
yourself: build a `tracing_subscriber::fmt::Subscriber`, give it an `EnvFilter::from_default_env()`,
set `.with_writer(std::io::sink)`. The load-bearing part is that the filter matches the one you will
replay under. More generally, **anything you add to the body to observe it is part of the body**: to
compare across verbosity levels, feature flags or `cfg` settings is to compare different programs.

## 4. Minimize the body, not the schedule

There is nothing to shrink in a schedule string. What you minimize is the test, and each round throws
the current schedule away. The loop:

1. **Delete something** — a thread, an operation, a phase, a layer of indirection. Delete rather than
   comment out; a body full of dead scaffolding is not smaller.
2. **Re-find the failure** with a fresh search at a *higher* iteration count than the one that
   originally found it, because you have just changed the population of interleavings. A smaller body
   is a cheaper body, so this is usually affordable.
3. **Re-capture the schedule** from the new failure, and use that one from here on.
4. **Repeat** until every remaining line is load-bearing.

For step 2, `check_random` is the default, but reach for `check_pct(f, iterations, depth)` when random
search stops finding it: PCT bounds the preemptions it inserts, which makes it far better at bugs
needing a specific preemption deep in a long execution. Once the interleaving space is finite, switch
to `check_dfs`; [Schedulers and check functions](./schedulers.md) covers what to expect from each.

For the running example, three workers, the `seen` vector and the id collection were all noise:

```rust,should_panic
# extern crate shuttle;
# use shuttle::sync::{Arc, Mutex};
# use shuttle::{check_dfs, thread};
# fn get_or_init(cell: &Mutex<Option<usize>>, next_id: &Mutex<usize>) -> usize {
#     if let Some(id) = *cell.lock().unwrap() { return id; }
#     let mut next = next_id.lock().unwrap(); *next += 1; let id = *next;
#     *cell.lock().unwrap() = Some(id); id
# }
check_dfs(
    || {
        let cell = Arc::new(Mutex::new(None));
        let next_id = Arc::new(Mutex::new(0usize));
        let (c2, n2) = (cell.clone(), next_id.clone());
        let other = thread::spawn(move || get_or_init(&c2, &n2));
        let mine = get_or_init(&cell, &next_id);
        assert_eq!(mine, other.join().unwrap(), "two ids for one cell");
    },
    None,
);
```

That fails within the first few interleavings `DfsScheduler` tries, with `left: 1, right: 2`, and the
failure is now a statement about the code rather than about a schedule. `check_dfs` uses a fixed data
source seed, so its schedule is stable across runs; this body produces `910111f8acd19101002802aa00`.

### When the smaller body stops reproducing

The normal case, not a setback. Three possibilities, in order of likelihood:

- **You deleted a thread the bug needs.** Put the last thing back and cut something else. Always keep
  the last body that reproduced; minimizing without a known-good fallback loses you an afternoon.
- **You deleted contention rather than the bug.** It is still there but needs a rarer interleaving, so
  the same iteration count no longer finds it. Raise the count by an order of magnitude, or try
  `check_pct` at depth 2 and 3, before concluding the cut mattered.
- **You deleted the assertion's reachability.** A check that now sits after a `join` serializing
  everything cannot fail — see [tests that pass vacuously](./pitfalls.md#tests-that-pass-vacuously).

## 5. When replay does not reproduce

Work down this list; the first two rows account for most of it.

| Symptom | Check |
|---|---|
| `scheduled task is not runnable` | The body changed since capture: an edit, a rebuild with different features, a different `RUST_LOG`, an upgraded wrapper crate that added a scheduling point. |
| `schedule ended early` | The same, or a schedule recorded under a `max_steps` bound that genuinely stops mid-execution — see [step 7](#schedules-that-stop-early). |
| `expected random choice but next schedule step is context switch`, or its mirror image | The body draws a different number of random values than the recording, so something branches on data the schedule does not carry. |
| Replay runs cleanly and the assertion passes | The nastiest case: the body is not a pure function of its schedule, and nothing diverged in a way the scheduler could see. |

For the last row, and for the others once you have ruled out an edit,
`shuttle::check_uncontrolled_nondeterminism(f, iterations)` is the instrument: it generates a random
schedule, replays it immediately, and checks the second run asks the scheduler the same questions in
the same order. The usual culprits, in the order they turn up: a `HashMap` or `HashSet` iteration that
decides what the test does, a `std::sync` primitive that leaked into the code under test (invisible to
the scheduler, so absent from the schedule), `rand::thread_rng` instead of `shuttle::rand`,
`Instant::now`, and state left in a `static` by the previous iteration. [Determinism rules and common
pitfalls](./pitfalls.md) has the catalog, and is worth reading before you conclude Shuttle is wrong.

## 6. Is it even a product bug?

A good share of first Shuttle failures are the harness, not the code. Three patterns to rule out.

**`max_steps` exceeded is a bound, not a hang.** When `MaxSteps::FailAfter(n)` trips, the runner
persists the schedule *first* and then panics, so this failure is as replayable as any other. Two
things about the report mislead people:

```text
failing schedule:
"
9101282a80880808aa020002828a
"
Task failed, serializing schedule
test panicked in task 'task-1'
thread 'main' panicked at shuttle-engine/src/runtime/execution.rs:192:37:
exceeded max_steps bound 40. this might be caused by an unfair schedule (e.g., a spin loop)?
```

`test panicked in task 'task-1'` names whichever task happened to be running when the bound tripped,
not a culprit, and the panic location is inside `shuttle-engine` rather than in your code — both signs
that no assertion of yours failed. Either the test legitimately needs more steps (raise the bound, or
call `shuttle::current::reset_step_count()` wherever real progress happens), or a wait loop never
yields; see [spin loops and busy waiting](./pitfalls.md#spin-loops-and-busy-waiting).

**A deadlock report can be a sender you forgot to drop.** `deadlock! blocked tasks: [...]` means no
task could be scheduled while an unfinished, non-detached task remained. A `Receiver` blocks while any
`Sender` is alive, a `sync_channel(0)` send blocks until someone receives — test bugs that look
like product deadlocks. `SHUTTLE_CAPTURE_BACKTRACE` plus the `, detached` and `, pending future`
markers usually settle it; [a deadlock report that is really a test
bug](./pitfalls.md#a-deadlock-report-that-is-really-a-test-bug) lists the rest.

**An assertion may only hold for one interleaving.** The most common way a Shuttle test fails on a
correct program is an assertion written while thinking about a single ordering. `assert_eq!(*counter,
2)` after joining two increments is fine; the same assertion *before* the joins is not, and neither is
asserting a work queue is empty when nothing has made the consumer run. Assert an invariant, not a
snapshot.

## 7. Land it

Two tests come out of a triage, and they do different jobs.

**The durable one is the minimized body.** The `check_dfs` test from step 4 rediscovers the
interleaving from scratch on every CI run, so it survives refactors of the code under test and fails
for a reason a reader understands without decoding hex. Check this in whenever the minimized body is
small enough for an exhaustive search.

**The cheap one is a pinned schedule.** When the bug needs a body too large for `check_dfs` and the
search that found it takes minutes, a saved schedule is a regression test costing exactly *one*
execution: no iteration count to tune, no wall-clock budget, no flakiness.

```rust,no_run
# extern crate shuttle;
# fn slow_pipeline_test() {}
/// Regression test for #1234: `get_or_init` performed its check and its act under two separate
/// lock acquisitions, so two callers could both observe `None` and both allocate an id. Found by
/// the nightly soak at PCT depth 3 after ~40,000 iterations; the committed schedule is the
/// minimal interleaving that exhibits it. This test proves the two callers agree on one id.
///
/// Valid only for this exact body: if you change `slow_pipeline_test`, re-run a search, re-capture
/// a schedule, and replace `tests/schedules/issue-1234.txt`.
#[test]
fn issue_1234_get_or_init_allocates_twice() {
    shuttle::replay_from_file(slow_pipeline_test, "tests/schedules/issue-1234.txt");
}
```

The comment is doing real work. A pinned schedule carries no information about what it proves and
nobody can recover that from the hex, so the comment records the original failure, the search that
found it, and the fact that the file must be re-blessed if the body changes — without that last
sentence, the first person to add a log line sees `scheduled task is not runnable` and deletes it.
Prefer a committed file to a string literal; it is what `FailurePersistence::File` already produced.

### Schedules that stop early

If the schedule came from a `max_steps` failure, or you truncated the recorded *steps* deliberately to
cut off everything after the interesting preemption, replay it under a `ReplayScheduler` with
`set_allow_incomplete`, which turns `schedule ended early` and `scheduled task is not runnable` into a
clean stop instead of a panic. Drop the step bound at the same time, or you just reproduce the bound:

```rust
# extern crate shuttle;
# use shuttle::sync::{Arc, Mutex};
use shuttle::scheduler::ReplayScheduler;
use shuttle::{thread, Config, FailurePersistence, MaxSteps, Runner};

# fn hot_loop() {
#     let lock = Arc::new(Mutex::new(0usize));
#     let other = lock.clone();
#     let t = thread::spawn(move || { for _ in 0..100 { *other.lock().unwrap() += 1; } });
#     for _ in 0..100 { *lock.lock().unwrap() += 1; }
#     t.join().unwrap();
# }
let mut config = Config::new();
config.max_steps = MaxSteps::None; // the bound produced this schedule; do not re-impose it
config.failure_persistence = FailurePersistence::None; // we already have the schedule

// Recorded from `hot_loop` under `MaxSteps::FailAfter(40)`, so it ends mid-execution.
let mut scheduler = ReplayScheduler::new_from_encoded("9101282a80880808aa020002828a");
scheduler.set_allow_incomplete();
Runner::new(scheduler, config).run(hot_loop);
```

`set_allow_incomplete` makes the replay *silent* when the schedule runs out, so a test relying on it
must assert something itself or it passes whether or not it reached the interesting part. It takes
`set_target_clock` (needs `vector-clocks`), which skips steps concurrent with the failure rather than
causally before it. Both appear in [Debugging failures](./debugging.md#more-control-over-replay), the
clocks themselves in [How Shuttle works](./internals.md).

Finally, write down the two numbers the triage produced. The iteration count and scheduler that found
the bug say what your PR check does *not* catch: if the soak needed 40,000 PCT-depth-3 iterations, a
500-iteration random check was never going to. And the depth that reproduces it describes the bug —
number of badly timed preemptions required. Both belong in the budget conversation in [Performance and
continuous integration](./ci-and-performance.md#budgets-instead-of-guesses).
