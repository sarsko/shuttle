# Schedulers and check functions

A Shuttle test runs your code on a single OS thread and decides itself when to switch between tasks. Every time
your code reaches a *yieldpoint* — acquiring a lock, sending on a channel, awaiting a future, spawning a thread —
the runtime stops, collects the set of tasks that are currently runnable, and asks a **scheduler** which one to
run next:

```rust,ignore
fn next_task(&mut self, runnable: &[&Task], current: Option<TaskId>, is_yielding: bool) -> Option<TaskId>;
```

That single decision is the whole game. The scheduler's policy determines which interleavings you visit, and
therefore which bugs you find and how many executions you need to find them. A uniformly random scheduler is a
fine default, but for some bug shapes a smarter policy finds the failure in ten executions instead of ten
thousand. See [How Shuttle works](./internals.md) for how the runtime creates and drives those scheduling
points; you rarely call `next_task` yourself.

## The `check_*` functions

Each `check_*` function constructs a scheduler, wraps it in a `Runner` with a default
[`Config`](./configuration.md), and runs your closure until the scheduler says to stop. The closure is
`Fn() + Send + Sync + 'static`, so it runs many times and must be self-contained. Failures are reported by
panicking, so a Shuttle test is just a `#[test]` that panics when some schedule violates your assertions. When
these functions aren't enough, drop down to a [`Runner`](#dropping-down-to-a-runner).

### `check_random`

**`pub fn check_random<F>(f: F, iterations: usize)`**

Runs `f` for `iterations` executions, choosing uniformly at random among the runnable tasks at every scheduling
point. Each execution gets a fresh seed from the scheduler's RNG, so the executions explore different
interleavings.

```rust,should_panic
# extern crate shuttle;
use shuttle::sync::{Arc, Mutex};
use shuttle::thread;

shuttle::check_random(|| {
    let counter = Arc::new(Mutex::new(0usize));
    let threads: Vec<_> = (0..2).map(|_| {
        let counter = counter.clone();
        // Read and write in *separate* critical sections: a lost update.
        thread::spawn(move || {
            let x = *counter.lock().unwrap();
            *counter.lock().unwrap() = x + 1;
        })
    }).collect();
    for t in threads { t.join().unwrap(); }
    assert_eq!(*counter.lock().unwrap(), 2);
}, 100);
```

The scheduler is seeded from the OS RNG, so a passing run tells you nothing about the next run — which is the
point. On failure, Shuttle prints the failing schedule (see [Debugging failures](./debugging.md)) and
`RandomScheduler` also prints the failing *seed*, which you can feed back through `check_random_with_seed` or the
`SHUTTLE_RANDOM_SEED` environment variable.

### `check_random_with_seed`

**`pub fn check_random_with_seed<F>(f: F, seed: u64, iterations: usize)`**

The same policy, but with the top-level seed supplied by you rather than by the OS. Use it when something else
owns randomness — most commonly `proptest`, which wants to generate and shrink the seed itself — or to reproduce
a run you have the seed for. Note that `SHUTTLE_RANDOM_SEED`, if set, overrides the seed you pass here.

### `check_pct`

**`pub fn check_pct<F>(f: F, iterations: usize, depth: usize)`**

Runs `f` for `iterations` executions using Probabilistic Concurrency Testing (PCT) with a bug depth of `depth`.
PCT gives every task a random priority, always runs the highest-priority runnable task, and picks `depth - 1`
random *priority change points*; at each change point the running task is demoted to the lowest priority,
forcing a preemption. So `depth` bounds the number of preemptions a schedule may make, where a preemption means
switching away from a task that was *still runnable*, as opposed to switching because it blocked.

