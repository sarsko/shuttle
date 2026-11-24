use crate::basic::clocks::me;
use crate::{check_replay_roundtrip, check_replay_roundtrip_file, Config, FailurePersistence};
use shuttle_engine::scheduler::serialization::deserialize_schedule;

use shuttle::scheduler::{PctScheduler, RandomScheduler, ReplayScheduler, RoundRobinScheduler, Schedule};
use shuttle::sync::Mutex;
use shuttle::{replay, thread, Runner};
use std::panic;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use test_log::test;

fn concurrent_increment_buggy() {
    let lock = Arc::new(Mutex::new(0usize));

    let threads = (0..2)
        .map(|_| {
            let lock = Arc::clone(&lock);
            thread::spawn(move || {
                let curr = *lock.lock().unwrap();
                *lock.lock().unwrap() = curr + 1;
            })
        })
        .collect::<Vec<_>>();

    for thd in threads {
        thd.join().unwrap();
    }

    // there's a race where both threads read 0 and then set the counter to 1, so this can fail
    assert_eq!(*lock.lock().unwrap(), 2, "counter is wrong");
}

/// A schedule in which both threads read 0 before either writes, so the counter ends at 1.
///
/// Pinned in the legacy fixed-width hex encoding, which Shuttle is required to keep reading, so this
/// doubles as a check that old schedules still replay. Note that it is a schedule Shuttle persisted at
/// the moment of failure, so it stops there and says nothing about the steps taken while the panic
/// unwinds; replaying it must still reproduce the failure without `set_allow_incomplete`.
#[test]
#[should_panic(expected = "counter is wrong")]
fn replay_failing() {
    replay(concurrent_increment_buggy, "910211ed84dcbbe1bd8c946080408922290100")
}

/// A complete schedule in which the two increments do not overlap, so the counter reaches 2 and the
/// test passes.
#[test]
fn replay_passing() {
    replay(concurrent_increment_buggy, "9102120280404922480200")
}

#[test]
fn replay_roundtrip() {
    check_replay_roundtrip(concurrent_increment_buggy, PctScheduler::new(2, 100))
}

#[test]
fn replay_roundtrip_file() {
    check_replay_roundtrip_file(concurrent_increment_buggy, PctScheduler::new(2, 100))
}

/// Run `f` until it fails, persisting the failing schedule to a fresh directory, and return the
/// contents of every schedule file that was written.
fn persist_failing_schedules<F>(f: F, iterations: usize) -> Vec<String>
where
    F: Fn() + Send + Sync + std::panic::RefUnwindSafe + 'static,
{
    persist_failing_schedules_with(f, iterations, Config::new())
}

/// As [`persist_failing_schedules`], with control over the config.
fn persist_failing_schedules_with<F>(f: F, iterations: usize, config: Config) -> Vec<String>
where
    F: Fn() + Send + Sync + std::panic::RefUnwindSafe + 'static,
{
    let dir = tempfile::tempdir().expect("could not create tempdir");

    let mut config = config;
    config.failure_persistence = FailurePersistence::File(Some(dir.path().to_path_buf()));

    let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        let runner = Runner::new(RandomScheduler::new(iterations), config);
        runner.run(f);
    }));
    assert!(result.is_err(), "test was supposed to fail");

    let mut schedules = std::fs::read_dir(dir.path())
        .expect("could not read tempdir")
        .map(|entry| {
            let path = entry.expect("bad dir entry").path();
            std::fs::read_to_string(&path).expect("could not read schedule file")
        })
        .collect::<Vec<_>>();
    schedules.sort();
    schedules
}

