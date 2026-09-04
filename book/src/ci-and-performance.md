# Performance and continuous integration

A Shuttle test is a loop that runs your test body many times, so its cost is a product of three
things:

```text
total time  ≈  iterations  ×  steps per iteration  ×  cost per step
```

Iterations are the number you pass to `check_random` or `check_pct`. A *step* is an atomic region —
all the code between two yieldpoints — so a loop that takes a `Mutex` a thousand times is a thousand
steps. Cost per step is what Shuttle's runtime spends switching continuations and doing its
bookkeeping; [How Shuttle works](./internals.md) explains where that goes.

Every lever below moves one of those factors, and they are not equally cheap to pull. Cutting
iterations costs bug-finding probability. Cutting steps per iteration is usually free, because it is
a property of the test body rather than of the bug. Cutting cost per step is mostly a matter of
compiling with optimizations and not leaving expensive diagnostics switched on.

## Measure before you tune

Shuttle reports its own metrics through `tracing` at `INFO` level. If your test binary installs a
subscriber (the Shuttle repo uses `test-log`), run with a filter that admits the engine:

```sh
RUST_LOG=shuttle_engine=info cargo test --release my_shuttle_test -- --nocapture
```

At the end of a run you get a `run finished` event with the iteration count and min/max/average
statistics for steps, context switches, preemptions, and random choices per iteration:

```text
INFO shuttle_engine::scheduler::metrics: run finished iterations=1000
  steps=[min=31, max=214, avg=88.4] context_switches=[min=12, max=96, avg=41.2]
  preemptions=[min=0, max=37, avg=9.1] random_choices=[min=0, max=0, avg=0.0]
```

Attack that average step count first: a test averaging tens of steps can afford tens of thousands of
iterations in a PR check, and a test averaging tens of thousands of steps cannot. Turn the logging
off again before you commit — see below.

## Build and run settings

**Run Shuttle tests in release mode.** This is the largest and cheapest win. Shuttle's per-step work
is a pile of small generic functions, `RefCell` bookkeeping, and continuation switches, none of which
is fast unoptimized. This repository's own CI runs the whole workspace optimized:

```sh
cargo nextest run --release --workspace
cargo test --release --doc --workspace
```

Make that your default too. If you need debug symbols in the optimized build — so that a `replay`
run is usable under a debugger — add them to the release profile rather than dropping back to a debug
build:

```toml
[profile.release]
debug = true
```

**Leave `RUST_LOG` off.** Shuttle enters and exits `tracing` spans around every context switch, so a
subscriber that accepts those spans turns into per-step work; the test job in this repo sets
`RUST_LOG: off` for exactly this reason. Relatedly, leave `Config::record_steps_in_span` at its
default `false`, since setting it adds a `Span::record` call on every switch.

**Do not set `SHUTTLE_CAPTURE_BACKTRACE` in CI.** It captures a backtrace for each task when tasks
block, which its own documentation calls "quite expensive". Enable it locally when chasing a
deadlock, not on every PR.

**Watch `stack_size` on wide tests.** Each task runs on its own continuation stack of
`Config::stack_size` bytes, default `0xf000` (60 KiB), so a test with 100 live tasks holds several
megabytes of stacks. Raising `stack_size` multiplies that — the tokio wrapper in this repo raises it
to just under 1 MiB, fine for a handful of tasks and expensive for hundreds. Stacks are pooled and
reused across iterations within one run, so the cost scales with concurrently-live tasks, not with
iterations.

## Feature flags and what they cost

`vector-clocks` makes the engine maintain a real vector clock per task, updated on every
synchronization operation, and exposes it through `shuttle::current::clock` and `clock_for`. Those
clocks let a test assert on causality, and the `annotation` feature records them so
[Shuttle Explorer](./explorer.md) can draw happens-before edges. Without the feature `VectorClock`
compiles to a no-op stub. Enable it when you use the clocks or produce annotations; leave it off for
plain interleaving search.

`annotation` is a development feature: it pulls in `serde`, `serde_json`, and `regex`, and an
`AnnotationScheduler` writes a JSON trace of the execution to disk. Turn it on in the command or job
that produces a trace, not in the fast PR check.

