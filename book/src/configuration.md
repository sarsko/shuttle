# Configuring test runs

The `check_*` functions from [Schedulers and check functions](./schedulers.md) are convenience
wrappers: each one builds a scheduler, pairs it with `Config::default()`, and runs your test. When
you need to change something about *how* the run happens — a longer step bound, a wall-clock budget,
schedules written to files instead of stderr — you build the `Runner` yourself and hand it a
`Config`.

## Building a runner by hand

A `Runner` is a scheduler plus a `Config`. You call `Runner::run` with your test body, and it runs
that body once per iteration until the scheduler says to stop:

```rust
# extern crate shuttle;
use shuttle::scheduler::RandomScheduler;
use shuttle::sync::{Arc, Mutex};
use shuttle::{thread, Config, MaxSteps, Runner};

let mut config = Config::new();
config.max_steps = MaxSteps::FailAfter(50_000);
config.stack_size = 0x2_0000;

let scheduler = RandomScheduler::new(100);
let runner = Runner::new(scheduler, config);

let iterations = runner.run(|| {
    let lock = Arc::new(Mutex::new(0u64));
    let lock2 = Arc::clone(&lock);
    let t = thread::spawn(move || {
        *lock2.lock().unwrap() += 1;
    });
    *lock.lock().unwrap() += 1;
    t.join().unwrap();
});

// `run` returns the number of iterations that actually ran.
assert_eq!(iterations, 100);
```

Two things to note. First, the *number of iterations* is a property of the scheduler
(`RandomScheduler::new(100)`), not of the `Config`; `Config` controls everything else, and `run`
returns how many iterations actually happened — which matters when a wall-clock budget cuts the run
short. Second, `Config` is `#[non_exhaustive]`, so you cannot build it with a struct literal from
outside Shuttle: start from `Config::new()` (identical to `Config::default()`) and assign the fields
you care about. The same applies to `MaxSteps`, `FailurePersistence`, `UngracefulShutdownConfig` and
`ContinuationFunctionBehavior` — if you `match` on one of those enums you need a wildcard arm.

### Running a portfolio of schedulers

`PortfolioRunner` runs several schedulers in parallel, each on its own OS thread with its own clone
of the `Config`. If any of them finds a failing execution, the whole run panics:

```rust,should_panic
# extern crate shuttle;
use shuttle::scheduler::{PctScheduler, RandomScheduler};
use shuttle::sync::{Arc, Mutex};
use shuttle::{thread, Config, PortfolioRunner};

let mut config = Config::new();
config.silence_warnings = true;

// `true` means: stop every scheduler as soon as any one of them fails.
let mut runner = PortfolioRunner::new(true, config);
runner.add(RandomScheduler::new(50));
runner.add(PctScheduler::new(2, 50));
runner.add(PctScheduler::new(5, 50));

runner.run(|| {
    let lock = Arc::new(Mutex::new(0u64));
    let lock2 = Arc::clone(&lock);
    thread::spawn(move || {
        *lock2.lock().unwrap() = 1;
    });
    assert_eq!(0, *lock.lock().unwrap());
});
```

Pass `false` for `stop_on_first_failure` if you would rather let every scheduler finish and find
multiple bugs; the panic that propagates is then one of possibly several failures.
`PortfolioRunner::run` returns `()` rather than an iteration count. Because each job runs on a real
thread, this is the one place where Shuttle uses genuine parallelism — inside a single job, execution
is still one task at a time. It is a good fit for CI, where you have cores to spare; see
[Performance and continuous integration](./ci-and-performance.md).

## The `Config` fields

| Field | Type | Default |
|---|---|---|
| `stack_size` | `usize` | `0xf000` (61,440 bytes) |
| `max_steps` | `MaxSteps` | `MaxSteps::FailAfter(1_000_000)` |
| `max_time` | `Option<Duration>` | `None` |
| `failure_persistence` | `FailurePersistence` | `FailurePersistence::Print` |
| `silence_warnings` | `bool` | `false` |
| `record_steps_in_span` | `bool` | `false` |
| `ungraceful_shutdown_config` | `UngracefulShutdownConfig` | `immediately_return_on_panic: false`, `continuation_function_behavior: Leak` |