/// A schedule persisted from a real failure must replay that failure without any special handling.
///
/// This is the end-to-end property that matters: the schedule Shuttle hands you is enough to
/// reproduce the failure. In particular the replay must not need `set_allow_incomplete`, even though
/// the recorded schedule ends at the failure while the replayed execution goes on to make more
/// scheduling decisions as the panic unwinds.
#[test]
fn persisted_schedule_replays_without_allow_incomplete() {
    let schedules = persist_failing_schedules(concurrent_increment_buggy, 100);
    assert_eq!(
        schedules.len(),
        1,
        "expected exactly one schedule file per failure, got {}",
        schedules.len()
    );

    let result = panic::catch_unwind(|| {
        let scheduler = ReplayScheduler::new_from_encoded(&schedules[0]);
        let mut config = Config::new();
        // The replayed failure would otherwise persist a schedule of its own.
        config.failure_persistence = FailurePersistence::None;
        Runner::new(scheduler, config).run(concurrent_increment_buggy);
    });

    let payload = result.expect_err("replay should reproduce the failure");
    let message = payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .unwrap_or("<non-string panic>");
    assert!(
        message.contains("counter is wrong"),
        "replay panicked with {message:?} instead of reproducing the original failure"
    );
}

/// With eager reporting on, a panic the test catches itself must not consume the execution's report.
///
/// The panic hook runs for every panic, caught or not, so a swallowed panic reports a schedule too.
/// If reporting were once-only, that would claim the report and the failure that actually fails the
/// test would go unreported. Reporting is grow-only instead, so the later, longer schedule supersedes
/// the earlier one.
#[test]
fn a_caught_panic_does_not_suppress_the_real_failure() {
    // Only the panic hook sees the swallowed panic, so this is the configuration where the clash can
    // happen at all.
    let mut config = Config::new();
    config.eager_failure_reports = true;

    let schedules = persist_failing_schedules_with(
        || {
            let lock = Arc::new(Mutex::new(0usize));

            // Take a scheduling step, then panic and swallow it. This runs the panic hook.
            let caught = panic::catch_unwind(panic::AssertUnwindSafe(|| {
                *lock.lock().unwrap() += 1;
                panic!("swallowed on purpose");
            }));
            assert!(caught.is_err(), "the panic should have been caught here");

            // Now take more steps and fail for real, on a strictly longer schedule.
            for _ in 0..4 {
                *lock.lock().unwrap() += 1;
                thread::yield_now();
            }
            panic!("the real failure");
        },
        1,
        config,
    );

    assert_eq!(schedules.len(), 1, "expected exactly one schedule for one failure");
    let schedule = deserialize_schedule(&schedules[0]).expect("persisted schedule should decode");
    assert!(
        schedule.len() > 1,
        "persisted the schedule from the swallowed panic ({} steps) instead of the real failure",
        schedule.len()
    );
}

/// The flag decides whether the panic hook reports at all, which is observable through a test that
/// panics, catches it, and then passes.
///
/// Only the hook sees a panic like that: the runtime never learns of it, so it has nothing to report
/// after the fact. With eager reporting off, which is the default, a passing test therefore leaves no
/// schedule behind. With it on, the swallowed panic is reported even though the test passed.
#[test]
fn eager_reporting_decides_whether_a_swallowed_panic_is_reported() {
    assert!(
        !Config::new().eager_failure_reports,
        "the default should be to report once, after the unwind"
    );

    let run = |eager: bool| {
        let dir = tempfile::tempdir().expect("could not create tempdir");
        let mut config = Config::new();
        config.failure_persistence = FailurePersistence::File(Some(dir.path().to_path_buf()));
        config.eager_failure_reports = eager;

        // Panics and swallows it, then passes. Nothing here fails the test.
        Runner::new(RandomScheduler::new(1), config).run(|| {
            let lock = Arc::new(Mutex::new(0usize));
            let caught = panic::catch_unwind(panic::AssertUnwindSafe(|| {
                *lock.lock().unwrap() += 1;
                panic!("swallowed on purpose");
            }));
            assert!(caught.is_err(), "the panic should have been caught here");
        });

        std::fs::read_dir(dir.path()).expect("could not read tempdir").count()
    };

    assert_eq!(run(false), 0, "a swallowed panic should report nothing by default");
    assert_eq!(run(true), 1, "with eager reporting on, the hook should have reported");
}