**Never enable `bench-no-vector-clocks` for a real test run.** Its comment in `shuttle/Cargo.toml`
says so directly — it "SHOULD NOT be used in production" and exists solely so `cargo bench` can run
without vector-clock overhead. `shuttle`'s dev-dependency on itself forces `vector-clocks` on for
tests and benches, and this feature *overrides* that at the `cfg` level. When it is set,
`VectorClock::get` returns `0`, comparisons always report `Equal`, and dereferencing yields an empty
slice — so assertions built on clocks pass vacuously and recorded annotations are meaningless. Cargo
features are additive across the build graph, so enabling it anywhere disables clocks everywhere.
Keep it confined to the benchmark job.

## Budgets instead of guesses

There are two ways to bound a run, and they solve different problems.

`Config::max_time` bounds the wall clock, but is only checked *between* iterations, so it cannot
interrupt one pathological iteration. `Config::max_steps` bounds a single iteration: the default
`MaxSteps::FailAfter(1_000_000)` turns a livelock — a task spinning while waiting for another the
scheduler never picks — into a test failure instead of a hung job, and `ContinueAfter` instead
abandons the over-long iteration and moves on, which is what you want in a soak where some randomly
chosen schedules are legitimately enormous. Use both: cap the run with time, cap the outliers with
steps, and let the iteration count run effectively unbounded. `Runner::run` returns how many
iterations it managed, which is worth logging so you know whether the budget was generous or binding:

```rust,no_run
# extern crate shuttle;
use shuttle::scheduler::RandomScheduler;
use shuttle::{Config, MaxSteps, Runner};
use std::time::Duration;

let mut config = Config::new();
config.max_time = Some(Duration::from_secs(60));
config.max_steps = MaxSteps::ContinueAfter(200_000);

let runner = Runner::new(RandomScheduler::new(usize::MAX), config);
let iterations = runner.run(|| {
    // test body
});
println!("completed {iterations} iterations within the budget");
```

See [Configuring test runs](./configuration.md) for the rest of `Config`.

Picking numbers: for a **PR check**, choose a per-test iteration count that keeps the whole Shuttle
job inside a minute or two — low hundreds to low thousands is typical for a test averaging under a
hundred steps — and set `max_time` as a backstop so a slow runner cannot hang the queue. For a
**nightly job**, invert it: give each test a wall-clock budget of minutes, let the iteration count run
as high as it goes, and use `ContinueAfter` so one unlucky schedule cannot eat the budget. Do not try
to make the PR check exhaustive; its job is to catch regressions in code you just touched, and the
nightly job's is to find the bug that needs a hundred thousand schedules.

## Making the test body cheap

Steps per iteration is the factor most under your control, and it multiplies every iteration.

- **Fewer tasks.** Two or three tasks find nearly all the races that ten find, and the space the
  scheduler has to sample grows sharply with task count.
- **Smaller loops.** Shrink counted loops in the body to the smallest count that can still expose the
  bug, usually two or three. A `for _ in 0..1000` multiplies your step count by a thousand for very
  little extra coverage.
- **No I/O, no sleeps, no timeouts.** Real sleeps burn wall-clock time every iteration and buy
  nothing, since Shuttle already controls the interleaving. They are also a determinism hazard; see
  [Determinism rules and common pitfalls](./pitfalls.md).
- **Hoist read-only setup out of the body.** The closure is `Fn + Send + Sync + 'static` and runs once
  per iteration, so anything expensive and immutable can be built once and captured:

```rust
# extern crate shuttle;
use shuttle::sync::Arc;
use shuttle::thread;

// Built once, shared read-only by every execution.
let corpus = Arc::new((0..1_000u64).collect::<Vec<_>>());

shuttle::check_random(
    move || {
        let corpus = corpus.clone();
        let t = thread::spawn(move || corpus.iter().sum::<u64>());
        assert_eq!(t.join().unwrap(), 499_500);
    },
    50,
);
```

What you must **not** hoist is the state under test. Hoisting an `Arc<Mutex<...>>` that tasks write to
compiles and looks like a saving, but every execution then observes leftovers from the ones before it,
so what the test sees depends on how many iterations preceded it and the failing schedule will not
replay. The rule: immutable inputs may be hoisted, anything a task writes to may not. See
[Determinism rules and common pitfalls](./pitfalls.md).

## More coverage per wall-clock minute

Once the body is cheap, buy coverage with parallelism and variety rather than a bigger iteration
count on one scheduler.

