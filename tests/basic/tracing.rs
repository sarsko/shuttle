use shuttle::{thread, check_random};
use shuttle::sync::{Arc, Mutex};
use tracing::{warn_span, warn};
use test_log::test;

#[test]
fn tracing_nested_spans() {
    check_random(|| {
        let lock = Arc::new(Mutex::new(0));
        let threads: Vec<_> = (0..3).map(|i| {
            let lock = lock.clone();
            thread::spawn(move || {
                let outer = warn_span!("outer", tid=i);
                let _outer = outer.enter();
                {
                    let mut locked = lock.lock().unwrap();
                    let inner = warn_span!("inner", tid=i);
                    let _inner = inner.enter();
                    warn!("incrementing from {}", *locked);
                    *locked += 1;
                }
            })
        }).collect();

        for thread in threads {
            thread.join().unwrap();
        }
    }, 10)
}