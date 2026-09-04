# Reference tests

Once your crate is wired up for Shuttle, finding interleavings stops being the hard part. Deciding what to
assert is. For a counter you can write `assert_eq!(count, 2)`; for a work-stealing deque, a connection pool,
or a sharded cache, the set of *correct* outcomes is large and order-dependent, so people write a few weak
assertions — "the length is plausible", "nothing panicked" — the test passes, and the bug ships.

A **reference test**, or reftest, sidesteps that. You drive a random sequence of operations against the
concurrent implementation from several tasks, and check what you observed against a *reference
implementation*: a sequential, single-threaded, deliberately dumb version of the same component, obviously
correct because there is nothing in it to get wrong. The reference *is* the assertion — you stop inventing
predicates and start comparing against a model. And because Shuttle controls the interleaving *and* the
randomness, a failure arrives as a schedule string you can [replay](./debugging.md) under a debugger, which
turns "the fuzzer found something" into "here is the bug".

## The shape of one

Four parts: the **component**, built on [`shuttle::sync`](./writing-tests.md) so Shuttle can preempt inside
it; the **reference**, a plain struct with no locks and no threads (and `Clone`, for Level 2); a **workload**,
one operation sequence per task drawn from [`shuttle::rand`](#generating-the-operation-sequence); and a
**comparison**, at one of two strengths. *Quiescent final-state equivalence* runs everything, joins
everything, then compares the implementation's final state against the reference fed the same operations in
some legal order — no history, no search. *Linearizability* records every operation and its result, with
bounds on when it happened, then asks whether any sequential order consistent with real time explains the
whole history. Start at Level 1; go to Level 2 when you care what operations *return*.

## Level 1: quiescent final-state equivalence

Here is a counter store — a map from key to count behind one mutex — whose `incr` has the classic bug: the
read and the write are separate critical sections, so two tasks can read the same value and one increment
vanishes.

```rust,should_panic
# extern crate shuttle;
use shuttle::rand::{thread_rng, Rng};
use shuttle::sync::{Arc, Mutex};
use shuttle::thread;
use std::collections::BTreeMap;

/// The component under test.
struct Counters { counts: Mutex<BTreeMap<u32, u64>> }

impl Counters {
    /// BUG: the read and the write are two separate critical sections.
    fn incr(&self, key: u32) -> u64 {
        let current = self.counts.lock().unwrap().get(&key).copied().unwrap_or(0);
        self.counts.lock().unwrap().insert(key, current + 1);
        current + 1
    }
}

/// The reference. No locks, no threads, nothing to get wrong.
#[derive(Clone, Default)]
struct Model { counts: BTreeMap<u32, u64> }

impl Model {
    fn incr(&mut self, key: u32) -> u64 {
        let slot = self.counts.entry(key).or_insert(0);
        *slot += 1;
        *slot
    }
}

shuttle::check_random(|| {
    // Each task's plan is a list of keys to increment. Drawn from Shuttle's RNG, so the plan is part
    // of the schedule and replays with it.
    let plans: Vec<Vec<u32>> = (0..2)
        .map(|_| (0..2).map(|_| thread_rng().gen_range(0..2u32)).collect())
        .collect();

    let counters = Arc::new(Counters { counts: Mutex::new(BTreeMap::new()) });
    let workers: Vec<_> = plans.iter().cloned().map(|plan| {
        let counters = Arc::clone(&counters);
        thread::spawn(move || plan.into_iter().for_each(|k| { counters.incr(k); }))
    }).collect();
    for worker in workers { worker.join().unwrap(); }

    // Increments commute, so "the reference in *some* legal order" is any order at all.
    let mut model = Model::default();
    for key in plans.iter().flatten() { model.incr(*key); }

    assert_eq!(*counters.counts.lock().unwrap(), model.counts, "final state diverged");
}, 50);
```

That last comment is what makes Level 1 cheap. In general, "the reference applied in *some* legal order"
means searching over orders — but if the operations you generate **commute** (increments, set insertion,
union, insert-if-absent on distinct keys) every order yields the same final state, so there is one thing to
compare against and the check is a single `assert_eq!`. Pick a commuting operation set for Level 1 whenever
you can; if you cannot, you are doing Level 2 with the real-time constraints discarded, which is more work
for a weaker result. Read-only operations are pointless here: they leave no trace in the state.

The failure spells out the divergence, which is most of the diagnosis. `check_random` draws a fresh seed each
run, so the exact numbers vary; one execution reports:

```text
assertion `left == right` failed: final state diverged
  left: {0: 1, 1: 2}
 right: {0: 1, 1: 3}
```

Three increments of key `1` produced a count of 2 — one update lost. Collapsing the read-modify-write into a
single critical section fixes it, and the same test passes:

```rust
# extern crate shuttle;
# use shuttle::{rand::{thread_rng, Rng}, sync::{Arc, Mutex}, thread};
# use std::collections::BTreeMap;
# struct Counters { counts: Mutex<BTreeMap<u32, u64>> }
# #[derive(Clone, Default)] struct Model { counts: BTreeMap<u32, u64> }
# impl Model { fn incr(&mut self, k: u32) -> u64 { let s = self.counts.entry(k).or_insert(0); *s += 1; *s } }
impl Counters {
    fn incr(&self, key: u32) -> u64 {
        *self.counts.lock().unwrap().entry(key).or_insert(0) += 1;
        self.counts.lock().unwrap().get(&key).copied().unwrap_or(0)
    }
}
# fn reftest() {
#     let plans: Vec<Vec<u32>> =
#         (0..2).map(|_| (0..2).map(|_| thread_rng().gen_range(0..2u32)).collect()).collect();
#     let counters = Arc::new(Counters { counts: Mutex::new(BTreeMap::new()) });
#     let workers: Vec<_> = plans.iter().cloned().map(|plan| {
#         let counters = Arc::clone(&counters);
#         thread::spawn(move || plan.into_iter().for_each(|k| { counters.incr(k); }))
#     }).collect();
#     for worker in workers { worker.join().unwrap(); }
#     let mut model = Model::default();
#     for key in plans.iter().flatten() { model.incr(*key); }
#     assert_eq!(*counters.counts.lock().unwrap(), model.counts, "final state diverged");
# }
shuttle::check_random(reftest, 50);
```

Note the `BTreeMap`: a `HashMap` would work as the state, but comparing or iterating one drags in `std`'s
per-process hash seed — nondeterminism Shuttle cannot replay. See [`HashMap` and `HashSet` iteration
order](./pitfalls.md#hashmap-and-hashset-iteration-order).

Look closely at that "fix", though. It increments under the lock and then takes the lock *again* to read the
value it returns, so another task's increment can land in between and `incr` can return a count it did not
produce. The final state is still exactly right, so Level 1 will never complain — that is the class of bug it
is structurally blind to, and a common one: correct state, wrong answer.

## Level 2: linearizability

Linearizability is the standard correctness condition for a concurrent object, and the right target for a
reftest. An execution is linearizable if the operations can be arranged into one sequence such that the
sequence is accepted by the reference — every operation returns what the reference returns — and the order
respects **real time**: if *a* finished before *b* started, *a* comes first, while operations that overlap may
be ordered either way. That last clause is what makes the check meaningful — without it almost any set of
results is explicable by some order; with it, a `get` that returned a stale value after the write that should
have been visible to it has nowhere to hide.

So we need a history: for each completed operation, what was called, what it returned, and a pair of
timestamps bracketing it. The checker is Wing and Gong's algorithm — repeatedly pick an operation that is
*minimal* in the real-time order among those not yet placed (nothing still unplaced finished before it
started), apply it to a clone of the reference, and recurse; if the return value does not match, or the
recursion fails, back it out and try the next candidate. It is ordinary sequential code with no Shuttle in it,
which is why the two `assert!`s below are worth keeping: they pin its behavior on hand-built histories. The
Shuttle half wraps each call: draw an operation, take a tick, call the implementation, take another tick, push
the entry. The clock is a `u64` behind a `shuttle::sync::Mutex` — see [Recording without breaking
determinism](#recording-without-breaking-determinism) for why it must not be `Instant::now()`. The
implementation under test is the "fixed" one from Level 1, whose final state is always right.

```rust,should_panic
# extern crate shuttle;
use shuttle::{rand::{thread_rng, Rng}, sync::{Arc, Mutex}, thread};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)] enum Op { Incr(u32), Get(u32) }

/// One completed operation: what was called, what it returned, and the ticks it spanned.
#[derive(Clone, Debug)] struct Entry { op: Op, ret: u64, start: u64, end: u64 }

#[derive(Clone, Default)] struct Model { counts: BTreeMap<u32, u64> }

impl Model {
    fn apply(&mut self, op: Op) -> u64 {
        match op {
            Op::Incr(key) => { let s = self.counts.entry(key).or_insert(0); *s += 1; *s }
            Op::Get(key) => self.counts.get(&key).copied().unwrap_or(0),
        }
    }
}

/// Is there a sequential order of `history`, consistent with the observed real-time order, that
/// `model` accepts? Exponential in the worst case: only ever call it on short histories.
fn linearizes(history: &mut Vec<Entry>, model: &Model) -> bool {
    if history.is_empty() { return true; }
    for i in 0..history.len() {
        // Skip anything that another unplaced operation strictly precedes in real time.
        let start = history[i].start;
        if history.iter().enumerate().any(|(j, o)| j != i && o.end < start) { continue; }
        let entry = history.remove(i);
        let mut next = model.clone();
        let accepted = next.apply(entry.op) == entry.ret && linearizes(history, &next);
        history.insert(i, entry);
        if accepted { return true; }
    }
    false
}

// Overlapping increments may be ordered so the one that returned 1 goes first. Two increments that do
// not overlap and both returned 1 have no explanation at all.
let e = |op, ret, start, end| Entry { op, ret, start, end };
assert!(linearizes(&mut vec![e(Op::Incr(0), 2, 1, 4), e(Op::Incr(0), 1, 2, 3)], &Model::default()));
assert!(!linearizes(&mut vec![e(Op::Incr(0), 1, 1, 2), e(Op::Incr(0), 1, 3, 4)], &Model::default()));

struct Counters { counts: Mutex<BTreeMap<u32, u64>> }

impl Counters {
    /// The final state is always correct, but the value returned may not be ours.
    fn incr(&self, key: u32) -> u64 {
        *self.counts.lock().unwrap().entry(key).or_insert(0) += 1;
        self.get(key)
    }
    fn get(&self, key: u32) -> u64 { self.counts.lock().unwrap().get(&key).copied().unwrap_or(0) }
}

/// A logical clock and a history, both shared by every task.
struct Recorder { clock: Mutex<u64>, history: Mutex<Vec<Entry>> }

impl Recorder {
    /// Not `Instant::now()`: this is derived from the interleaving, so it replays.
    fn tick(&self) -> u64 { let mut c = self.clock.lock().unwrap(); *c += 1; *c }
    fn record(&self, op: Op, ret: u64, start: u64) {
        let end = self.tick();
        self.history.lock().unwrap().push(Entry { op, ret, start, end });
    }
}

shuttle::check_random(|| {
    let counters = Arc::new(Counters { counts: Mutex::new(BTreeMap::new()) });
    let recorder = Arc::new(Recorder { clock: Mutex::new(0), history: Mutex::new(Vec::new()) });
    let workers: Vec<_> = (0..2).map(|_| {
        let counters = Arc::clone(&counters);
        let recorder = Arc::clone(&recorder);
        thread::spawn(move || {
            for _ in 0..2 {
                let op = if thread_rng().gen_bool(0.75) { Op::Incr(0) } else { Op::Get(0) };
                let start = recorder.tick();
                let ret = match op {
                    Op::Incr(key) => counters.incr(key),
                    Op::Get(key) => counters.get(key),
                };
                recorder.record(op, ret, start);
            }
        })
    }).collect();
    for worker in workers { worker.join().unwrap(); }

    let mut history = std::mem::take(&mut *recorder.history.lock().unwrap());
    assert!(linearizes(&mut history, &Model::default()), "not linearizable: {history:?}");
}, 100);
```

Four operations across two tasks is enough; the failure arrives within the first few iterations and reads like
a proof:

```text
not linearizable: [
  Entry { op: Incr(0), ret: 2, start: 2, end: 3 },
  Entry { op: Incr(0), ret: 2, start: 1, end: 4 },
  Entry { op: Incr(0), ret: 3, start: 5, end: 7 },
  Entry { op: Incr(0), ret: 4, start: 6, end: 8 } ]
```

Two increments both returned 2, and no sequential order of four increments does that whatever the timestamps
say. Returning the value produced *inside* the critical section makes every history linearizable and the
test passes:

```rust
# extern crate shuttle;
# use shuttle::sync::Mutex;
# use std::collections::BTreeMap;
# struct Counters { counts: Mutex<BTreeMap<u32, u64>> }
impl Counters {
    fn incr(&self, key: u32) -> u64 {
        let mut counts = self.counts.lock().unwrap();
        let slot = counts.entry(key).or_insert(0);
        *slot += 1;
        *slot
    }
}
# fn main() {}
```

Be clear-eyed about the checker's cost: in the worst case it explores every permutation of the concurrent
operations, so it is exponential in the history length. Six to eight entries is comfortable, a dozen is
noticeable, thirty will hang your suite. Cap the history in the workload, not in the checker, and remember
it runs once per execution. To go further, reach for Lowe's *P-compositionality* or a Jepsen-style checker.

## Generating the operation sequence

The operation sequence must come from [`shuttle::rand`](./writing-tests.md#data-nondeterminism-shuttlerand).
Every value it produces is drawn from the schedule's data source, so the draws are recorded alongside the
interleaving and one schedule string reproduces both the operations *and* the order they ran in. Use the
real `rand` crate and you get a test that finds bugs it cannot reproduce: replay fails with `expected
context switch but next schedule step is random choice`, or worse, silently diverges into a passing
execution. The same goes for the system clock, `HashMap` order, and pointer values — see [Determinism rules
and common pitfalls](./pitfalls.md). If the randomness lives in a dependency you cannot edit, use the
`shuttle-rand` wrapper from [Third-party crates and wrappers](./wrappers.md#rand).

`shuttle::rand`'s surface is deliberately small: `thread_rng()`, the `Rng` and `RngCore` traits re-exported
from `rand` 0.8, and `rngs::ThreadRng`. There is no `SeedableRng`, no `distributions` module, and no
`seq::SliceRandom`, so `slice.choose(&mut rng)` is unavailable — write the choice out with `gen_range` over
an index, as above. `thread_rng()` is shared by every task and cannot be re-seeded: one global draw order
per execution, which is exactly right here.

Either workload shape works. **Planning up front** (Level 1) draws everything before spawning, so the
reference gets the same multiset with no recording at all; **drawing inline** (Level 2) interleaves the draws
with scheduling decisions, giving Shuttle more to explore at the cost of needing a history.

## Recording without breaking determinism

Real-time bounds must not come from the clock. `Instant::now()` and `SystemTime::now()` read the real clock
under Shuttle — time is not modelled — so the timestamps differ every run, the history stops being a
function of the schedule, replay diverges, and
[`check_uncontrolled_nondeterminism`](./pitfalls.md#finding-the-source-check_uncontrolled_nondeterminism)
starts reporting it. A monotonic `u64` behind a `shuttle::sync::Mutex`, as in `Recorder::tick`, is the whole
fix: it is derived entirely from the interleaving, so it replays. `shuttle::current::context_switches()` is
an alternative if you would rather not carry a mutex — a monotonically increasing count usable as a global
timestamp — but it is not a scheduling point, and taking the lock is sometimes what you want.

Which brings us to the honest part: **recording perturbs the schedule.** Every `tick` and `record` is a lock
acquisition, hence a preemption point the uninstrumented component does not contain, so the Level 2 test is not
exploring quite the same program as the Level 1 test — keep both. It is unavoidable, since you cannot observe
an ordering without establishing one, and acceptable, because the instrumentation only *adds* synchronization:
every history it produces is one the real program could produce. Record at the test boundary, wrapping the
calls, never from inside the component, or you move the scheduling points in shipped code paths.

## Sizing and cost

Four dials interact, and turning them all up is the standard way to build a reftest that finds nothing. Keep
**operations per task** at two or three: each extra one multiplies the interleaving space and, at Level 2,
multiplies the checker's work factorially, and concurrency bugs are nearly always two-operation bugs. Keep
**tasks** at two or three — three find bugs two cannot, such as a reader observing a half-finished transfer
between two writers, and four almost never find something three did not. Keep the **key space** small, because
a bug needs two operations to *collide*: with sixteen keys and four operations most executions touch disjoint
state and test nothing, and a wider key space belongs in its own sharding test. **Iterations** are whatever
fits the budget ([Performance and continuous integration](./ci-and-performance.md)), measured rather than
guessed, since at Level 2 the checker rather than the execution is usually the dominant cost.

The counter-intuitive rule is that small histories find bugs. Two tasks doing two operations each on one key
has a search space a randomized scheduler covers a meaningful fraction of in a few dozen executions; eight
tasks doing twenty operations over a hundred keys has a space in which a thousand executions are a rounding
error, and each one is slow enough that you cannot afford a thousand more.

Which is the argument for turning the dials all the way down and switching schedulers. Once the configuration
is that small, [`check_dfs`](./schedulers.md) enumerates *every* interleaving and a pass becomes a proof for
that workload rather than a sample: lift the Level 1 body into `fn reftest(plans: &[Vec<u32>])` and
`check_dfs(|| reftest(&[vec![0, 1], vec![1, 0]]), None)` finishes in well under a second. The plan has to be
hard-coded, because `check_dfs` builds its `DfsScheduler` with `allow_random_data = false`: a call into
`shuttle::rand` panics with `requested random data from DFS scheduler with allow_random_data = false`.
Constructing `DfsScheduler::new(max_iterations, true)` and a [`Runner`](./configuration.md) yourself lifts
that, but it then uses one fixed sequence of random values for every execution — exhaustive over
interleavings, a single sample over operations. Prefer both tests: `check_dfs` over a hand-picked plan or two,
`check_random` or `check_pct` over generated ones.

## What a reference test does not catch

**Operations you did not generate.** The workload defines the test. If `remove` is not in your `Op` enum, no
number of iterations will find the bug in `remove`, so every new public method is a new variant and a reftest
that has drifted behind the API is narrower than it looks. Argument shapes count too: a key space of `0..2`
never exercises the resize path.

**Anything the reference also gets wrong.** The reference encodes your understanding of the spec, so a shared
misconception is invisible — both sides agree. That is the strongest reason to keep it *trivial*: a `BTreeMap`
and three lines has room for a typo but not for a design error, whereas a reference that itself uses a clever
algorithm is a second implementation you now have to trust. If it needs a comment explaining why it is
correct, it is too complicated.

**Relaxed-memory bugs.** Shuttle treats every atomic ordering as `SeqCst` — no store buffering, no reordering —
so a component that is correct under `SeqCst` and broken under `Relaxed` passes every reftest you can write,
and Shuttle warns once per process when it sees a weaker ordering. See [How Shuttle
works](./internals.md#limitations-that-fall-out-of-the-design); Loom is the tool for that.

**Simultaneity, time, and liveness.** One task runs at a time and `thread::sleep` is a context switch rather
than a delay, so dependence on real parallelism and any logic branching on elapsed time are outside the model.
And a reftest checks what happened, not that anything had to: deadlock detection is free (Shuttle panics with
`deadlock! blocked tasks: [...]` when no task can progress), but starvation and unfairness are not properties
of a single execution, so no assertion in the body can see them.

## Reference tests are not golden trace tests

These get conflated. A reftest compares results against a model computed *in the same run*, so it survives
refactoring; a golden test compares a recorded artifact against a checked-in file, and breaks whenever
internals move.

Shuttle has nothing that produces a behavioral trace for you to diff, and it is easy to assume otherwise
because `shuttle-tokio`'s `check` helper reads two environment variables that look like trace plumbing
([Read by the `shuttle-tokio` test harness](./configuration.md#read-by-the-shuttle-tokio-test-harness)):
`SHUTTLE_TRACE_DIR` sets
`Config::failure_persistence` to `File(Some(dir))`, so a failing run drops its schedule at
`dir/schedule000.txt`, and `SHUTTLE_TRACE_FILE` swaps the scheduler for a `ReplayScheduler::new_from_file` and
runs the body once against that schedule. Both deal exclusively in **schedules** — the opaque hex blob
described in [Debugging failures](./debugging.md#the-schedule-string), encoding scheduling decisions and an RNG
seed and nothing about your component's behavior. Persist one, commit it, replay it and you have a regression
test for one interleaving, worth doing after a reftest failure. But it is not human-readable, and any edit that
shifts where the scheduling points fall invalidates it. For something you can look at instead, see
[Annotations and Shuttle Explorer](./explorer.md).

## A checklist

- [ ] The reference is sequential, lock-free, derives `Clone`, and is short enough to be obviously correct;
      it and the checker are unit-tested on their own.
- [ ] The `Op` enum covers every public operation, and changing the API means revisiting it.
- [ ] Every operation, argument, and key comes from `shuttle::rand` — nothing from the real `rand`, the clock,
      addresses, or `HashMap` order — and state comparisons use ordered collections.
- [ ] Level 1: the operation set commutes, so "some legal order" is one `assert_eq!`. Level 2: real-time bounds
      come from a `Mutex<u64>`, recording wraps calls at the test boundary rather than inside the component,
      and the history is capped at single digits.
- [ ] Two or three tasks, two or three operations each, one or two keys; all handles joined (or
      `thread::scope`) so the final comparison runs at quiescence.
- [ ] A `check_dfs` variant over a hard-coded plan alongside the randomized one, with iteration counts
      measured rather than guessed, because at Level 2 the checker usually dominates.
- [ ] Each failure's schedule persisted and committed with `replay_from_file`, next to the reftest that found
      it.

From here: [Fault injection and failure modeling](./fault-injection.md) for making the operations themselves
fail, and [Minimizing and triaging a failure](./triage.md) for turning a reftest failure into a bug report.