**Run a portfolio.** `PortfolioRunner` runs several schedulers in parallel OS threads against the
same body and fails the run if any of them finds a failure. On a multi-core runner that is nearly
free coverage, and mixing PCT depths is the cheapest hedge on how many preemptions your bug needs:

```rust,no_run
# extern crate shuttle;
use shuttle::scheduler::{PctScheduler, RandomScheduler};
use shuttle::{Config, PortfolioRunner};

// `true` stops every scheduler as soon as one of them fails.
let mut runner = PortfolioRunner::new(true, Config::new());
runner.add(RandomScheduler::new(20_000));
runner.add(PctScheduler::new(2, 20_000));
runner.add(PctScheduler::new(5, 20_000));

runner.run(|| {
    // test body
});
```

**Split schedulers across jobs.** A random job, a PCT-depth-2 job, and a PCT-depth-5 job running
concurrently cover more ground in the same wall-clock time than one job doing all three in sequence.
[Schedulers and check functions](./schedulers.md) covers what to expect from each.

**Vary the seed per run.** `check_random` draws a fresh seed from the OS on every run, so repeated
nightly runs already explore different schedules. When you want the seed to be a knob instead — so a
run is reproducible and a matrix can fan out over seeds — use `check_random_with_seed`, or set
`SHUTTLE_RANDOM_SEED`, which overrides the seed for `RandomScheduler`, `PctScheduler`, and
`UrwRandomScheduler`. Note that the environment variable wins over the seed passed in code, so
setting it globally in a job also overrides any regression test that pins its own seed.
`ReplayScheduler` is unaffected, so replay tests keep working either way.

**Shard across runners.** With `cargo nextest`, which this repo already uses,
`--partition count:N/M` splits the test list across `M` jobs.

## CI patterns

A fast Shuttle job on every pull request, in the style of this repo's `tests.yml`, sharded four ways:

```yaml
  shuttle:
    name: Shuttle tests (shard ${{ matrix.shard }})
    runs-on: ubuntu-latest
    strategy:
      fail-fast: false
      matrix:
        shard: [1, 2, 3, 4]
    env:
      RUST_LOG: off
    steps:
      - uses: actions/checkout@v3
      - name: Install Rust
        run: rustup update stable
      - name: Install nextest
        uses: taiki-e/install-action@nextest
      - name: cargo nextest
        run: cargo nextest run --release --features shuttle --partition count:${{ matrix.shard }}/4
```

A nightly soak, fanning out over seeds and PCT depths. `SOAK_ITERATIONS` and `SOAK_PCT_DEPTH` are
your own variables, read by your tests; `SHUTTLE_RANDOM_SEED` is Shuttle's:

```yaml
name: Shuttle soak
permissions:
  contents: read

on:
  schedule:
    - cron: '0 4 * * *'
  workflow_dispatch:

jobs:
  soak:
    name: Soak (seed ${{ matrix.seed }}, depth ${{ matrix.depth }})
    runs-on: ubuntu-latest
    strategy:
      fail-fast: false
      matrix:
        seed: [1, 2, 3, 4]
        depth: [2, 5]
    env:
      RUST_LOG: off
      SHUTTLE_RANDOM_SEED: ${{ matrix.seed }}
      SOAK_ITERATIONS: '200000'
      SOAK_PCT_DEPTH: ${{ matrix.depth }}
    steps:
      - uses: actions/checkout@v3
      - name: Install Rust
        run: rustup update stable
      - run: cargo test --release --features shuttle
```

`fail-fast: false` matters in both. A bug that only one shard or one seed reproduces is exactly the
failure you want to see, so do not let the first red job cancel the others.

## Making CI failures actionable

This is the part people get wrong. A Shuttle failure whose schedule you cannot recover is barely more
useful than a flaky stress test, and CI is where the schedule is easiest to lose: logs get truncated,
folded, or rotated.

Persist schedules to files and upload them. `FailurePersistence::File` writes each failing schedule
to `schedule000.txt`, `schedule001.txt`, and so on in the directory you name, and Shuttle then prints
the path and points the reader at `shuttle::replay_from_file`:

```rust
# extern crate shuttle;
use shuttle::{Config, FailurePersistence};
use std::path::PathBuf;

let mut config = Config::new();
config.failure_persistence = FailurePersistence::File(Some(PathBuf::from("target/shuttle-failures")));
```

