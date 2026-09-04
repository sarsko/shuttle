# Using LLMs for Shuttle work

A language model is good at the mechanical parts of Shuttle work and bad at the one judgement the
work actually requires: deciding whether a concurrency test tests anything at all.

That is not a general claim about language models; it is specific to the shape of a Shuttle test. A
Shuttle test that passes and a Shuttle test that is vacuous are nearly indistinguishable from the
outside: both compile, both are green, both run the iteration count you asked for and print nothing.
The difference lives entirely in whether the assertion could ever have been false and whether the
scheduler had any choice to make, and neither is visible in the diff. Meanwhile the surrounding work
is genuinely mechanical — swapping import paths across a hundred files, filling in a `cfg_if!`
template, writing a sequential model of a structure you have already described — and that part a
model does faster and more consistently than you do. So the useful question is which jobs to hand
over and how to check what comes back. [Determinism rules and common pitfalls](./pitfalls.md)
catalogues the ways a green Shuttle test can be worthless; this chapter is the subset a model walks
into unprompted, plus the review that catches it.

## Jobs to hand over

### Rewriting imports across a crate

Getting a crate ready for Shuttle means routing every concurrency import through one module whose
contents depend on a Cargo feature, as described in [Writing Shuttle tests](./writing-tests.md).
Setting up the module is a five-minute job. Converting the other two hundred call sites from
`std::sync::Mutex` to `crate::sync::Mutex` is tedious, easy to get 95% right and hard to get 100%
right, and no fun at all — a good description of work to delegate.

Two things make it a reasonable hand-off. The compiler checks most of it: rename an import that does
not exist and the build fails. And the *residue* is greppable, since the whole point of the pattern
is that afterwards `std::sync` and `std::thread` appear in exactly one file. Ask for the rewrite,
then grep for the leftovers rather than believing a summary that says it was complete.

Check by hand what the compiler cannot see: a `parking_lot` or `tokio` dependency that also needs
swapping, a `std::thread_local!` or `lazy_static!` that has to become Shuttle's macro, and any
`OnceLock` or `LazyLock`, which have no Shuttle equivalent and need restructuring, not renaming.

### A new wrapper crate

The wrapper crates in `wrappers/` are close to boilerplate: a `cfg_if!` that re-exports either the
real crate or a Shuttle-backed reimplementation, plus a `Cargo.toml` that forwards the wrapped
crate's features and pins its version. [Third-party crates and wrappers](./wrappers.md) shows the
template in full, and a model given that template produces a plausible skeleton in one shot. The
feature plumbing — remembering that the string before the slash is the dependency key and not the
package name — is exactly the kind of detail it gets right more reliably than a person does at the
end of an afternoon. The skeleton is not the work, though: the work is the `-impl` crate underneath,
where somebody decides what the primitive's blocking behaviour is in terms of `shuttle::sync`.

### The reference model and the operation generator

A reference test compares your concurrent data structure against a simple sequential model under
every interleaving ([Reference tests](./reftests.md)). Both halves of the scaffolding are good
delegation targets: the model, usually the obvious `Vec`- or `BTreeMap`-backed implementation of the
same interface, and the operation generator that draws a sequence of calls from `shuttle::rand`.
Neither contains concurrency reasoning, both are long, and both are mostly determined by an
interface you can paste in. What you keep is the linearizability argument — what it means for the
concurrent structure's observed history to be consistent with *some* sequential order of the model.
Get that wrong and the test compares the wrong things while looking thorough.

### Reading a log and naming the race

This is the job models are best at, and the most underused. A Shuttle failure comes with a `tracing`
log in which every event is wrapped in `execution{i=N}` and `step{task=...}` spans, so the
interleaving is right there in the output — just spread over a few thousand lines ([Debugging
failures](./debugging.md)). Asking which two operations from different tasks touched the same state
with no lock held between them is pattern matching over a long text, which is the thing to hand
over; name your tasks first so the log is legible. The answer is a hypothesis, and it is cheap to
check: shrink the test until only those two operations remain and confirm the failure survives.

