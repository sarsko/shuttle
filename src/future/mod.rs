//! Shuttle's implementation of an async executor, roughly equivalent to [`futures::executor`].
//!
//! The [spawn] method spawns a new asynchronous task that the executor will run to completion. The
//! [block_on] method blocks the current thread on the completion of a future.
//!
//! [`futures::executor`]: https://docs.rs/futures/0.3.13/futures/executor/index.html

use crate::runtime::execution::ExecutionState;
use crate::runtime::task::TaskId;
use crate::runtime::thread;
use std::future::Future;
use std::pin::Pin;
use std::result::Result;
use std::task::{Context, Poll};

/// Spawn a new async task that the executor will run to completion.
pub fn spawn<T, F>(fut: F) -> JoinHandle<T>
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let result = std::sync::Arc::new(std::sync::Mutex::new(None));
    let stack_size = ExecutionState::with(|s| s.config.stack_size);
    let task_id = ExecutionState::spawn_future(Wrapper::new(fut, std::sync::Arc::clone(&result)), stack_size, None);

    prntln!(true, "JH SPAWN -- Pre switch");
    thread::switch();
    prntln!(true, "JH SPAWN -- Post switch");

    JoinHandle { task_id, result }
}

/// An owned permission to join on an async task (await its termination).
#[derive(Debug)]
pub struct JoinHandle<T> {
    task_id: TaskId,
    result: std::sync::Arc<std::sync::Mutex<Option<Result<T, JoinError>>>>,
}

impl<T> JoinHandle<T> {
    /// Abort the task associated with the handle.
    pub fn abort(&self) {
        prntln!(true, "JH ABORT");
        ExecutionState::try_with(|state| {
            if !state.is_finished() {
                let task = state.get_mut(self.task_id);
                println!("JH DETACHING");
                task.detach();
            }
        });
    }
}

// TODO: need to work out all the error cases here
/// Task failed to execute to completion.
#[derive(Debug)]
pub enum JoinError {
    /// Task was aborted
    Cancelled,
}

impl<T> Drop for JoinHandle<T> {
    fn drop(&mut self) {
        prntln!(true, "JH DROPPING");
        self.abort();
    }
}

impl<T> Future for JoinHandle<T> {
    type Output = Result<T, JoinError>;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        prntln!(true, "JH POLLING ==");
        /*
        // Ok so it has to return Poll::Pending;
        // Hmm nevermind? It is stuck somewhere else than the loop
        loop {
            println!("LOOPING");
            let lock = self.result.lock().unwrap().take();
            if let Some(result) = lock {
                prntln!(true, "JH POLLING == Ready");
                return Poll::Ready(result);
            } else {
                prntln!(true, "JH POLLING == NOT Ready");
                ExecutionState::with(|state| {
                    let me = state.current().id();
                    let r = state.get_mut(self.task_id).set_waiter(me);
                    println!("JH POLLING == NOT Ready -- Setting waiter for: {:?}, waiter: {:?}", self.task_id, me);
                    assert!(r, "task shouldn't be finished if no result is present");
                });
                thread::switch(); // Added by me
            }
        }
        */
        let lock = self.result.lock().unwrap().take();
        if let Some(result) = lock {
        //if let Some(result) = self.result.lock().unwrap().take() {
            prntln!(true, "JH POLLING == Ready");
            Poll::Ready(result)
        } else {
            prntln!(true, "JH POLLING == NOT Ready");
            ExecutionState::with(|state| {
                let me = state.current().id();
                let r = state.get_mut(self.task_id).set_waiter(me);
                println!("JH POLLING == NOT Ready -- Setting waiter for: {:?}, waiter: {:?}", self.task_id, me);
                assert!(r, "task shouldn't be finished if no result is present");
                //state.current_mut().block();
            });
            //_cx.waker().wake_by_ref(); // Oh hey this actually made less things fail
            //thread::switch(); // Added by me
            Poll::Pending
        }
    }
}

