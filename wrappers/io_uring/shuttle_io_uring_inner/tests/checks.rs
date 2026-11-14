//! The checks of how a ring's user treats the buffers and files of the ops in flight.

#![cfg(target_os = "linux")]

mod common;

use std::collections::HashSet;
use std::os::unix::fs::FileExt;
use std::sync::{Arc, Mutex};

use common::{fd, file, iov, iov_mut, push, reap, reap_n, results};
use shuttle::check_random;
use shuttle_io_uring_inner::checks::{self, Checks};
use shuttle_io_uring_inner::{IoUring, opcode, squeue};

const ITERATIONS: usize = 50;

fn async_write(ring: &IoUring, file: &std::fs::File, buf: &[u8], offset: u64, user_data: u64) {
    push(
        ring,
        opcode::Write::new(fd(file), buf.as_ptr(), buf.len() as u32)
            .offset(offset)
            .build()
            .flags(squeue::Flags::ASYNC)
            .user_data(user_data),
    );
}

fn async_read(ring: &IoUring, file: &std::fs::File, buf: &mut [u8], offset: u64, user_data: u64) {
    push(
        ring,
        opcode::Read::new(fd(file), buf.as_mut_ptr(), buf.len() as u32)
            .offset(offset)
            .build()
            .flags(squeue::Flags::ASYNC)
            .user_data(user_data),
    );
}

#[test]
fn the_buffer_checks_are_on_and_the_file_range_checks_off_by_default() {
    check_random(
        || {
            assert_eq!(
                checks::checks(),
                Checks {
                    buffers: true,
                    file_ranges: false
                }
            );
        },
        1,
    );
}

/// A write's buffer changed while the write is in flight: the kernel may have read either version.
#[test]
#[should_panic(expected = "changed while the write was in flight: byte 10 of its 64")]
fn changing_a_write_buffer_in_flight_panics() {
    check_random(
        || {
            let file = file();
            let ring = IoUring::new(8).unwrap();
            let mut data = vec![1_u8; 64];
            async_write(&ring, &file, &data, 0, 1);
            ring.submit().unwrap();
            data[10] = 2;
            reap_n(&ring, 1);
        },
        ITERATIONS,
    );
}

/// The same for the buffers a vectored write lists.
#[test]
#[should_panic(expected = "changed while the writev was in flight: byte 3 of its 8")]
fn changing_a_vectored_write_buffer_in_flight_panics() {
    check_random(
        || {
            let file = file();
            let ring = IoUring::new(8).unwrap();
            let head = vec![1_u8; 16];
            let mut tail = vec![2_u8; 8];
            let iovecs = [iov(&head), iov(&tail)];
            push(
                &ring,
                opcode::Writev::new(fd(&file), iovecs.as_ptr(), 2)
                    .build()
                    .flags(squeue::Flags::ASYNC)
                    .user_data(1),
            );
            ring.submit().unwrap();
            tail[3] = 3;
            reap_n(&ring, 1);
        },
        ITERATIONS,
    );
}

/// Something wrote into a read's buffer while the read is in flight: the kernel may overwrite it.
#[test]
#[should_panic(
    expected = "something wrote into the buffer of the read with user_data 0x1 while the read was in flight"
)]
fn writing_into_a_read_buffer_in_flight_panics() {
    check_random(
        || {
            let file = file();
            let ring = IoUring::new(8).unwrap();
            let mut buf = vec![0_u8; 64];
            async_read(&ring, &file, &mut buf, 0, 1);
            ring.submit().unwrap();
            buf[0] = 7;
            reap_n(&ring, 1);
        },
        ITERATIONS,
    );
}

/// A read's buffer holds `0xA5`s while the read is in flight, so that code that consumes it too
/// early sees garbage; when it completes, it holds what the read returned, and past a short read's
/// count, what it held before.
#[test]
fn a_read_buffer_holds_poison_until_the_read_completes() {
    check_random(
        || {
            let file = file();
            file.write_all_at(&[9_u8; 100], 0).unwrap();
            let ring = IoUring::new(8).unwrap();
            let mut buf = vec![3_u8; 256];
            async_read(&ring, &file, &mut buf, 0, 1);
            ring.submit().unwrap();
            assert!(
                buf.iter().all(|&b| b == 0xa5),
                "the buffer of a read in flight is poisoned"
            );
            assert_eq!(reap_n(&ring, 1)[0].result(), 100);
            assert!(buf[..100].iter().all(|&b| b == 9), "the bytes read");
            assert!(
                buf[100..].iter().all(|&b| b == 3),
                "the bytes past the count are as they were"
            );
        },
        ITERATIONS,
    );
}

