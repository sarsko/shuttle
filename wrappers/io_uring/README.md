# Shuttle support for `io-uring`

This crate contains the wrapper that enables testing of applications that drive an `io_uring` with
the [`io-uring`](https://crates.io/crates/io-uring) crate with Shuttle.

## How to use

To use it, add the following in your Cargo.toml:

```
[features]
shuttle = [
   "io-uring/shuttle",
]

[target.'cfg(target_os = "linux")'.dependencies]
io-uring = { package = "shuttle-io-uring", version = "0.7" }
```

The code will behave as before when the `shuttle` feature flag is not provided, and will run on a
simulated kernel that Shuttle controls when the `shuttle` feature flag is provided.

## What the `shuttle` feature changes

Without the feature, this crate is io-uring: the kernel's ring. With it, it is
`shuttle-io-uring-inner`, a simulated kernel with io-uring 0.7.16's API for the operations it
models (`Nop`, `Read`, `Readv`, `Write`, `Writev`, `Fsync`, `Fallocate`, `Timeout` and
`AsyncCancel`):

- Each ring has a kernel task that completes the ops in flight, one per step, in an order Shuttle
  chooses, by running each op's system call on the real file descriptor when it completes. Ops may
  also complete inline, during the enter that submits them.
- Timeout SQEs and the bounded waits of `submit_with_args` are timers, which fire while I/O is in
  flight only one time in 256, as a timeout normally outlasts the I/O it guards.
- The CQ tail and flags are Shuttle atomics, and a completion posts in the kernel's order: tail
  store, then flags load and eventfd signal, which adds 1 to the eventfd's counter.
- By default the simulation checks how the program treats the buffers of the ops in flight, and
  panics on a write buffer changed, a read buffer written, or memory handed to the kernel for two
  ops at once where either is a read. A read's buffer holds `0xA5` bytes while it is in flight.
- Every choice is a draw from Shuttle's random number generator, so a failing schedule replays and
  `check_uncontrolled_nondeterminism` passes. All state belongs to the running execution.

The feature also adds a few items io-uring does not have, all for tests: `AsyncFd`, which waits on
the readiness of a ring's fd or of an eventfd registered with a ring (tokio's needs an I/O driver,
which a Shuttle execution does not have), and the `faults` and `checks` modules. The `AsyncFd` is
only the readable part a ring driver needs; a model of tokio's whole `AsyncFd` belongs in
`shuttle-tokio`, and is a separate piece of work.

## Limitations

- Only on Linux: on other targets the crate is empty.
- Opcodes beyond those above, linked or drained SQEs, `SQPOLL`, `IOPOLL`, registered buffers and
  deferred task work are not modelled; an opcode without a builder fails to build under the feature.
- Ops run real system calls, so there is no crash model.
- Building a ring, like the rest of Shuttle, panics outside of a Shuttle test, and `check_dfs` cannot
  run one, as every choice the simulation makes is a random draw.

See [the crate documentation](https://docs.rs/shuttle-io-uring-inner) for the model in full.