### `stack_size`

The stack allocated to each task's continuation, in bytes. Default `0xf000`, about 60 KiB — much
smaller than a real thread's stack, because Shuttle allocates one per task and pools them.

Raise it when your test bodies recurse or hold large values on the stack. A task that overflows its
stack does not produce a nice Rust error; it crashes the process (a `SIGBUS` or `SIGSEGV`), so an
unexplained hard crash is a good reason to try a bigger `stack_size`. For reference, Shuttle's own
`tokio` wrapper sets `0x000F_0000` (about 960 KiB) because Tokio's machinery is stack-hungry.
`shuttle::thread::Builder::stack_size` overrides the config value for an individual spawned thread.

### `max_steps`

A *step* is an atomic region: all the code between two yieldpoints. Everything a task does between
acquiring and releasing a `Mutex`, for example, is one step. The step bound is checked before each
scheduling decision, and it exists to stop a single iteration running forever — a spin loop whose
partner thread the scheduler never picks would otherwise never terminate. Shuttle's schedulers make
no fairness guarantees, so this is not a hypothetical; see
[Determinism rules and common pitfalls](./pitfalls.md).

`MaxSteps` has three variants:

- **`FailAfter(n)`** (default `n = 1_000_000`): when the bound is hit, Shuttle persists the failing
  schedule and then panics with
  `exceeded max_steps bound 1000000. this might be caused by an unfair schedule (e.g., a spin loop)?`
  Because the schedule is persisted first, you can replay the offending iteration.
- **`ContinueAfter(n)`**: when the bound is hit, the current iteration stops where it is and the
  next iteration begins. Nothing is printed and no schedule is persisted — hitting the bound is not
  a failure. The stopped iteration still counts towards the scheduler's iteration count. This is the
  variant to use when long executions are expected and uninteresting rather than a bug, and it is
  also how you put a depth bound on an exhaustive `DfsScheduler` run.
- **`None`**: no bound at all. Only reasonable if you are confident the test terminates under
  *every* schedule, or if you are supplying a fixed schedule via a replay scheduler.

To pick a value, run the test once with a generous bound and see how long real executions get, then
set the bound a few times higher. Too tight and you get confusing `FailAfter` panics on perfectly
healthy executions; too loose and a livelocked iteration burns minutes before you hear about it. If a
test's step count scales with its workload (a queue test parameterized by message count, say), either
use `ContinueAfter`, or keep a tight bound and reset the counter as the test makes progress:

```rust
# extern crate shuttle;
# use shuttle::scheduler::RandomScheduler;
# use shuttle::{Config, MaxSteps, Runner};
# let mut config = Config::new();
# config.max_steps = MaxSteps::FailAfter(2_000);
# Runner::new(RandomScheduler::new(10), config).run(|| {
for _round in 0..100 {
    // ... do one round of work ...
    shuttle::current::reset_step_count(); // budget starts over from here
}
# });
```

That bounds *stalled progress* rather than total work, and lets you scale the test up without
touching the bound. Used carelessly it defeats the bound entirely and lets a test run forever.

### `max_time`

An optional wall-clock budget for the entire run, `None` by default. It is checked *between*
iterations only: it never interrupts an iteration that is already running, so a single pathological
iteration can overrun it — that is what `max_steps` is for.

The budget and the scheduler's iteration count are both stopping conditions, and the run ends at
whichever comes first. Exceeding the budget is not a failure; `Runner::run` simply returns the number
of iterations it managed. So "explore for ten minutes, however many iterations that is" is easy to
express: give the scheduler a huge iteration count and let the clock stop it. If you require a
minimum amount of exploration, assert on the returned count.

```rust,no_run
# extern crate shuttle;
use shuttle::scheduler::RandomScheduler;
use shuttle::{Config, Runner};
use std::time::Duration;

let mut config = Config::new();
config.max_time = Some(Duration::from_secs(60));

let iterations = Runner::new(RandomScheduler::new(usize::MAX), config).run(|| {
    // ...
});
assert!(iterations > 100, "only managed {iterations} iterations in 60s");
```