/// A read that completes inline never shows the poison: it has no time in flight.
#[test]
fn a_read_that_completes_inline_is_never_poisoned() {
    let outcomes = Arc::new(Mutex::new(HashSet::new()));
    let seen = Arc::clone(&outcomes);
    check_random(
        move || {
            let file = file();
            file.write_all_at(&[9_u8; 16], 0).unwrap();
            let ring = IoUring::new(8).unwrap();
            let mut buf = vec![0_u8; 16];
            push(
                &ring,
                opcode::Read::new(fd(&file), buf.as_mut_ptr(), 16).build().user_data(1),
            );
            ring.submit().unwrap();
            let snapshot = buf.clone();
            let inline = snapshot == [9_u8; 16];
            assert!(inline || snapshot == [0xa5_u8; 16], "the buffer reads {snapshot:?}");
            seen.lock().unwrap().insert(inline);
            reap_n(&ring, 1);
            assert_eq!(buf, [9_u8; 16]);
        },
        ITERATIONS * 4,
    );
    assert_eq!(*outcomes.lock().unwrap(), HashSet::from([true, false]));
}

/// Two reads into one buffer: the kernel would write the memory from both.
#[test]
#[should_panic(expected = "overlaps the buffer of the read with user_data 0x1")]
fn two_reads_into_one_buffer_panic() {
    check_random(
        || {
            let file = file();
            let ring = IoUring::new(8).unwrap();
            let mut buf = vec![0_u8; 64];
            async_read(&ring, &file, &mut buf, 0, 1);
            async_read(&ring, &file, &mut buf[32..], 64, 2);
            ring.submit().unwrap();
            reap_n(&ring, 2);
        },
        ITERATIONS,
    );
}

/// A read into the buffer a write in flight reads from.
#[test]
#[should_panic(expected = "overlaps the buffer of the write with user_data 0x1")]
fn a_read_into_a_writes_buffer_panics() {
    check_random(
        || {
            let file = file();
            let ring = IoUring::new(8).unwrap();
            let mut buf = vec![0_u8; 64];
            async_write(&ring, &file, &buf, 0, 1);
            let reads = [iov_mut(&mut buf[..8])];
            push(
                &ring,
                opcode::Readv::new(fd(&file), reads.as_ptr(), 1)
                    .offset(128)
                    .build()
                    .flags(squeue::Flags::ASYNC)
                    .user_data(2),
            );
            ring.submit().unwrap();
            reap_n(&ring, 2);
        },
        ITERATIONS,
    );
}

/// Writes only read their buffers, so several may share one.
#[test]
fn two_writes_from_one_buffer_are_fine() {
    check_random(
        || {
            let file = file();
            let ring = IoUring::new(8).unwrap();
            let data = vec![5_u8; 64];
            async_write(&ring, &file, &data, 0, 1);
            async_write(&ring, &file, &data[16..], 64, 2);
            ring.submit().unwrap();
            assert_eq!(results(&reap_n(&ring, 2)), vec![(1, 64), (2, 48)]);
        },
        ITERATIONS,
    );
}

/// Ops in sequence may reuse a buffer: the checks apply only while an op is in flight.
#[test]
fn reusing_a_buffer_after_the_completion_is_fine() {
    check_random(
        || {
            let file = file();
            let ring = IoUring::new(8).unwrap();
            let mut buf = vec![1_u8; 64];
            async_write(&ring, &file, &buf, 0, 1);
            reap_n(&ring, 1);
            buf.fill(2);
            async_write(&ring, &file, &buf, 64, 2);
            reap_n(&ring, 1);
            async_read(&ring, &file, &mut buf, 0, 3);
            reap_n(&ring, 1);
            assert_eq!(buf, [1_u8; 64]);
        },
        ITERATIONS,
    );
}

/// With the buffer checks off, nothing is checked, copied or poisoned.
#[test]
fn the_buffer_checks_turn_off() {
    check_random(
        || {
            checks::set_checks(Checks {
                buffers: false,
                ..Checks::default()
            });
            let file = file();
            let ring = IoUring::new(8).unwrap();
            let mut data = vec![1_u8; 64];
            async_write(&ring, &file, &data, 0, 1);
            ring.submit().unwrap();
            data[10] = 2;
            let mut buf = vec![0_u8; 64];
            async_read(&ring, &file, &mut buf, 0, 2);
            async_read(&ring, &file, &mut buf, 0, 3);
            ring.submit().unwrap();
            assert!(buf.iter().all(|&b| b == 0), "nothing poisons the buffer");
            reap_n(&ring, 3);
        },
        ITERATIONS,
    );
}

