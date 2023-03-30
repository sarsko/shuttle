use shuttle::sync::{Arc, Mutex};
use shuttle::{check_random, thread};
use test_log::test;
use tracing::{warn, warn_span};

#[test]
fn tracing_nested_spans() {
    check_random(
        || {
            let lock = Arc::new(Mutex::new(0));
            let threads: Vec<_> = (0..3)
                .map(|i| {
                    let lock = lock.clone();
                    thread::spawn(move || {
                        let outer = warn_span!("outer", tid = i);
                        let _outer = outer.enter();
                        {
                            let mut locked = lock.lock().unwrap();
                            let inner = warn_span!("inner", tid = i);
                            let _inner = inner.enter();
                            warn!("incrementing from {}", *locked);
                            *locked += 1;
                        }
                    })
                })
                .collect();

            for thread in threads {
                thread.join().unwrap();
            }
        },
        10,
    )
}

use tracing::{info, trace};
use tracing_attributes::instrument;

use futures::future::join_all;
use shuttle::future::block_on;
#[instrument(skip_all, fields(num_threads = ?num_threads))]
fn spawn_some_futures_and_set_tag(num_threads: i64) {
    let threads: Vec<_> = (0..num_threads)
        .map(|_| shuttle::future::spawn(async move {}))
        .collect();

    block_on(join_all(threads));
}

#[instrument(skip_all, fields(num_threads = ?num_threads))]
fn spawn_some_threads_and_set_tag(num_threads: i64) {
    let threads: Vec<_> = (0..num_threads).map(|_| thread::spawn(move || {})).collect();

    threads.into_iter().for_each(|t| t.join().expect("Failed"));
}

fn spawn_threads_which_spawn_more_threads(num_threads: i64) {
    std::env::set_var("RUST_LOG", "trace");
    trace!("==== Entry point ===== ");
    info!("THIS IS AN INFO",);
    let threads: Vec<_> = (0..num_threads)
        .map(|i| {
            thread::spawn(move || {
                info!("THIS IS AN INFO",);
                trace!("--------- SPAWN --------");
                spawn_some_threads_and_set_tag(i);
                spawn_some_threads_and_set_tag(i);
                spawn_some_threads_and_set_tag(i);
                //spawn_some_futures_and_set_tag(i);
            })
        })
        .collect();

    threads.into_iter().for_each(|t| t.join().expect("Failed"));
}

//use tracing::info_span;
#[test]
fn threads_which_spawn_threads_which_spawn_threads() {
    /*
    std::env::set_var("TEST_TRACE", "trace");
    let subscriber = fmt::Subscriber::builder()
        .with_env_filter(EnvFilter::from_env("TEST_TRACE"))
        .with_test_writer()
        .without_time()
        .finish();
    //check_random(|| spawn_threads_which_spawn_more_threads(20), 4);
    subscriber::with_default(subscriber, || {
        check_random(|| spawn_threads_which_spawn_more_threads(20), 4);
    });
    */
    check_random(|| spawn_threads_which_spawn_more_threads(20), 4);
}
