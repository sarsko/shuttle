# Shuttle support for `io-uring`

This crate contains the implementation that enables testing of applications that drive an
`io_uring` with Shuttle. It should not be depended on directly, depend on `shuttle-io-uring` instead.

It is a simulated kernel with the API of the [`io-uring`](https://crates.io/crates/io-uring) crate,
0.7.16, for the operations it models. Each ring has a Shuttle task that completes its ops in an
order Shuttle chooses, by running each op's system call on the real file descriptor, so a ring
driver's races are explored and a failing schedule replays. See the crate documentation for the
model, the checks it makes of the program's buffers, and what it does not model.

## License

Parts of this crate are derived from
[`turmoil-io-uring`](https://github.com/tokio-rs/turmoil/tree/684acc1a8eea3a9cf2c6959dc47b69dba981cac1/crates/turmoil-io-uring)
0.1.0, Copyright (c) 2022 Carl Lerche, which is licensed under the MIT license: see `LICENSE-MIT`.
Every file says in its header whether it is derived from that crate or new in the fork. The parts
that are not derived, and the changes to the ones that are, are licensed under the Apache License,
Version 2.0, as the rest of Shuttle is.