// We wrap a task returning a value inside a wrapper task that returns ().  The wrapper
// contains a mutex-wrapped field that stores the value where it can be passed to a task
// waiting on the join handle.
struct Wrapper<T, F> {
    future: Pin<Box<F>>,
    result: std::sync::Arc<std::sync::Mutex<Option<Result<T, JoinError>>>>,
}

impl<T, F> Wrapper<T, F>
where
    F: Future<Output = T> + Send + 'static,
{
    fn new(future: F, result: std::sync::Arc<std::sync::Mutex<Option<Result<T, JoinError>>>>) -> Self {
        Self {
            future: Box::pin(future),
            result,
        }
    }
}

impl<T, F> Future for Wrapper<T, F>
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        println!("------------- WRAPPER"); // I don't think this should happen
        let out = match self.future.as_mut().poll(cx) {
            Poll::Ready(result) => {
                println!("------------- WRAPPER READY"); // I don't think this should happen
                // Run thread-local destructors before publishing the result.
                // See `pop_local` for details on why this loop looks this slightly funky way.
                // TODO: thread locals and futures don't mix well right now. each task gets its own
                //       thread local storage, but real async code might know that its executor is
                //       single-threaded and so use TLS to share objects across all tasks.
                while let Some(local) = ExecutionState::with(|state| state.current_mut().pop_local()) {
                    println!("------------- WRAPPER READY -- Popping local"); // I don't think this should happen
                    drop(local);
                }

                // OK so if we keep looping in JoinHandle then we get stuck here
                println!("------------- WRAPPER READY -- Writing to lock"); // I don't think this should happen
                *self.result.lock().unwrap() = Some(Ok(result));

                // Unblock our waiter if we have one and it's still alive
                ExecutionState::with(|state| {
                    println!("------------- WRAPPER READY -- If let"); // I don't think this should happen
                    if let Some(waiter) = state.current_mut().take_waiter() {
                        if !state.get_mut(waiter).finished() {
                            state.get_mut(waiter).unblock();
                        }
                    }
                });
                println!("------------- WRAPPER RETURN READY"); // I don't think this should happen
                Poll::Ready(())
            }
            Poll::Pending => {
                println!("------------- WRAPPER PENDING"); // I don't think this should happen
                Poll::Pending
            },
        };
        println!("------------- WRAPPER END"); // I don't think this should happen
        out
    }
}

use crate::prntln;

/// Run a future to completion on the current thread.
pub fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = Box::pin(future);
    let waker = ExecutionState::with(|state| state.current_mut().waker());
    let cx = &mut Context::from_waker(&waker);

    prntln!(true, "BLOCK ON -- SWITCH");
    thread::switch();
    prntln!(true, "BLOCK ON -- SWITCH AFTER");

    loop {
        prntln!(true, "BLOCK ON -- LOOP -- Polling the future");
        match future.as_mut().poll(cx) {
            Poll::Ready(result) => {
                prntln!(true, "BLOCK ON -- Ready : Breaking");
                break result
            },
            Poll::Pending => {
                prntln!(true, "BLOCK ON -- PENDING : Sleep unless woken");
                ExecutionState::with(|state| state.current_mut().sleep_unless_woken());
                prntln!(true, "BLOCK ON -- PENDING : Put it to sleep");
            }
        }

        prntln!(true, "BLOCK ON -- AFTER MATCH : Switching");
        thread::switch();
        prntln!(true, "BLOCK ON -- AFTER MATCH : After switch");
    }
}

/// Yields execution back to the scheduler.
///
/// Borrowed from the Tokio implementation.
pub async fn yield_now() {
    println!("------------- YIELD NOW"); // I don't think this should happen (to any extent)
    /// Yield implementation
    struct YieldNow {
        yielded: bool,
    }

    impl Future for YieldNow {
        type Output = ();

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            println!("------------- YIELD"); // I don't think this should happen (to any extent)
            if self.yielded {
                return Poll::Ready(());
            }

            self.yielded = true;
            cx.waker().wake_by_ref();
            ExecutionState::request_yield();
            Poll::Pending
        }
    }

    YieldNow { yielded: false }.await
}