/// A cancelled read leaves its buffer as it was, as the kernel never started it.
#[test]
fn a_cancelled_read_leaves_its_buffer_as_it_was() {
    let outcomes = Arc::new(Mutex::new(HashSet::new()));
    let seen = Arc::clone(&outcomes);
    check_random(
        move || {
            let file = file();
            file.write_all_at(&[9_u8; 32], 0).unwrap();
            let ring = IoUring::new(8).unwrap();
            let mut buf = vec![4_u8; 32];
            async_read(&ring, &file, &mut buf, 0, 1);
            ring.submit().unwrap();
            push(&ring, opcode::AsyncCancel::new(1).build().user_data(2));
            ring.submit().unwrap();
            let results = results(&reap_n(&ring, 2));
            let read = results[0].1;
            match read {
                r if r == -libc::ECANCELED => assert_eq!(buf, [4_u8; 32]),
                32 => assert_eq!(buf, [9_u8; 32]),
                other => panic!("read result {other}"),
            }
            seen.lock().unwrap().insert(read);
        },
        ITERATIONS * 4,
    );
    assert_eq!(*outcomes.lock().unwrap(), HashSet::from([-libc::ECANCELED, 32]));
}

/// With the file range checks on, a write in flight and an op on an overlapping range of the same
/// file panic: the outcome depends on which completes first.
#[test]
#[should_panic(expected = "covers bytes 1024..2048 of the file")]
fn overlapping_ops_on_a_file_panic_with_the_file_range_checks() {
    check_random(
        || {
            checks::set_checks(Checks {
                file_ranges: true,
                ..Checks::default()
            });
            let file = file();
            let ring = IoUring::new(8).unwrap();
            let data = vec![1_u8; 4096];
            let mut buf = vec![0_u8; 1024];
            async_write(&ring, &file, &data, 0, 1);
            async_read(&ring, &file, &mut buf, 1024, 2);
            ring.submit().unwrap();
            reap_n(&ring, 2);
        },
        ITERATIONS,
    );
}

/// Disjoint ranges, and reads of the same range, pass the file range checks, and so do ops that
/// overlap once the first has completed. They do not apply to other files, or by default.
#[test]
fn the_file_range_checks_allow_what_is_ordered() {
    check_random(
        || {
            let file = file();
            let other = common::file();
            let ring = IoUring::new(8).unwrap();
            let data = vec![1_u8; 4096];
            let mut buf = vec![0_u8; 1024];
            let mut buf2 = vec![0_u8; 1024];
            // Off by default.
            async_write(&ring, &file, &data, 0, 1);
            async_read(&ring, &file, &mut buf, 1024, 2);
            ring.submit().unwrap();
            reap_n(&ring, 2);

            checks::set_checks(Checks {
                file_ranges: true,
                ..Checks::default()
            });
            async_write(&ring, &file, &data, 0, 3);
            async_write(&ring, &file, &data, 4096, 4);
            async_write(&ring, &other, &data, 0, 5);
            ring.submit().unwrap();
            reap_n(&ring, 3);
            async_read(&ring, &file, &mut buf, 0, 6);
            async_read(&ring, &file, &mut buf2, 512, 7);
            ring.submit().unwrap();
            reap_n(&ring, 2);
            assert_eq!(buf, [1_u8; 1024]);
        },
        ITERATIONS,
    );
}

/// A read or write at offset `-1` uses and advances the file position (`IORING_FEAT_RW_CUR_POS`).
#[test]
fn offset_minus_one_uses_the_file_position() {
    check_random(
        || {
            let file = file();
            let ring = IoUring::new(8).unwrap();
            assert!(ring.params().is_feature_rw_cur_pos());
            let first = [1_u8; 8];
            let second = [2_u8; 8];
            for (ud, data) in [(1, &first), (2, &second)] {
                push(
                    &ring,
                    opcode::Write::new(fd(&file), data.as_ptr(), 8)
                        .offset(u64::MAX)
                        .build()
                        .user_data(ud),
                );
                assert_eq!(reap_n(&ring, 1)[0].result(), 8);
            }
            let mut read = [0_u8; 16];
            file.read_exact_at(&mut read, 0).unwrap();
            assert_eq!(read, [[1_u8; 8], [2_u8; 8]].concat()[..]);
            assert!(reap(&ring).is_empty());
        },
        ITERATIONS,
    );
}