### Turning a reproduction into a regression test

Once you have shrunk a failure to a few operations, converting it into a checked-in test is
mechanical: name it, move the state construction inside the closure, pick the check function, add
the `std` twin that runs in the normal test job. [Minimizing and triaging a failure](./triage.md)
covers the shrinking; the transcription afterwards is fine to delegate, with one condition — the
test must be shown failing against the unfixed code before it is shown passing against the fix.

## What to put in the prompt

Most bad Shuttle output comes from missing context rather than bad reasoning, and the context is
small enough to paste. Five things earn their space.

**The two determinism rules.** Every primitive must come from Shuttle, and an execution must be a
pure function of its schedule. Stated as rules they prevent a whole class of output; left implicit
they get rediscovered wrongly. The pair at the top of [the pitfalls chapter](./pitfalls.md) is short
enough to paste verbatim.

**What Shuttle actually implements.** Shuttle sees only the primitives it provides, so the swap
table in [Writing Shuttle tests](./writing-tests.md) is not trivia — it is the boundary of what the
test can explore. Include the gaps: no `OnceLock`, no `LazyLock`, all atomic orderings treated as
`SeqCst`, timeouts that never fire, `sleep` that does not sleep.

**The check-function menu.** `check_random`, `check_random_with_seed`, `check_pct`, `check_dfs`,
`check_urw`, `check_uncontrolled_nondeterminism`, `replay`, and `replay_from_file` are the whole
surface ([Schedulers and check functions](./schedulers.md)). Given the list, a model picks sensibly
between them; without it, it invents a plausible ninth.

**Release mode.** Shuttle does per-step bookkeeping that an unoptimized build does not compress, so
Shuttle tests run under `--release`; otherwise you get `cargo test --features shuttle` and a large
constant factor of avoidable cost.

**A demonstration that the test fails.** This is the important one. Require that the model run the
test against the *unfixed* code, paste the failure, and only then show the fix and the pass. A test
never observed failing is not evidence of anything, and this is the only requirement on the list
that cannot be met by writing more plausible-looking code.

A prompt that carries all five:

```text
You are writing a Shuttle test for the bug described below, in the crate I have pasted.

Shuttle runs all tasks as coroutines on one OS thread and chooses the interleaving itself.
Two rules follow, and both are hard requirements:

1. Every synchronization primitive and every thread inside the test must come from Shuttle:
   shuttle::sync (Mutex, RwLock, Condvar, Barrier, Once, mpsc, atomic), shuttle::thread,
   shuttle::future, shuttle::thread_local!, shuttle::lazy_static!, shuttle::rand,
   shuttle::hint::spin_loop. A std primitive still compiles and is invisible to the scheduler,
   so the test would pass for the wrong reason. Arc and Weak stay as std's.
2. The execution must be a function of the schedule alone. No wall-clock reads, no OS thread
   ids, no pointer addresses, no uncontrolled RNG, no HashMap iteration order affecting control
   flow, no I/O.

Shuttle has no OnceLock or LazyLock. It models all atomic orderings as SeqCst. Timeouts never
fire and thread::sleep is a plain yield point, so nothing may depend on elapsed time.

Choose one of: check_dfs(f, None) (exhaustive, for a body with few interleavings),
check_random(f, iterations), check_pct(f, iterations, depth). Say why you chose it, and justify
the iteration count or the PCT depth by how many preemptions the bug needs.

Build all shared state INSIDE the test closure. The closure is Fn, not FnMut, and runs once per
iteration, so anything constructed outside it accumulates across iterations.

Before you show me a passing test, do this and paste the raw output:
  1. cargo test --release --features shuttle <name>   against the UNFIXED code
     -> this must FAIL, and you must quote the panic message and the schedule string
  2. apply the fix
  3. the same command -> this must pass

If step 1 passes, the test is wrong. Do not adjust the assertion until it fails; work out why
no interleaving can violate it and tell me.

Do not invent a schedule string or a seed. Only ever quote one that a run you performed printed.
```