/// A failure in a later execution must still be reported, even if an earlier execution of the same
/// runner already reported one.
#[test]
fn every_failing_execution_reports_its_own_schedule() {
    // `concurrent_increment_buggy` fails on some but not all schedules, so the runner reaches the
    // failing execution only after some successful ones. If the report were suppressed by state left
    // over from a previous execution, this would come back empty.
    for _ in 0..5 {
        let schedules = persist_failing_schedules(concurrent_increment_buggy, 100);
        assert_eq!(schedules.len(), 1, "expected exactly one schedule per failure");
        assert!(!schedules[0].trim().is_empty(), "persisted an empty schedule");
    }
}

/// A panic *after* the test has finished running is not a Shuttle failure and must not be reported as
/// one.
///
/// The panic hook is installed once per process and stays installed forever, so it has to know when it
/// is inside an execution and when it is not. Otherwise an unrelated panic later in the test thread,
/// including the test harness reporting a failure, gets a "failing schedule" report attached to it,
/// naming a schedule that has nothing to do with what actually went wrong.
#[test]
fn panic_after_test_reports_no_schedule() {
    let dir = tempfile::tempdir().expect("could not create tempdir");

    let mut config = Config::new();
    config.failure_persistence = FailurePersistence::File(Some(dir.path().to_path_buf()));
    Runner::new(RandomScheduler::new(10), config).run(|| {
        let lock = Arc::new(Mutex::new(0usize));
        let thd = {
            let lock = Arc::clone(&lock);
            thread::spawn(move || *lock.lock().unwrap() += 1)
        };
        thd.join().unwrap();
        assert_eq!(*lock.lock().unwrap(), 1);
    });

    let result = panic::catch_unwind(|| panic!("nothing to do with Shuttle"));
    assert!(result.is_err(), "the panic should have been caught");

    let persisted = std::fs::read_dir(dir.path())
        .expect("could not read tempdir")
        .map(|entry| entry.expect("bad dir entry").path())
        .collect::<Vec<_>>();
    assert!(
        persisted.is_empty(),
        "reported a schedule for a panic outside any execution: {persisted:?}"
    );
}

/// A schedule can end while a task unwinds, after the replay has switched to another task. Here the
/// main thread spins on `yield_now` while the spawned task unwinds. The replay has to share the steps
/// after the end of the schedule, so that the unwinding task finishes and its panic comes back,
/// instead of the spinning thread running until `max_steps`.
#[test]
fn replay_past_the_end_lets_the_unwinding_task_finish() {
    // Ends while the spawned task unwinds, once the main thread has yielded twice.
    let schedule = Schedule::new_from_task_ids(0, vec![0, 0, 1, 1, 0, 0]);
    let mut config = Config::new();
    config.failure_persistence = FailurePersistence::None;
    let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        Runner::new(ReplayScheduler::new_from_schedule(schedule), config).run(|| {
            let mutex = Arc::new(Mutex::new(0));
            drop(thread::spawn(move || {
                let _guard = mutex.lock().unwrap();
                panic!("task failed");
            }));
            loop {
                thread::yield_now();
            }
        })
    }));
    let payload = result.expect_err("replay should reproduce the failure");
    assert_eq!(payload.downcast_ref::<&str>(), Some(&"task failed"));
}

/// Set by the test closure, for the scheduler of `scheduler_panic_reports_its_schedule`.
static THREAD_FINISHING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// A random scheduler that panics on the first decision after the spawned thread has finished. Shuttle
/// makes that decision on the executor's own stack, outside any task.
struct FailsAfterAThreadFinishes(RandomScheduler);

impl shuttle::scheduler::Scheduler for FailsAfterAThreadFinishes {
    fn new_execution(&mut self) -> Option<Schedule> {
        self.0.new_execution()
    }