A time-bounded run is only reproducible in the sense that each failing iteration still replays
individually; how many iterations you get depends on the machine.

### `failure_persistence`

How Shuttle reports the schedule of a failing execution so you can replay it. Three variants:

- **`Print`** (default): the serialized schedule goes to stderr, followed by a line telling you to
  pass that string to `shuttle::replay`.
- **`File(Option<PathBuf>)`**: the schedule is written to a file in the given directory, or the
  process's current directory if `None`. Shuttle picks the first unused name of the form
  `schedule000.txt`, `schedule001.txt`, … creating it atomically, so parallel tests never collide.
  It then prints `failing schedule persisted to file: <path>` and tells you to pass that path to
  `shuttle::replay_from_file`. If the write fails for any reason — including a directory that does
  not exist — Shuttle falls back to printing the schedule. Use this when schedules are too long to
  paste comfortably into source, which happens quickly for tests with many steps.
- **`None`**: no schedule is emitted. You still see `test panicked in task '<name>'` and the
  original panic, but the failure is not replayable. Useful when Shuttle is driven by something that
  minimizes for you (proptest, for example) and the intermediate failures would just be noise.

`Print` pairs with `shuttle::replay`, which takes the schedule string; `File` pairs with
`shuttle::replay_from_file`, which takes the path. See
[Debugging failures: schedules and replay](./debugging.md) for what the schedule string contains and
how to work with it. One caveat worth knowing: part of this reporting happens from a process-wide
panic hook that Shuttle installs exactly once, capturing the `Config` of whichever execution ran
first. If you mix different `failure_persistence` settings across tests in a single test binary,
output for a failure may be produced according to the config that installed the hook.

### `silence_warnings`

`false` by default. When `true`, Shuttle suppresses two warnings about places where its model differs
from reality: that its `atomic` implementation is unsound and may miss bugs (Shuttle treats every
atomic operation as `SeqCst`), and that `lazy_static` values are dropped at the end of each
execution, unlike real ones. Both are worth reading once per codebase and are pure noise afterwards,
so silencing them in a test that legitimately uses atomics or `lazy_static` is normal. The
`SHUTTLE_SILENCE_WARNINGS` environment variable silences them process-wide regardless of this field.

### `record_steps_in_span`

`false` by default. Shuttle wraps each task's execution in a `tracing` span, which reads
`step{task=1}`. With this on, Shuttle calls `Span::record()` with the current step count each time
the task is scheduled.

Whether that helps depends entirely on your `Subscriber`. `tracing_subscriber::fmt` *appends* on
`record()`, so you get `step{task=1 i=3 i=9 i=12}` — unreadable for any task scheduled more than a
handful of times, which is why this defaults to off. A subscriber that overwrites instead gives you
one clean `step{task=1 i=12}`, which is useful for correlating log output with a schedule position.

### `ungraceful_shutdown_config`

This one is about what happens *after* your test has already failed — when a task panics, its stack
still has to unwind, and its drop handlers may perform Shuttle operations. The two fields, set as
`config.ungraceful_shutdown_config.immediately_return_on_panic = true;`, control that shutdown.

**`immediately_return_on_panic`** (default `false`). By default Shuttle serializes the failing
schedule and then keeps scheduling until the panicking task has fully unwound before returning. That
is wasteful, and it means running your code while `std::thread::panicking()` is true, where a second
panic aborts the process and takes the rest of your test binary with it. Setting this to `true` stops
scheduling as soon as a task panics, which is cheaper and shrinks that window — though it does not
close it, since Shuttle still resumes the unwind and a drop handler may panic then. Turn it on when a
failing test aborts instead of reporting a clean failure. One consequence: if a drop handler yields
back to the scheduler during unwinding, the failure surfaces as a panic with the payload
`Task panicked, and early return is enabled.` rather than your original panic message, which can
break `#[should_panic(expected = "...")]` matching.

