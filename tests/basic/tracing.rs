//use shuttle::{check_random, thread};
use test_log::test;
use tracing::{info, trace};
use tracing_attributes::instrument;

use futures::future::join_all;
use shuttle::{check_random, future::block_on, thread};
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
                spawn_some_futures_and_set_tag(i);
            })
        })
        .collect();

    threads.into_iter().for_each(|t| t.join().expect("Failed"));
}

//use tracing::info_span;
use tracing::subscriber;
use tracing_subscriber::{fmt, EnvFilter};
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