    fn next_task(
        &mut self,
        runnable: &[&shuttle::scheduler::Task],
        current: Option<shuttle::scheduler::TaskId>,
        is_yielding: bool,
    ) -> Option<shuttle::scheduler::TaskId> {
        let current_finished = !runnable.iter().any(|task| Some(task.id()) == current);
        if current_finished && THREAD_FINISHING.swap(false, std::sync::atomic::Ordering::SeqCst) {
            panic!("scheduler failed");
        }
        self.0.next_task(runnable, current, is_yielding)
    }

    fn next_u64(&mut self) -> u64 {
        self.0.next_u64()
    }
}

/// A panic out of the scheduler fails the test as a task's panic does, and has to report a schedule
/// too, though only the panic hook sees it.
#[test]
fn scheduler_panic_reports_its_schedule() {
    let dir = tempfile::tempdir().expect("could not create tempdir");
    let mut config = Config::new();
    config.failure_persistence = FailurePersistence::File(Some(dir.path().to_path_buf()));
    let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        Runner::new(FailsAfterAThreadFinishes(RandomScheduler::new_from_seed(1, 1)), config).run(|| {
            let lock = Arc::new(Mutex::new(0));
            let thd = {
                let lock = Arc::clone(&lock);
                thread::spawn(move || {
                    *lock.lock().unwrap() += 1;
                    THREAD_FINISHING.store(true, std::sync::atomic::Ordering::SeqCst);
                })
            };
            *lock.lock().unwrap() += 1;
            thd.join().unwrap();
        })
    }));
    let payload = result.expect_err("the scheduler should fail the test");
    assert_eq!(payload.downcast_ref::<&str>(), Some(&"scheduler failed"));
    let persisted = std::fs::read_dir(dir.path()).expect("could not read tempdir").count();
    assert_eq!(persisted, 1, "expected one schedule for the scheduler's panic");
}

fn deadlock() {
    let lock1 = Arc::new(Mutex::new(0usize));
    let lock2 = Arc::new(Mutex::new(0usize));
    let lock1_clone = Arc::clone(&lock1);
    let lock2_clone = Arc::clone(&lock2);

    thread::spawn(move || {
        let _l1 = lock1_clone.lock().unwrap();
        let _l2 = lock2_clone.lock().unwrap();
    });

    let _l2 = lock2.lock().unwrap();
    let _l1 = lock1.lock().unwrap();
}

#[test]
fn replay_deadlock_roundtrip() {
    check_replay_roundtrip(deadlock, PctScheduler::new(2, 100))
}

#[test]
fn replay_deadlock_roundtrip_file() {
    check_replay_roundtrip_file(deadlock, PctScheduler::new(2, 100))
}

fn deadlock_3() {
    let lock1 = Arc::new(Mutex::new(0usize));
    let lock2 = Arc::new(Mutex::new(0usize));
    let lock3 = Arc::new(Mutex::new(0usize));

    let lock1_clone = Arc::clone(&lock1);
    let lock2_clone = Arc::clone(&lock2);
    let lock3_clone = Arc::clone(&lock3);

    thread::spawn(move || {
        let _l1 = lock1_clone.lock().unwrap();
        let _l2 = lock2_clone.lock().unwrap();
    });

    thread::spawn(move || {
        let _l2 = lock2.lock().unwrap();
        let _l3 = lock3_clone.lock().unwrap();
    });

    let _l3 = lock3.lock().unwrap();
    let _l1 = lock1.lock().unwrap();
}

#[test]
#[should_panic(expected = "deadlock")]
fn replay_deadlock3_block() {
    // Reproduce deadlock
    let schedule = Schedule::new_from_task_ids(0, vec![0, 0, 1, 0, 1, 0, 2, 2, 1]);
    let scheduler = ReplayScheduler::new_from_schedule(schedule);
    let runner = Runner::new(scheduler, Default::default());
    runner.run(deadlock_3);
}