**`continuation_function_behavior`** (default `ContinuationFunctionBehavior::Leak`). Shuttle pools
task continuations. When a continuation that was initialized but never actually run is dropped, its
closure — and everything the closure captured — has to be disposed of before the continuation can be
reused, and doing that mid-panic is exactly the double-panic risk above. So during a panic Shuttle
either `Leak`s the closure (`std::mem::forget`, the default) or `Drop`s it normally. The source
explains the default: most Shuttle tests are not written in a "collect" mode, so the volume of leaks
is low, and Shuttle already leaks the continuation itself, which is the bigger leak. Choose `Drop` if
your test accounts for resources released by those destructors and you are confident they cannot
panic. Both types are `#[non_exhaustive]`; the source notes other strategies (returning the
continuation function, handing it to a sacrificial thread) that are not implemented yet.

## Environment variables

These affect a whole test binary without touching any `Config`, which makes them the right tool for
a one-off debugging run or a CI job. The names of three of them are re-exported as constants from
`shuttle`.

| Variable | Constant | Effect |
|---|---|---|
| `SHUTTLE_RANDOM_SEED` | — | Seed for `RandomScheduler`, `UrwRandomScheduler` and `PctScheduler` |
| `SHUTTLE_SILENCE_WARNINGS` | `shuttle::SILENCE_WARNINGS` | Suppress the atomics and `lazy_static` warnings |
| `SHUTTLE_CAPTURE_BACKTRACE` | `shuttle::CAPTURE_BACKTRACE` | Capture and print task backtraces |
| `SHUTTLE_ANNOTATION_FILE` | `shuttle::ANNOTATION_FILE` | Where the `annotation` feature writes its JSON |

```sh
SHUTTLE_RANDOM_SEED=12345 SHUTTLE_CAPTURE_BACKTRACE=1 cargo test --features shuttle my_test
```

- **`SHUTTLE_RANDOM_SEED`** overrides the seed the scheduler would otherwise use — including a seed
  you passed explicitly to `check_random_with_seed` or `RandomScheduler::new_from_seed` — and Shuttle
  logs the seed it adopted at `info` level. The value must parse as a `u64`; if it does not, the test
  panics with `The seed provided by SHUTTLE_RANDOM_SEED is not a valid u64`. This is the fastest way
  to re-run a randomized suite on a seed that failed in CI.
- **`SHUTTLE_CAPTURE_BACKTRACE`** makes Shuttle capture a backtrace every time a task blocks or
  sleeps and include those backtraces in the deadlock panic message, so a deadlock report tells you
  where each task got stuck. Capturing backtraces is expensive: this is a debugging switch, not
  something to leave on in CI. Shuttle reminds you it is unset when it detects a deadlock.
- **`SHUTTLE_ANNOTATION_FILE`** only does anything with the `annotation` feature enabled, and
  defaults to `annotated.json` in the current directory.

`SHUTTLE_SILENCE_WARNINGS` and `SHUTTLE_CAPTURE_BACKTRACE` are checked for *presence* only: any value
at all, including `0` and the empty string, turns them on, and unsetting the variable is the only way
to turn them off.

## Crate features

`shuttle` has no default features. Three are worth knowing about.

### `vector-clocks`

Enables real vector clocks: Shuttle tracks a per-task clock and propagates it through every
synchronization operation, so `shuttle::current::clock()` and `clock_for()` return meaningful
causality information. Without the feature, `VectorClock` compiles down to a no-op stub whose `get()`
always returns 0 — the calls still work, but the answers are meaningless. Turn it on if you build
anything on top of the happens-before relation: linearizability checks, race detectors, or a
`ReplayScheduler` with `set_target_clock`. The cost is memory and time on every operation, which is
why it is off by default; see [How Shuttle works](./internals.md) for what the clocks track.

```toml
[dev-dependencies]
shuttle = { version = "0.9", features = ["vector-clocks"] }
```

### `annotation`

