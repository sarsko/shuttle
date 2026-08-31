# Annotations and Shuttle Explorer

A failing Shuttle test hands you an encoded schedule string. It makes the failure
reproducible (see [Debugging failures: schedules and replay](./debugging.md)), but
it is opaque: it tells you *that* an interleaving fails, not *what your tasks
did* — which task held the lock, who was blocked on whom, whether the two writes
you suspect were really concurrent. Annotations are Shuttle's answer. Replay a
schedule with annotations enabled and Shuttle records every scheduling decision and
synchronization operation into a JSON file. **Shuttle Explorer**, a VS Code
extension in this repository, renders that file as an interactive timeline: one row
per task, one mark per step, with vector-clock causality and cursors placed in your
source.

Set expectations first: annotations are **opt-in** behind the `annotation` Cargo
feature, and with the feature off every recording call is an inlined no-op. Only
`annotate_replay` (or a hand-built `AnnotationScheduler`) writes a file — ordinary
`check_random`/`check_dfs` runs record nothing. And Shuttle Explorer is **not
published** to the Marketplace: you build it from source and run it in a debug
extension host. It is experimental, and parts of its UI are unfinished.

## Enabling annotations

The feature lives on the `shuttle` crate and forwards to `shuttle-engine` and
`shuttle-schedulers`. Because Shuttle is a dev-dependency, enabling it cannot
affect your production build:

```toml
[dev-dependencies]
shuttle = { version = "0.9", features = ["annotation", "vector-clocks"] }
```

Two notes on that line. `annotation` pulls in `serde`, `serde_json`, and `regex`
— that is the whole extra dependency cost — and it is what makes
`shuttle::annotate_replay` exist, since that function is
`#[cfg(feature = "annotation")]`. Enable `vector-clocks` alongside it: the
recorded clocks drive the causality colouring, the causal-graph view, and the
happens-before filters, and without them Shuttle substitutes a zero-sized stub
clock that the annotation code's clock serializer cannot compile against.

> **The `annotation` feature does not currently build.** As of Shuttle 0.9.3,
> `cargo check` with `annotation` enabled fails inside
> `shuttle-engine/src/annotations/mod.rs`, with or without `vector-clocks`:
> `annotation_file()` is called without being in scope, and an
> `ExecutionState::try_with(..)` result is passed where an `Option<VectorClock>`
> is expected (plus two `VectorClock::time` errors when `vector-clocks` is off).
> Shuttle's own test configuration never enables `annotation`, so the breakage
> is invisible in CI. Everything below describes the intended workflow and the
> data model the code produces; expect to fix those compile errors — or wait for
> a release that does — before you can follow it end to end.

### Where the file goes

The output path comes from the `SHUTTLE_ANNOTATION_FILE` environment variable
(exposed as the constant `shuttle::ANNOTATION_FILE`). Unset, Shuttle writes
`annotated.json` relative to the test process's working directory, which for
`cargo test` is your package root. A complete invocation:

```sh
RUST_BACKTRACE=1 \
SHUTTLE_ANNOTATION_FILE=annotated.json \
  cargo test --lib annotated_lost_update -- --exact --nocapture
```

`RUST_BACKTRACE=1` matters: Shuttle captures per-event backtraces with
`std::backtrace::Backtrace::capture()`, which is a no-op unless `RUST_BACKTRACE`
or `RUST_LIB_BACKTRACE` is set. Without it you get a timeline with no source
locations and no code cursors. Run one annotated test at a time — two in the
same binary would race to write the same path.

## `annotate_replay` and the workflow

The intended loop is two runs. First find the failure the usual way — see
[Schedulers and check functions](./schedulers.md) — and copy the encoded schedule
out of the panic message or the file `FailurePersistence` wrote. Then replay *that*
schedule with annotations on:

```rust,ignore
#[test]
fn annotated_lost_update() {
    shuttle::annotate_replay(
        || lost_update(), // exactly the closure your failing test used
        "910213ab84c0b9c2b2e39ab0f2c6cd0e...",
    );
}
```

`annotate_replay` builds a `ReplayScheduler::new_from_encoded` and wraps it in an
`AnnotationScheduler`, which forwards every `next_task`/`next_u64` call to the
inner scheduler while recording it. Recording starts when that scheduler is
constructed and the JSON is written when it is **dropped** — including while
unwinding from the test's panic, so a failing run still produces a file. (If
nothing was ever scheduled, no file is written; if your harness aborts instead of
unwinding, the `Drop` never runs.)

Annotating the replay rather than the search is deliberate: a search runs
thousands of executions into one flat log with no notion of iterations, so you
would get an enormous file describing an arbitrary mix. A replay is exactly one
execution — the failing one.

### Naming tasks and objects