#[test]
fn replay_deadlock3_end_early() {
    // Schedule ends without all tasks finishing
    let schedule = Schedule::new_from_task_ids(0, vec![0, 0, 1, 2]);
    let mut scheduler = ReplayScheduler::new_from_schedule(schedule);
    scheduler.set_allow_incomplete();
    let runner = Runner::new(scheduler, Default::default());
    runner.run(deadlock_3);
}

#[test]
fn replay_deadlock3_task_disabled() {
    // Schedule ends when a task is not runnable
    let schedule = Schedule::new_from_task_ids(0, vec![0, 1, 2, 0]);
    let mut scheduler = ReplayScheduler::new_from_schedule(schedule);
    scheduler.set_allow_incomplete();
    let runner = Runner::new(scheduler, Default::default());
    runner.run(deadlock_3);
}

#[ignore = "this test aborts due to an issue with panic handling on exit with the generators library -- can be removed when we switch to corosensei"]
#[test]
fn replay_deadlock3_drop_mutex() {
    // Schedule ends with a task holding a Mutex, whose MutexGuard needs to be correctly cleaned up
    let schedule = Schedule::new_from_task_ids(0, vec![0, 0, 1, 0, 1, 0]);
    let mut scheduler = ReplayScheduler::new_from_schedule(schedule);
    scheduler.set_allow_incomplete();
    let runner = Runner::new(scheduler, Default::default());
    runner.run(deadlock_3);
}

/// A program that always terminates, and counts its completions in a non-Shuttle counter so that a
/// replay which stops early can be told apart from one that ran the program to the end.
fn concurrent_increment(completions: Arc<AtomicUsize>) -> impl Fn() + Send + Sync + 'static {
    move || {
        let lock = Arc::new(Mutex::new(0usize));

        let threads = (0..3)
            .map(|_| {
                let lock = Arc::clone(&lock);
                thread::spawn(move || {
                    *lock.lock().unwrap() += 1;
                })
            })
            .collect::<Vec<_>>();

        for thd in threads {
            thd.join().unwrap();
        }

        assert_eq!(*lock.lock().unwrap(), 3);
        completions.fetch_add(1, Ordering::SeqCst);
    }
}

/// A schedule that covers only the beginning of the execution ends the replay...
#[test]
#[should_panic(expected = "schedule ended early")]
fn replay_schedule_ends_early() {
    let schedule = Schedule::new_from_task_ids(0, vec![0, 0]);
    let scheduler = ReplayScheduler::new_from_schedule(schedule);
    let runner = Runner::new(scheduler, Default::default());
    runner.run(concurrent_increment(Arc::new(AtomicUsize::new(0))));
}

/// ... unless the scheduler is asked to continue after the schedule, in which case the program runs
/// to completion under the continuation scheduler.
#[test]
fn replay_continue_after_schedule() {
    let completions = Arc::new(AtomicUsize::new(0));

    let schedule = Schedule::new_from_task_ids(0, vec![0, 0]);
    let mut scheduler = ReplayScheduler::new_from_schedule(schedule);
    scheduler.set_continue_after_schedule();
    let runner = Runner::new(scheduler, Default::default());
    runner.run(concurrent_increment(Arc::clone(&completions)));

    assert_eq!(completions.load(Ordering::SeqCst), 1);
}

/// A schedule that diverges from the program (here, by naming a task that will never exist) ends the
/// replay...
#[test]
#[should_panic(expected = "scheduled task is not runnable")]
fn replay_schedule_diverges() {
    let schedule = Schedule::new_from_task_ids(0, vec![0, 7]);
    let scheduler = ReplayScheduler::new_from_schedule(schedule);
    let runner = Runner::new(scheduler, Default::default());
    runner.run(concurrent_increment(Arc::new(AtomicUsize::new(0))));
}