Enables recording of an annotated execution trace (and pulls in `serde`, `serde_json` and `regex`).
It provides the `AnnotationScheduler` and the `shuttle::annotate_replay` function, which replays an
encoded schedule while writing a JSON trace to `SHUTTLE_ANNOTATION_FILE` (default `annotated.json`)
for the Shuttle Explorer VS Code extension to visualize. See
[Annotations and Shuttle Explorer](./explorer.md).

### `bench-no-vector-clocks`

Do not enable this feature. Quoting `shuttle/Cargo.toml`:

> The following feature overrides the `vector-clocks` feature. It SHOULD NOT be used in production.
> Its purpose is solely to allow `cargo bench` to be run without vector clocks enabled, as they are
> otherwise always enabled via a dev-dependency to ensure all *test* assertions utilizing vector
> clocks behave correctly during testing

Shuttle depends on itself with `features = ["vector-clocks"]` as a dev-dependency, so clocks are
unconditionally on for its own tests and benchmarks need a way to opt out. Enabling it in your crate
silently degrades every clock to the no-op stub, quietly breaking anything that relies on
happens-before information.

## Recipes

**Fast local iteration.** Few iterations, a tight step bound so mistakes surface quickly, warnings
silenced, schedules printed so you can copy one straight into `replay`:

```rust
# extern crate shuttle;
# fn my_test() {
#     let t = shuttle::thread::spawn(|| {});
#     t.join().unwrap();
# }
use shuttle::scheduler::RandomScheduler;
use shuttle::{Config, FailurePersistence, MaxSteps, Runner};

let mut config = Config::new();
config.max_steps = MaxSteps::FailAfter(20_000);
config.failure_persistence = FailurePersistence::Print;
config.silence_warnings = true;

Runner::new(RandomScheduler::new(500), config).run(my_test);
```

**Nightly soak.** A wall-clock budget rather than an iteration count, a portfolio so several
schedulers explore at once, and schedules saved to a directory you can collect as build artifacts.
Make sure the directory exists — Shuttle falls back to printing if it cannot write the file:

```rust,no_run
# extern crate shuttle;
# fn my_test() {}
use shuttle::scheduler::{PctScheduler, RandomScheduler};
use shuttle::{Config, FailurePersistence, MaxSteps, PortfolioRunner};
use std::path::PathBuf;
use std::time::Duration;

let dir = PathBuf::from("target/shuttle-failures");
std::fs::create_dir_all(&dir).unwrap();

let mut config = Config::new();
config.max_time = Some(Duration::from_secs(30 * 60));
config.max_steps = MaxSteps::FailAfter(2_000_000);
config.failure_persistence = FailurePersistence::File(Some(dir));
config.silence_warnings = true;

// Keep going after the first failure so one night finds several bugs.
let mut runner = PortfolioRunner::new(false, config);
runner.add(RandomScheduler::new(usize::MAX));
runner.add(PctScheduler::new(2, usize::MAX));
runner.add(PctScheduler::new(5, usize::MAX));
runner.run(my_test);
```

**Debugging one known failure.** A replay scheduler runs the single schedule you already have, so
there is nothing to persist and no reason to bound steps. Run it under
`SHUTTLE_CAPTURE_BACKTRACE=1`, and turn on `immediately_return_on_panic` if the failure currently
aborts instead of reporting cleanly:

```rust,no_run
# extern crate shuttle;
# fn my_test() {}
use shuttle::scheduler::ReplayScheduler;
use shuttle::{Config, FailurePersistence, MaxSteps, Runner};

let mut config = Config::new();
config.failure_persistence = FailurePersistence::None; // we already have the schedule
config.max_steps = MaxSteps::None;
config.ungraceful_shutdown_config.immediately_return_on_panic = true;
config.record_steps_in_span = true; // if your subscriber overwrites on `record`

let scheduler = ReplayScheduler::new_from_file("schedule000.txt").expect("could not load schedule");
Runner::new(scheduler, config).run(my_test);
```

[Debugging failures: schedules and replay](./debugging.md) goes further into this workflow, including
`set_allow_incomplete` for schedules that no longer match the code.