The step-1 requirement does most of the work: it converts "write a test" into "produce a witness".

## Failure modes

### Vacuous tests

A vacuous Shuttle test is one where no reachable interleaving can violate the assertion. It is not
specific to models; it is simply what gets produced when the objective is a test that passes.

The most common form is a join in the wrong place. Below, the increment is a read and a separate
write — the classic lost update — but each thread is joined immediately after it is spawned, so only
one thread is ever runnable and the scheduler has nothing to choose. `check_dfs` explores every
interleaving there is, finds one, and passes:

```rust
# extern crate shuttle;
use shuttle::sync::{Arc, Mutex};
use shuttle::thread;

shuttle::check_dfs(
    || {
        let counter = Arc::new(Mutex::new(0usize));
        for _ in 0..2 {
            let counter = Arc::clone(&counter);
            let handle = thread::spawn(move || {
                let value = *counter.lock().unwrap();
                *counter.lock().unwrap() = value + 1;
            });
            handle.join().unwrap(); // joined here, so nothing can interleave
        }
        assert_eq!(*counter.lock().unwrap(), 2);
    },
    None,
);
```

Nothing about that is obviously wrong. The bug is real, the primitives are Shuttle's, the assertion
is the right one, and the scheduler is the most thorough available. Spawn both threads before
joining either and the same body fails:

```rust,should_panic
# extern crate shuttle;
use shuttle::sync::{Arc, Mutex};
use shuttle::thread;

shuttle::check_dfs(
    || {
        let counter = Arc::new(Mutex::new(0usize));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let counter = Arc::clone(&counter);
                thread::spawn(move || {
                    let value = *counter.lock().unwrap();
                    *counter.lock().unwrap() = value + 1;
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(*counter.lock().unwrap(), 2);
    },
    None,
);
```

One line moved. That is the whole margin between a test that catches a lost update and a test that
certifies its absence.

The second form is state built outside the closure. Check functions take `F: Fn() + Send + Sync +
'static` — `Fn`, not `FnMut`, and called once per iteration — so hoisting setup out of the closure
"for efficiency" gives a body that only makes sense the first time through:

```rust,should_panic
# extern crate shuttle;
use shuttle::sync::{Arc, Mutex};
use shuttle::thread;

let counter = Arc::new(Mutex::new(0usize)); // outside the closure: never reset
shuttle::check_random(
    move || {
        let c = Arc::clone(&counter);
        let handle = thread::spawn(move || *c.lock().unwrap() += 1);
        handle.join().unwrap();
        assert_eq!(*counter.lock().unwrap(), 1);
    },
    10,
);
```

That one at least fails, on iteration 2, in a way that sends you hunting a concurrency bug that is
not there. The dangerous variant is the same mistake with an assertion loose enough to tolerate the
accumulation — `assert!(*counter.lock().unwrap() >= 1)` — which passes forever.

The third form is an assertion true by construction: `assert!(v.len() <= 100)` on a vector that only
ever gets two pushes, or a check that every value read was one of the values written in a test where
nothing could have produced any other value.

### Silently untested code