/// ... and is also continued from, rather than abandoning the execution.
#[test]
fn replay_continue_after_diverging_from_schedule() {
    let completions = Arc::new(AtomicUsize::new(0));

    let schedule = Schedule::new_from_task_ids(0, vec![0, 7]);
    let mut scheduler = ReplayScheduler::new_from_schedule(schedule);
    scheduler.set_continue_after_schedule();
    let runner = Runner::new(scheduler, Default::default());
    runner.run(concurrent_increment(Arc::clone(&completions)));

    assert_eq!(completions.load(Ordering::SeqCst), 1);
}

/// The continuation scheduler can be any scheduler, not just the default `RandomScheduler`.
#[test]
fn replay_continue_after_schedule_with_scheduler() {
    let completions = Arc::new(AtomicUsize::new(0));

    let schedule = Schedule::new_from_task_ids(0, vec![0, 0]);
    let mut scheduler = ReplayScheduler::new_from_schedule(schedule);
    scheduler.set_continue_after_schedule_with(RoundRobinScheduler::new(1));
    let runner = Runner::new(scheduler, Default::default());
    runner.run(concurrent_increment(Arc::clone(&completions)));

    assert_eq!(completions.load(Ordering::SeqCst), 1);
}

/// A program that always fails, with a message that depends on both the interleaving and the random
/// values the tasks were given.
fn random_sum_then_fail() {
    use shuttle::rand::Rng;

    let sum = Arc::new(Mutex::new(0u64));

    let threads = (0..3)
        .map(|_| {
            let sum = Arc::clone(&sum);
            thread::spawn(move || {
                for _ in 0..3 {
                    let value = shuttle::rand::thread_rng().gen::<u64>();
                    let mut sum = sum.lock().unwrap();
                    *sum = sum.wrapping_mul(31).wrapping_add(value);
                }
            })
        })
        .collect::<Vec<_>>();

    for thd in threads {
        thd.join().unwrap();
    }

    // If this would be a `panic!`, downcasting the `catch_unwind` error to `String` fails.
    assert_eq!(*sum.lock().unwrap(), 0, "so much work, and all for nothing");
}

/// Run `random_sum_then_fail` to failure and return the panic message, persisting the schedule to
/// `dir` if `dir` is given and replaying `schedule` if `schedule` is given.
fn run_to_failure(schedule: Option<std::path::PathBuf>, dir: Option<std::path::PathBuf>) -> String {
    let result = panic::catch_unwind(move || {
        let mut config = Config::new();
        config.failure_persistence = match dir {
            Some(dir) => FailurePersistence::File(Some(dir)),
            None => FailurePersistence::None,
        };
        match schedule {
            Some(path) => {
                let scheduler = ReplayScheduler::new_from_file(path).expect("could not read schedule file");
                Runner::new(scheduler, config).run(random_sum_then_fail)
            }
            None => {
                // A schedule covering only the start of the execution, continued from.
                let mut scheduler = ReplayScheduler::new_from_schedule(Schedule::new_from_task_ids(0, vec![0, 0]));
                scheduler.set_continue_after_schedule();
                Runner::new(scheduler, config).run(random_sum_then_fail)
            }
        }
    })
    .expect_err("test should panic");
    *result.downcast::<String>().expect("panic payload should be a string")
}

/// A failure found after the end of the recorded schedule is still reported as a schedule that
/// replays the whole execution on its own: the runner records the steps actually taken, and the
/// random values the program was given came from the replayed schedule's seed.
#[test]
fn replay_continue_after_schedule_records_a_replayable_schedule() {
    let dir = tempfile::tempdir().unwrap();

    let expected = run_to_failure(None, Some(dir.path().to_path_buf()));
    assert!(expected.contains("so much work"), "unexpected failure: {expected}");

    // The panic hook that persists schedules is installed process-globally by the first `Runner` to
    // run, so in a test harness that shares one process between tests (plain `cargo test`) the
    // config of whichever test got there first decides where, or whether, the schedule is written.
    // Under `cargo nextest`, which CI uses and which gives each test its own process, it is ours.
    let Some(persisted) = std::fs::read_dir(dir.path()).unwrap().next() else {
        return;
    };

    // The message embeds the sum the tasks computed, so it only matches if the replay of the
    // recorded schedule reproduced the same interleaving *and* the same random values.
    assert_eq!(run_to_failure(Some(persisted.unwrap().path()), None), expected);
}