By default rows are labelled `task 3` and `object 7`. `shuttle::current::set_name_for_task`
records a task's debug name, and `shuttle::annotations::WithName` records a name and
type for a synchronization object (implemented for Shuttle's `Mutex` and the
underlying `BatchSemaphore`):

```rust,ignore
use shuttle::annotations::WithName;
use shuttle::sync::Mutex;

let counter = Mutex::new(0usize).with_name("counter");
```

`with_name` is a no-op without the feature, so the call can stay in place; unnamed
objects show up as `Batch semaphore` in the details panel.

## What gets recorded

The JSON is a single object with interned string tables and a flat event log.
The exact shape lives in `shuttle-engine/src/annotations/mod.rs`; this excerpt is
representative:

```json
{
  "version": 0,
  "files": [{ "path": "./src/counter.rs" }],
  "functions": [{ "name": "my_crate::counter::increment" }],
  "objects": [
    { "created_by": 0, "created_at": 3, "name": "counter", "kind": "shuttle::sync::Mutex" }
  ],
  "tasks": [
    { "created_by": 0, "first_step": 0, "last_step": 41, "name": "main-thread" },
    { "created_by": 0, "first_step": 6, "last_step": 33, "name": null }
  ],
  "events": [
    [0, null,             { "TaskCreated": [1, false] },         [2, 0], null],
    [0, [[0, 0, 14, 21]], { "SemaphoreAcquireFast": [0, 1] },    [3, 0], null],
    [1, [[0, 0, 22, 9]],  { "SemaphoreAcquireBlocked": [0, 1] }, [1, 4], [0, 1]],
    [0, [[0, 0, 16, 5]],  { "SemaphoreRelease": [0, 1] },        [4, 0], null],
    [1, null,             "Tick",                                [2, 5], [1]]
  ]
}
```

* `version` is `0`; the extension refuses anything else.
* `files` and `functions` are string tables. Backtrace frames refer to them by
  index, each frame being `[file, function, line, column]`.
* `objects` and `tasks` carry creation provenance (`created_by`, `created_at`) and
  the step range the entity is alive for.
* Each event is a five-element array
  `[task, backtrace_or_null, kind, clock_or_null, runnable_or_null]`. The `kind`
  is a serialized Rust enum: unit variants are bare strings (`"Tick"`,
  `"Random"`, `"TaskTerminated"`), the rest single-key objects. The full set is
  `SemaphoreCreated`, `SemaphoreClosed`, `SemaphoreAcquireFast`,
  `SemaphoreAcquireBlocked`, `SemaphoreAcquireUnblocked`, `SemaphoreTryAcquire`,
  `SemaphoreRelease`, `TaskCreated`, `TaskTerminated`, `Random`, `Tick`.
* `clock` is the acting task's vector clock — what lets a reader distinguish
  before, after, and concurrent.
* `runnable` is non-null only on the first event after a scheduling decision, and
  lists the tasks available to run then. Tasks missing from it were blocked.

Every Shuttle primitive is built on `BatchSemaphore`, so `Mutex::lock` appears as
a semaphore acquire and a `Condvar` wait as a blocked acquire; see
[How Shuttle works](./internals.md) for why.

### What is not recorded

This is a log of *scheduling-relevant* operations, not a trace of your program. It
holds no values, so you cannot see what a task computed or what a lock protected;
no atomics, cells, or plain memory accesses, so a data race on a `Cell` produces
no events of its own — only the `Tick`s around it; and no panic or assertion
detail, so you correlate the end of the log with the test's own output.

One filtering rule deserves emphasis: backtrace frames are kept only for paths
beginning with `./src/`, so **only code inside your crate's `src/` directory
appears** and a test living in `tests/` contributes no frames at all. Put the test
you want to annotate in a `#[cfg(test)] mod tests` inside `src/`.

## Building Shuttle Explorer

