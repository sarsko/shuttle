use futures::stream::FuturesUnordered;
use futures::StreamExt;
use shuttle::{prntln, prnt};

fn test_futures_unordered() {
    shuttle::future::block_on(async {
        let tasks = (0..1)
            .map(|_| shuttle::future::spawn(async move {
                prntln!(true, "\n ??????? In the future\n\n");
            }))
            .collect::<FuturesUnordered<_>>();

        prntln!(true, "Bound tasks, awaiting");

        let _t = tasks.collect::<Vec<_>>().await;
        prntln!(true, "Collected successfully");
    });
}

#[test]
fn streams_one_fut(){
    let _ = tracing_subscriber::fmt::try_init();
    shuttle::check_random(test_futures_unordered, 10);
}

fn test_futures_unordered_spawn_collect() {
    shuttle::future::block_on(async {
        let tasks = (0..1)
            .map(|_| shuttle::future::spawn(async move {prntln!(true, "In the future");}))
            .collect::<FuturesUnordered<_>>();

        prntln!(true, "Bound tasks, awaiting");

        let _t = shuttle::future::spawn( async { tasks.collect::<Vec<_>>().await; }).await;
        prntln!(true, "Collected successfully");
    });
}

#[test]
fn streams_one_fu_spawn_collect(){
    let _ = tracing_subscriber::fmt::try_init();
    shuttle::check_random(test_futures_unordered_spawn_collect, 10);
}

pub fn test_futures_unordered_next() {
    shuttle::future::block_on(async {
        let mut tasks = (0..3)
            .map(|_| shuttle::future::spawn(async move {}))
            .collect::<FuturesUnordered<_>>();

        while let Some(result) = tasks.next().await {
            assert_eq!(result.unwrap(), ());
            println!("========== finished task");
        }
    });
}

#[test]
fn streams_next(){
    let _ = tracing_subscriber::fmt::try_init();
    shuttle::check_random(test_futures_unordered_next, 10);
}

pub fn test_futures_unordered_next_block_on_spawn() {
    shuttle::future::block_on(async {let _res = shuttle::future::spawn(async {
        let mut tasks = (0..3)
            .map(|_| shuttle::future::spawn(async move {}))
            .collect::<FuturesUnordered<_>>();

        while let Some(result) = tasks.next().await {
            assert_eq!(result.unwrap(), ());
            println!("========== finished task");
        }
    });});
}

#[test]
fn streams_block_on_spawn(){
    let _ = tracing_subscriber::fmt::try_init();
    shuttle::check_random(test_futures_unordered_next_block_on_spawn, 10);
}