/// Records the interleaving of a program, and the random values it was given, in a non-Shuttle log.
fn record_interleaving(log: Arc<std::sync::Mutex<Vec<(usize, u64)>>>) -> impl Fn() + Send + Sync + 'static {
    move || {
        let threads = (0..3)
            .map(|_| {
                let log = Arc::clone(&log);
                thread::spawn(move || {
                    use shuttle::rand::Rng;
                    for _ in 0..5 {
                        let value = shuttle::rand::thread_rng().gen::<u64>();
                        log.lock().unwrap().push((me(), value));
                        thread::yield_now();
                    }
                })
            })
            .collect::<Vec<_>>();

        for thd in threads {
            thd.join().unwrap();
        }
    }
}

/// Continuing after the schedule stays deterministic: the continuation scheduler is seeded from the
/// replayed schedule, so the same schedule gives the same interleaving and the same random values.
#[test]
fn replay_continue_after_schedule_is_deterministic() {
    let run = || {
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let schedule = Schedule::new_from_task_ids(0, vec![0, 0]);
        let mut scheduler = ReplayScheduler::new_from_schedule(schedule);
        scheduler.set_continue_after_schedule();
        let runner = Runner::new(scheduler, Default::default());
        runner.run(record_interleaving(Arc::clone(&log)));
        let log = std::mem::take(&mut *log.lock().unwrap());
        log
    };

    let first = run();
    assert_eq!(first.len(), 15, "the program ran to completion");
    assert_eq!(first, run());
}

fn long_schedule() {
    let mut threads = vec![];
    for _ in 0..100 {
        threads.push(shuttle::thread::spawn(|| {
            for _ in 0..100 {
                shuttle::thread::yield_now();
            }
        }));
    }
    for t in threads {
        t.join().unwrap();
    }
    // If this would be a `panic!`, downcasting the `catch_unwind` error to `String` fails.
    assert_eq!(1, 2, "so much work, and all for nothing");
}

#[test]
fn replay_long_schedule() {
    check_replay_roundtrip(long_schedule, RandomScheduler::new(1));
}

#[test]
fn replay_long_schedule_file() {
    check_replay_roundtrip_file(long_schedule, RandomScheduler::new(1));
}

// Check that FailurePersistence::None does not print a schedule
#[test]
fn replay_persist_none() {
    let result = panic::catch_unwind(|| {
        let scheduler = PctScheduler::new(2, 100);
        let mut config = Config::new();
        config.failure_persistence = FailurePersistence::None;
        let runner = Runner::new(scheduler, config);
        runner.run(concurrent_increment_buggy);
    })
    .expect_err("test should panic");
    let output = result.downcast::<String>().unwrap();
    assert!(output.contains("counter is wrong"));
    // All our current failure persistence modes print the word "schedule", so check that's missing
    assert!(!output.contains("schedule"));
}