Full instructions are in
[`shuttle-explorer/README.md`](https://github.com/awslabs/shuttle/tree/main/shuttle-explorer).
In outline: install VS Code 1.92 or later, Node 18-ish, and — easy to skip, and
non-negotiable — the **esbuild Problem Matcher** extension, without which the build
fails *silently*. Then `npm install` in `shuttle-explorer/`, open that directory in
VS Code, and press <kbd>F5</kbd>; two watch tasks start and an "extension host"
window opens.

In that window, open your Rust crate as the workspace folder — that is what makes
source navigation work, since recorded relative paths resolve against the *first*
workspace folder. Run **Shuttle Explorer: Focus on Home View** from the command
palette to reveal the panel, then load your JSON with the file button in its
top-left corner or the **View annotated schedule** command. Those are the
extension's only entry points: one command, and one webview view ("Home") inside a
panel container called **Shuttle Explorer**.

## Using the extension

**Rows and timeline.** The left column is a collapsible tree: tasks nest under the
task that spawned them, and each object is attached under the nearest common
ancestor of the tasks that touched it. It opens to depth 3; click the `+`/`–`
circles to fold, or use the toolbar toggle to hide object rows. On the right, the x
axis is step number (index into `events`), and each row gets a span covering its
first and last step plus a mark per event. Scroll to zoom; zoomed out, adjacent
marks collapse into summaries. Arrow keys step the selection between events.

**Selection.** Click a mark, a row, or a tree label. The SELECTION tab shows the
event's kind, step number, task, and its backtrace as a function/`path:line:col`
table — click a table row to open that file at that position. A selected task
shows its kind (Thread or Future), name, creator and creating step, and first/last
step; an object shows the same plus its recorded type. Selection also recolours
the timeline by causality: marks are tagged *before* or *after* when ordered by
program order or vector clock, and left neutral when concurrent. The neutral marks
are the interesting ones — they are what could have moved.

**Code cursors.** With one of your source files open, Shuttle Explorer decorates
it with `#N` markers: the selected event's position for its task, plus the last
known position of every other running task. Stepping the selection walks all the
cursors forward together — the closest thing here to a multi-threaded debugger.
Open `annotated.json` itself and the selected event's array is highlighted, which
is the practical way to read fields the panel does not surface, such as
`runnable`.

**Filters.** The FILTERS tab has a checkbox and count per category — tick,
semaphore, task, random. `Tick` events are *hidden by default*; enable them to see
every scheduling point rather than only synchronization operations. The "Create
filter" button builds a filter keeping only events causally before, causally after,
or concurrent with the selected event; from an object, only events with (or
without) that object. You can also load a `.js` file exporting a `check` function
as a custom filter, watched and re-applied on save. A final toolbar toggle switches
the x axis from step index to causal depth, placing unordered events at the same
horizontal position; the source notes this causal-graph view is still imprecise.

## Reading a timeline

**A race.** You want two accesses the timeline shows as concurrent. Select the
last event of the task that panicked and look for neutral marks — neither before
nor after — in the other task rows; those have no happens-before relation to your
failure, and a "concurrent" filter strips the rest away. Since plain memory
accesses are not recorded, the marks you see are the surrounding lock or channel
operations, and their code cursors tell you where each task sat while the other
ran.

**A deadlock.** Shuttle reports the deadlock itself; the timeline tells you who
waited on what. Look at the right-hand end of each task row: the involved tasks
all end on `SemaphoreAcquireBlocked`, and the `runnable` list of the last
scheduled event excludes them. Select each blocked acquire and follow its link to
an object row — two tasks blocked on each other's object is the classic lock-order
inversion, and the two backtraces name the two acquisition sites.

**A lost wakeup.** The signature is an unmatched pair: a task ends on
`SemaphoreAcquireBlocked` for an object with no later `SemaphoreAcquireUnblocked`
naming that task's id. Filter to that object and read its row left to right — if
the `SemaphoreRelease` (or notifying operation) sits *before* the blocked acquire,
the wakeup happened before anyone was waiting. Most `Condvar` bugs take this
shape; see [Async code and futures](./async.md) for the future-based version,
where the waker is what went missing.

## Limits and troubleshooting

* **File size.** Every scheduling point is a `Tick` and every event may carry a
  backtrace, so files grow fast: a few thousand steps is comfortable, hundreds of
  thousands is not. Shrink the reproducer; if you only need the shape of the
  interleaving, drop `RUST_BACKTRACE` and the file shrinks dramatically.
* **Experimental and unfinished.** The extension accepts only schedule version
  `0`, with no compatibility guarantee — the Rust structs and the TypeScript parser
  are kept in sync by hand. The "Scroll to selected task/event/object" (eye) button
  has no behaviour wired up, blocked-range shading and causal-dependence arrows
  exist in the source but are disabled, and hover previews of code cursors are
  commented out so only clicking updates the decorations.
* **The panel will not reopen.** Known bug: once closed, re-opening it generally
  fails. Reload the extension host window (**Developer: Reload Window**) and load
  the file again; if the window closes on reload, press <kbd>F5</kbd> from the
  development window to restart.
* **No timeline appeared.** Check in order: was a file written (package root, or
  wherever `SHUTTLE_ANNOTATION_FILE` pointed); did the test unwind rather than
  abort; is the webview reporting an error under **Developer: Open Webview
  Developer Tools**. If you get a timeline but no source navigation, either
  `RUST_BACKTRACE` was unset or the paths do not resolve — frames are captured only
  for `./src/...` paths, against the host's first workspace folder.
* **`annotate_replay` does not exist.** The `annotation` feature is not enabled on
  the `shuttle` dev-dependency. See [Configuring test runs](./configuration.md)
  for the other environment variables Shuttle reads.