Then upload that directory whenever the job fails:

```yaml
      - name: Upload failing schedules
        if: failure()
        uses: actions/upload-artifact@v4
        with:
          name: shuttle-schedules-${{ matrix.shard }}
          path: target/shuttle-failures/
          if-no-files-found: ignore
```

Belt and braces are worth it here, because the file only exists if the panic hook ran:

- **Keep the printed schedule too.** The default `FailurePersistence::Print` writes the schedule to
  stderr, which survives even if the artifact upload does not.
- **Log the seed.** When a `RandomScheduler` run fails, Shuttle prints a `failing seed` message
  telling you to pass that seed to `check_random_with_seed` or set `SHUTTLE_RANDOM_SEED`. A seed is
  one number: it fits in a log line and a bug report and survives everything. That is also the
  argument for a fixed seed per CI job — the job configuration itself then tells you what to rerun.
- **Say which job it was.** If you fan out over seeds, schedulers, and shards, put those values in the
  job name and the artifact name, so a maintainer can reconstruct the exact command.

## Triaging a failure only CI has seen

1. **Get the schedule.** Download the artifact and point `replay_from_file` at it, or paste the
   printed string into `replay`. Either runs the body exactly once, deterministically, on one OS
   thread — see [Debugging failures](./debugging.md).
2. **If it does not reproduce, suspect the test, not the bug.** Replay only works if the body is
   deterministic apart from the choices Shuttle controls; wall-clock time, real thread IDs, hashing by
   address, and `std` synchronization primitives all break that.
   `check_uncontrolled_nondeterminism` exists to find exactly this: it runs a random schedule, then
   replays it and checks that the replay matched. Run it before concluding anything about the code
   under test. [Determinism rules and common pitfalls](./pitfalls.md) lists the usual culprits.
3. **If it does reproduce, understand it, then write a targeted test.** Pasting the schedule string
   into a `#[test]` is a fine *temporary* step while you debug — one fast execution that fails until
   you fix the bug. But schedule strings do not survive code changes: the schedule is a sequence of
   scheduling decisions, so adding a lock acquisition or reordering two statements makes the same
   string drive a different execution, and the test either stops testing the bug or fails for an
   unrelated reason. Once you understand it, replace the pinned schedule with a small deterministic
   test that forces the problematic ordering directly, and keep a cheap `check_random` or `check_pct`
   run over the fixed code as the durable regression check.
4. **Feed it back into the budget.** If the nightly job needed 80,000 iterations to find it, that is
   evidence about the depth of bug your PR check will never catch.

## Benchmarking Shuttle itself

If you are changing Shuttle or comparing configurations, the workspace ships four Criterion
benchmarks in `shuttle/benches`, each declared with `harness = false` in `shuttle/Cargo.toml`:

- `lock` — threads that do nothing but take and drop a `Mutex`, so almost every step is a
  synchronization operation: a stress test of the core `Execution` logic. It also has a scaling group
  sweeping 4 to 1024 tasks against 1,000 to 100,000 total events.
- `counter` — threads, and async tasks, incrementing an `AtomicUsize` many times each: the per-step
  cost of the cheapest possible operation.
- `create` — many short, single-event tasks: task and continuation creation rather than stepping.
- `buffer` — a bounded-buffer producer/consumer over `Mutex` and `Condvar`, exercising the blocking
  and wakeup paths.

Each is parameterized over `PctScheduler` and `RandomScheduler` and over a "narrow" (5 tasks) and
"wide" (100 tasks) shape, so a regression usually shows up as a pattern across variants rather than
as one number. Run them the way CI does, with `bench-no-vector-clocks`, and use Criterion's
`--save-baseline`/`--baseline` to compare two revisions:

```sh
cargo bench -p shuttle --features bench-no-vector-clocks
cargo bench -p shuttle --features bench-no-vector-clocks --bench lock -- --save-baseline main
```

This repo automates the same comparison on every pull request: `bench.yml` runs
`boa-dev/criterion-compare-action` against the base branch with `cwd: shuttle` and
`features: bench-no-vector-clocks`. That job is `continue-on-error: true`, because benchmark numbers
on shared CI runners are noisy enough that gating a PR on them would be a nuisance — read it as a
signal to investigate, not as a gate.