Why bound preemptions? Because bug depth is empirically tiny: most real concurrency bugs are triggered by one or
two badly-timed preemptions at specific points, not by an exotic interleaving of a hundred switches. A uniformly
random scheduler spreads its probability mass over schedules with many preemptions, most of them equivalent to
each other; PCT concentrates it on the low-preemption schedules where the bugs live, and gives a probabilistic
lower bound on finding any depth-`d` bug. The algorithm comes from ["A Randomized Scheduler with Probabilistic
Guarantees of Finding Bugs"][pct] (Burckhardt et al., ASPLOS 2010); Shuttle follows Coyote's variant, which
learns the step bound dynamically from the first execution rather than requiring you to supply it. The trade-off:
raising `depth` lets each execution express more bugs but dilutes the probability of hitting any particular one,
so you need more `iterations` to keep the same per-bug probability. Start at depth 2 or 3; if you suspect a deeper
bug, prefer several small depths in a [portfolio](#portfoliorunner) over one large depth.

```rust,should_panic
# extern crate shuttle;
use shuttle::sync::{Arc, Mutex};
use shuttle::thread;

// Two locks acquired in opposite orders. Deadlocking needs two preemptions, so
// PCT at depth 1 never finds it; depth 2 finds it almost immediately.
shuttle::check_pct(|| {
    let lock1 = Arc::new(Mutex::new(0));
    let lock2 = Arc::new(Mutex::new(0));
    {
        let (lock1, lock2) = (lock1.clone(), lock2.clone());
        thread::spawn(move || {
            let _l1 = lock1.lock().unwrap();
            let _l2 = lock2.lock().unwrap();
        });
    }
    thread::spawn(move || {
        let _l2 = lock2.lock().unwrap();
        let _l1 = lock1.lock().unwrap();
    });
}, 1000, 2);
```

Watch the argument order: `check_pct(f, iterations, depth)` takes iterations first, while
`PctScheduler::new(depth, iterations)` takes depth first. PCT also requires the test to actually be concurrent —
it panics with `test closure did not exercise any concurrency` if no execution ever has two runnable tasks.

### `check_dfs`

**`pub fn check_dfs<F>(f: F, max_iterations: Option<usize>)`**

Exhaustively enumerates schedules by depth-first search, changing the choice at the deepest scheduling point
that still has unexplored options. It stops when every schedule has been explored, or after `max_iterations`
executions if you pass `Some(n)`. Note that the bound is on the *number of executions*, not on schedule depth;
there is no depth bound, so use [`Config::max_steps`](./configuration.md) to cap runaway executions. Exhaustive
search is only tractable for very small tests, since the number of schedules grows exponentially in the number
of scheduling points: reach for it to verify a hand-written concurrency primitive, not an integration test.

```rust
# extern crate shuttle;
use shuttle::sync::{Arc, Mutex};
use shuttle::thread;

// Small enough to enumerate: passing `None` explores every interleaving.
shuttle::check_dfs(|| {
    let counter = Arc::new(Mutex::new(0usize));
    let c = counter.clone();
    let t = thread::spawn(move || *c.lock().unwrap() += 1);
    *counter.lock().unwrap() += 1;
    t.join().unwrap();
    assert_eq!(*counter.lock().unwrap(), 2);
}, None);
```

`check_dfs` also forbids data nondeterminism: calling `shuttle::rand` under it panics, because random values
would make the search incomplete. If you need it anyway, construct the scheduler yourself as
`DfsScheduler::new(max_iterations, true)` — the second argument is `allow_random_data`, and it replays the same
fixed sequence of random values in every execution to preserve determinism.

### `check_urw`

**`pub fn check_urw<F>(f: F, iterations: usize)`**

Uniform Random Walk, from ["Selectively Uniform Concurrency
Testing"](https://dl.acm.org/doi/abs/10.1145/3669940.3707214) (Zhao et al., ASPLOS 2025). Plain random
scheduling is uniform over *choices*, not over *interleavings*: a task with two steps left is picked as often as
one with two thousand, which biases the walk. URW weights each choice by an estimate of how many events remain
on each task, so it samples interleavings closer to uniformly. Shuttle gets those estimates from a single trial
execution — the first iteration schedules with a plain random walk while counting events per task, and later
iterations use the resulting estimates. That makes URW most interesting when your tasks are very uneven in
length.

### `check_uncontrolled_nondeterminism`

**`pub fn check_uncontrolled_nondeterminism<F>(f: F, max_iterations: usize)`**

This one isn't looking for concurrency bugs in your code — it's looking for bugs in your *test*. It wraps a
`RandomScheduler` and, for each schedule it generates, immediately replays that schedule a second time, checking
that the set of runnable tasks and the sequence of random choices are identical both times. If they differ,
something outside Shuttle's control is steering the execution: `rand::thread_rng` instead of `shuttle::rand`,
real system time, `HashMap` iteration order, a real `std::thread`. Such a test's failures cannot be replayed, so
this is worth running once when you first write a test.

```rust
# extern crate shuttle;
use shuttle::rand::{thread_rng, Rng};
use shuttle::thread;

shuttle::check_uncontrolled_nondeterminism(|| {
    let t = thread::spawn(|| thread_rng().gen::<u64>() % 2);
    let _ = thread_rng().gen::<u64>();
    t.join().unwrap();
}, 100);
```

Swap `shuttle::rand::thread_rng` for `rand::thread_rng` and it panics with `possible nondeterminism: ...`. Two
caveats: every schedule runs twice, so the test does roughly twice the work of a `check_random` with the same
iteration count; and passing is not a proof, since the check only catches nondeterminism that actually perturbed
the schedules it happened to explore. See [Determinism rules and common pitfalls](./pitfalls.md) for the full list.

### `check`

**`pub fn check<F>(f: F)`**

Runs `f` exactly once under a round-robin scheduler. It explores a single deterministic interleaving, so it
finds essentially nothing; it is a smoke test that your closure runs at all under Shuttle. It's hidden from the
API docs, and you should reach for `check_random` instead. If you do want round robin for more than one
execution — for example to hold the task ordering fixed while data nondeterminism varies — build a
`RoundRobinScheduler::new(iterations)` and a `Runner` yourself. It's the only built-in scheduler with no
`check_*` wrapper of its own.

### `replay` and `replay_from_file`

**`pub fn replay<F>(f: F, encoded_schedule: &str)`**\
**`pub fn replay_from_file<F, P: AsRef<std::path::Path>>(f: F, path: P)`**

Run `f` exactly once following a schedule recorded from an earlier failure: the string Shuttle prints when a
test fails, or the file it writes when `Config::failure_persistence` is `FailurePersistence::File`. Replay is
deterministic as long as `f` has no nondeterminism beyond what Shuttle controls, which is why the previous section
matters. Pass the *same* test body that failed, plus the schedule string:

```rust,no_run
# extern crate shuttle;
shuttle::replay(|| {
    // ... the same test body that failed ...
}, "910102ccdedf9592aba2afd70104");
```

[Debugging failures](./debugging.md) covers the workflow in full, including using `ReplayScheduler` directly. With
the `annotation` feature enabled, `annotate_replay` records a trace for [Shuttle Explorer](./explorer.md).

## Dropping down to a `Runner`

The `check_*` functions all use `Config::default()`. When you need to change the configuration — a step bound, a
wall-clock time limit, writing failing schedules to files — construct the scheduler and a
[`Runner`](./configuration.md) yourself. All scheduler types are re-exported from `shuttle::scheduler`; the
randomized ones (`RandomScheduler`, `PctScheduler`, `UrwRandomScheduler`) also offer `new_from_seed` alongside
`new` for reproducible seeding.

```rust
# extern crate shuttle;
use shuttle::scheduler::PctScheduler;
use shuttle::{thread, Config, FailurePersistence, MaxSteps, Runner};

let mut config = Config::new();
config.max_steps = MaxSteps::ContinueAfter(50_000);
config.failure_persistence = FailurePersistence::File(None);

let runner = Runner::new(PctScheduler::new_from_seed(0x5EED, 3, 100), config);
let iterations = runner.run(|| {
    let threads: Vec<_> = (0..2).map(|_| thread::spawn(|| ())).collect();
    for t in threads { t.join().unwrap(); }
});
assert_eq!(iterations, 100);
```

`Runner::run` returns the number of executions it actually ran, which is useful when a `max_time` budget or an
exhausted DFS ends the test early.

## `PortfolioRunner`

Different policies find different bugs, and you rarely know in advance which one you need. A `PortfolioRunner`
runs a portfolio of schedulers on the same test body, each in its own OS thread with its own independent Shuttle
runtime. If any of them finds a failing execution, the whole run fails.

```rust,no_run
# extern crate shuttle;
use shuttle::scheduler::{PctScheduler, RandomScheduler};
use shuttle::PortfolioRunner;

// `true`: stop all jobs as soon as one fails. `false`: let them all finish, so
// you can find multiple independent bugs in one run.
let mut runner = PortfolioRunner::new(true, Default::default());
runner.add(PctScheduler::new(2, 10_000));
runner.add(PctScheduler::new(5, 10_000));
runner.add(RandomScheduler::new(10_000));
runner.run(|| {
    // ... test body ...
});
```

The parallelism is *between* schedulers, not within one: each job still executes its test body's tasks one at a
time on a single thread, which is what makes Shuttle deterministic. The portfolio just buys you three cores'
worth of independent exploration in the wall-clock time of one. Adding more schedulers than you have cores slows
each job down proportionally, so size the portfolio to your machine or CI runner — see [Performance and continuous
integration](./ci-and-performance.md).

## Which scheduler should I use?

| Situation | Use |
| --- | --- |
| Default choice, new test, no information | `check_random` |
| Hunting a specific suspected race or deadlock | `check_pct` at depth 2-5 |
| Verifying a small hand-written primitive | `check_dfs` |
| Tasks with wildly uneven lengths | `check_urw` |
| A test whose failures aren't reproducible | `check_uncontrolled_nondeterminism` |
| Reproducing a known failure | `replay` / `replay_from_file` |
| A long CI job, unknown bug shape | `PortfolioRunner` with random plus a few PCT depths |

On iteration counts, be honest with yourself: the count is a probability dial, not a correctness threshold, and
no number makes a Shuttle test sound. Reference points: 100 iterations reliably catches a bug reachable from a
single badly-timed switch between two threads; 1,000 is a reasonable default for a test with a handful of tasks;
tests with many tasks or deep bugs want 10,000 or more and belong in a nightly job rather than a pre-commit one.
Prefer a time budget (`Config::max_time`) over a huge iteration count when you care about CI wall-clock time,
and remember that a test that fails once in 10,000 executions has found a real bug — never dismiss it as
flakiness.

## Writing your own scheduler

The `Scheduler` trait has three methods, and implementing it is genuinely small:

```rust,no_run
# extern crate shuttle;
use shuttle::scheduler::{DataSource, RandomDataSource, Schedule, Scheduler, Task, TaskId};
use shuttle::Runner;

/// Always runs the *newest* runnable task, for a fixed number of executions.
#[derive(Debug)]
struct NewestTaskScheduler {
    iterations: usize,
    max_iterations: usize,
    data_source: RandomDataSource,
}

impl Scheduler for NewestTaskScheduler {
    fn new_execution(&mut self) -> Option<Schedule> {
        if self.iterations >= self.max_iterations {
            return None; // ends the test
        }
        self.iterations += 1;
        Some(Schedule::new(self.data_source.reinitialize()))
    }

    fn next_task(&mut self, runnable: &[&Task], _current: Option<TaskId>, _yielding: bool) -> Option<TaskId> {
        Some(runnable.iter().map(|t| t.id()).max().unwrap())
    }

    fn next_u64(&mut self) -> u64 {
        self.data_source.next_u64()
    }
}

let scheduler = NewestTaskScheduler {
    iterations: 0,
    max_iterations: 100,
    data_source: RandomDataSource::initialize(0),
};
Runner::new(scheduler, Default::default()).run(|| { /* ... test body ... */ });
```

The invariants the trait asks you to respect:

- `new_execution` returning `None` ends the test; `Some(schedule)` starts another execution. The `Schedule` you
  return carries the seed for that execution, and the runtime records the actual choices into it as they happen
  — that recording becomes the replay string, so returning a schedule seeded from a `DataSource` is what makes
  your scheduler's failures reproducible.
- `next_task` must return a `TaskId` drawn from `runnable`, which is guaranteed non-empty. `current_task` is
  `None` only before the execution has begun.
- Returning `None` from `next_task` is legal and stops exploration of the current schedule — that's how
  `ReplayScheduler` stops at the end of a recorded schedule, and how `PortfolioRunner` aborts jobs early. It is
  not a failure.
- `is_yielding` is a hint that `current_task` asked to yield, e.g. inside a spin loop. Ignoring it is allowed,
  but a scheduler that keeps picking a yielding task can spin forever; `Config::max_steps` is what protects you
  from that.
- `next_u64` supplies data nondeterminism to `shuttle::rand`. Derive it from a `DataSource` reseeded per
  execution rather than a global RNG, or replay will not work.

Schedulers compose, too: `UncontrolledNondeterminismCheckScheduler` and `AnnotationScheduler` are both wrappers
around an arbitrary inner `Scheduler`, and you can wrap your own the same way. See the [`shuttle::scheduler`
docs](https://docs.rs/shuttle) for the full API.

[pct]: https://www.microsoft.com/en-us/research/wp-content/uploads/2016/02/asplos277-pct.pdf