This is the one to worry about most, because the diff looks correct. If a `std::sync::Mutex`, a
`std::sync::atomic` operation, or a `std::thread::spawn` survives in the code under test, Shuttle
either sees nothing there or hangs. The atomics case is the quiet one: they never block, so the test
runs to completion, but with no scheduling point between a load and a store Shuttle treats the whole
read-modify-write as a single atomic step and the interleaving that loses an update is unreachable.
There is a runnable demonstration in [Determinism rules and common
pitfalls](./pitfalls.md#a-std-primitive-leaked-into-the-test): the same test fails immediately once
the path becomes `shuttle::sync::atomic`.

Models land here because `std::sync::Mutex` is far more common than `shuttle::sync::Mutex`, the
substitution is invisible to the compiler since the APIs match, and a model editing one file at a
time has no view of the crate-wide invariant that `std::sync` appears nowhere outside `src/sync.rs`.
The check is a grep, not a read: after any model-authored change, grep for `std::sync`,
`std::thread`, `parking_lot`, `tokio::`, `rayon`, `lazy_static`, and `once_cell`, and confirm every
hit is in the one import module or outside the tested path.

### Hallucinated API

Names that sound like Shuttle's but are not: `shuttle::sync::OnceLock`, `check_dfs_bounded`,
`shuttle::check_exhaustive`, `Config::max_iterations`, `shuttle::sync::atomic::AtomicCell`. This is
the least dangerous failure mode, because the compiler arbitrates immediately:

```text
error[E0432]: unresolved import `shuttle::sync::OnceLock`
 --> src/lib.rs:2:5
  |
2 | use shuttle::sync::OnceLock;
  |     ^^^^^^^^^^^^^^^^^^^^^^^ no `OnceLock` in `sync`

error[E0425]: cannot find function `check_dfs_bounded` in crate `shuttle`
```

It is worth naming anyway, for two reasons. Accepting rustc's suggested import here — it offers `use
std::sync::OnceLock;` — is how you get the previous failure mode. And the same tendency applies to
what the compiler cannot catch: environment variables and feature names. Shuttle reads exactly five
variables — `SHUTTLE_CAPTURE_BACKTRACE`, `SHUTTLE_SILENCE_WARNINGS`, `SHUTTLE_RANDOM_SEED`,
`SHUTTLE_ALWAYS_PERSIST_SEED`, `SHUTTLE_ANNOTATION_FILE` — and has three features, none on by
default: `vector-clocks`, `annotation`, `bench-no-vector-clocks`. A misspelled variable is silently
ignored, and while an unknown Cargo feature is an error, a *wrong* known one is not.

### Invented schedule strings and seeds

A schedule string is a sequence of task ids and random draws indexed by scheduling point, produced
by one run of one revision of one program; it means nothing apart from that provenance. A model
asked for a replay example will produce a hex string of the right shape and it will not replay
anything. Nor will a real string against slightly different code: mutating a single hex digit of one
that does reproduce a failure gives

```text
thread 'main' panicked at shuttle-schedulers/src/replay.rs:87:21:
expected context switch but next schedule step is random choice
```

and a string that is not well-formed at all fails further down, inside `bitvec`, with a range error
that looks like a Shuttle bug and is not. The rule is absolute: a schedule string or seed belongs in
a document only if a run you performed printed it. Even genuine strings expire, since adding a lock
acquisition or changing how many random numbers you draw shifts every subsequent step. When a
failing interleaving is worth keeping, keep the shrunk test under `check_dfs` rather than the
schedule; see [Debugging failures](./debugging.md) and [Minimizing and triaging a
failure](./triage.md).

### Numbers from nowhere

`check_random(f, 1000)` is the number a model writes when it has no opinion, and `check_pct(f, 1000,
3)` is the same non-answer with an extra parameter. Neither is wrong, since 1000 is a reasonable
default, but the number should follow from something: how many interleavings the body has, how many
preemptions the bug needs, how much wall-clock time the job has. If the body is small enough for
`check_dfs` to finish, the count is not a judgement call at all and the right argument is `None`. If
it is not, the PCT depth decides whether a bug requiring three well-placed preemptions is reachable,
and `depth = 1` never finds it. Ask for the reasoning; a model that cannot supply one has not
thought about the interleaving space, which is the only thing the test is for.

The related omission is `--release`. Shuttle's per-step bookkeeping is a pile of small generic
functions and continuation switches, none of which is fast unoptimized, so the same test is much
slower in a debug build — a CI budget problem that is really a build-flag problem. See [Performance
and continuous integration](./ci-and-performance.md).

## A review rubric

Run this over any Shuttle test you did not write. Each item is a grep, a one-line edit, or one run.

1. **Break the code and confirm the test fails.** Invert a comparison, remove a lock, widen a
   critical section. If the test still passes, stop here — nothing else matters.
2. **Grep for `std::sync`, `std::thread`, `parking_lot`, `tokio::`, `rayon`, `lazy_static`,
   `once_cell`.** Every hit should be in the single import module or outside the tested path.
3. **Check where the state is constructed.** It must be inside the closure. If anything is captured
   from outside, ask what it looks like on iteration 2.
4. **Check where the joins are.** Handles spawned in a loop should be collected and joined after the
   loop, or wrapped in `thread::scope`. A join inside the spawning loop serializes the test.
5. **Read the assertion and name the interleaving that makes it false.** If you cannot, the test is
   vacuous until proven otherwise. Invariants asserted only after all joins are the usual tell.
6. **Check the iteration count or PCT depth against a stated reason.** No reason means no coverage
   argument.
7. **Run `check_uncontrolled_nondeterminism` on the body.** It replays each random schedule
   immediately and reports divergence, which catches the ambient-nondeterminism class cheaply.
8. **Verify every quoted schedule string, seed, panic message, and environment variable** by running
   the thing or grepping the source. Assume each is fabricated until it is not.
9. **Confirm the command line says `--release`**, and that `RUST_LOG` is not left on in CI.
10. **Ask what the test does not cover.** Weak memory orderings, data races through `UnsafeCell`,
    and real time are outside Shuttle's model however the test is written; a model will not say so.

## What this book got wrong

Parts of this book were drafted with LLM assistance, and the failure modes above are not
hypothetical — they are the ones that had to be corrected by running the code. Three examples, each
verified against this repository at 0.9.3.

**The panic message named the wrong task.** Early drafts reported a failing test as `test panicked
in task "task-0"`. The real output, from a run of the `check_random` example in
`shuttle/src/lib.rs`:

```text
Task failed, serializing schedule
test panicked in task 'main-thread'
failing schedule:
"
910102cac2ffb5dab1ef837d04
"
pass that string to `shuttle::replay` to replay the failure
```

Single quotes, not double; the schedule on its own lines rather than in the same sentence; and
`main-thread` for the root task, because `ExecutionState::spawn_main_thread` names it that. `task-N`
is the fallback in `ExecutionState::failing_task` for a task with no name, so an unnamed spawned
thread does read `test panicked in task 'task-1'` — but the root task never does. The draft was not
inventing freely: `shuttle/src/lib.rs` still shows the older `task-0` form in its own module
documentation. A stale doc comment in the crate is exactly the kind of authoritative-looking source
that survives into generated text, and the only way to catch it was to run the example.

**The schedule strings did not replay.** Early drafts contained hex schedule strings of the right
length and shape that no run had ever produced. Each was replaced by a string captured from a real
execution, and the ones that could not be re-captured against the code as written were deleted
rather than fixed — the right outcome, since a schedule paired with prose that has drifted from the
code that produced it is worse than no schedule at all.

**The `annotation` feature was described as working.** A draft of [Annotations and Shuttle
Explorer](./explorer.md) documented `shuttle::annotate_replay` and the annotation workflow as
available behind the `annotation` feature. It is not: `cargo check -p shuttle --features annotation`
fails to compile `shuttle-engine`, with four errors in `shuttle-engine/src/annotations/mod.rs` —
`annotation_file()` called without being in scope, two `no field 'time' on type '&VectorClock'`, and
a `Result` where an `Option<VectorClock>` is expected. Enabling `vector-clocks` alongside it does
not help. Shuttle's own CI never enables the feature, so nothing catches it and the documentation
reads as though it works. A model reading `Cargo.toml`, the feature list, and the doc comments would
conclude the same. Only building it says otherwise.

The pattern in all three is the same: the claim was consistent with everything written down and
wrong about what the code does. That is the weakness to design your review around, and it is why
every example in this book is compiled and every quoted output was captured from a run.

## Where the code goes

Sending source you do not own to a third-party service is a disclosure, and Shuttle work involves
pasting exactly the files an organisation is most likely to treat as sensitive: the concurrent
internals of a production system, along with the bug reports that motivated the test. Use whatever
tooling your organisation has approved for source code, and settle that before pasting a crate into
a prompt rather than after.