/// Tests that events not causally related to the failure are never scheduled.
#[test]
fn replay_causality() {
    // The main thread will spawn three threads:
    // - A, which acquires the lock and sets it to one;
    // - B, which acquires the lock and asserts it is zero;
    // - C, which sets an unrelated atomic Boolean.
    // If A runs before B (as in the schedule below), then the assertion
    // fails. If we provide the clock of this failure to the scheduler, we
    // should never see thread C do anything, i.e., the atomic Boolean should
    // never be set, because it is not causally related to the actual panic.

    use std::sync::atomic::{AtomicBool, Ordering};

    let flag = Arc::new(AtomicBool::new(false));
    let flag_clone = Arc::clone(&flag);

    let result = panic::catch_unwind(|| {
        let schedule = Schedule::new_from_task_ids(0, vec![0, 0, 1, 1, 0, 0, 3, 2, 0, 1, 2, 2]);
        let mut scheduler = ReplayScheduler::new_from_schedule(schedule);
        scheduler.set_target_clock(&[2, 2, 1]);
        let mut config = Config::new();
        config.failure_persistence = FailurePersistence::None;
        let runner = Runner::new(scheduler, config);
        runner.run(move || {
            assert_eq!(me(), 0);
            let lock = Arc::new(Mutex::new(0usize));
            let lock_clone = Arc::clone(&lock);
            thread::spawn(move || {
                assert_eq!(me(), 1);
                *lock_clone.lock().unwrap() = 1;
            });
            thread::spawn(move || {
                assert_eq!(me(), 2);
                let guard = lock.lock().unwrap();
                assert!(*guard == 0, "expected panic");
                drop(guard);
            });
            let flag_clone = Arc::clone(&flag_clone);
            thread::spawn(move || {
                assert_eq!(me(), 3);
                // Note that this operation is performed in a separate thread,
                // since the (non-Shuttle) atomic does not increment the clock
                // of the current thread. If the atomic were set instead in the
                // main thread, then the clocks of "setting the atomic" and "B
                // acquiring a lock" would be indistinguishable. However, we
                // need the non-Shuttle atomic to smuggle data out of a this
                // panicking test.
                flag_clone.store(true, Ordering::SeqCst);
            });
        });
    })
    .expect_err("test should panic");
    let output = result.downcast::<&str>().unwrap();
    assert_eq!(*output, "expected panic");

    assert!(!flag.load(Ordering::SeqCst));
}

/// Similar to `replay_causality`, but with a schedule that also contains
/// random choice steps.
#[test]
fn replay_causality_with_random() {
    // The thread setup here is the same as in `replay_causality`, but thread
    // 3 is using the RNG rather than setting a Boolean flag.

    let result = panic::catch_unwind(|| {
        // Manually construct a schedule, to show explicitly the thread steps
        // and the random steps made for thread 3 (which are irrelevant to the
        // failure being replayed).
        let mut schedule = Schedule::new(0);
        schedule.push_task(0.into());
        schedule.push_task(0.into());
        schedule.push_task(1.into());
        schedule.push_task(1.into());
        schedule.push_task(0.into());
        schedule.push_task(0.into());
        schedule.push_task(3.into());
        schedule.push_random();
        schedule.push_random();
        schedule.push_random();
        schedule.push_task(2.into());
        schedule.push_task(0.into());
        schedule.push_task(1.into());
        schedule.push_task(2.into());
        schedule.push_task(2.into());

        let mut scheduler = ReplayScheduler::new_from_schedule(schedule);
        scheduler.set_target_clock(&[2, 2, 1]);
        let mut config = Config::new();
        config.failure_persistence = FailurePersistence::None;
        let runner = Runner::new(scheduler, config);
        runner.run(move || {
            assert_eq!(me(), 0);
            let lock = Arc::new(Mutex::new(0usize));
            let lock_clone = Arc::clone(&lock);
            thread::spawn(move || {
                assert_eq!(me(), 1);
                *lock_clone.lock().unwrap() = 1;
            });
            thread::spawn(move || {
                assert_eq!(me(), 2);
                let guard = lock.lock().unwrap();
                assert!(*guard == 0, "expected panic");
                drop(guard);
            });
            thread::spawn(move || {
                use shuttle::rand::Rng;
                assert_eq!(me(), 3);
                let mut thread_rng = shuttle::rand::thread_rng();
                thread_rng.gen::<u64>();
                thread_rng.gen::<u64>();
                thread_rng.gen::<u64>();
            });
        });
    })
    .expect_err("test should panic");
    let output = result.downcast::<&str>().unwrap();
    assert_eq!(*output, "expected panic");
}
