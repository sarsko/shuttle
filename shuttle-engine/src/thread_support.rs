use crate::runtime::execution::{self, ExecutionState};
use crate::runtime::task::{Event, EventKind};
use crate::runtime::thread;
use std::cell::{Cell, RefCell};
use std::marker::PhantomData;

/// Cooperatively gives up a timeslice to the Shuttle scheduler.
#[track_caller]
pub fn yield_now() {
    let waker = ExecutionState::with(|state| state.current().waker());
    waker.wake_by_ref();
    ExecutionState::request_yield();
    thread::switch_with(Event::untargeted(EventKind::Yield));
}

/// The body of a spawned thread. Runs `f`, drops thread locals, publishes result.
pub fn thread_fn<F, T>(
    f: F,
    switch_before_exit: bool,
    result: std::sync::Arc<std::sync::Mutex<Option<std::thread::Result<T>>>>,
) where
    F: FnOnce() -> T,
{
    let ret = f();

    if switch_before_exit && ExecutionState::with(|s| s.exit_current_truncates_execution()) {
        thread::switch_with(Event::on_task(EventKind::Exit, ExecutionState::me()));
    }

    // Nobody can join a thread whose `JoinHandle` is gone, so drop what it returned now, while its
    // thread-locals are still there for the value's destructors, as std does.
    let ret = if std::sync::Arc::strong_count(&result) > 1 {
        Some(ret)
    } else {
        drop(ret);
        None
    };

    tracing::trace!("thread finished, dropping thread locals");
    ExecutionState::drop_task_locals();
    tracing::trace!("done dropping thread locals");

    if let Some(ret) = ret {
        *result.lock().unwrap() = Some(Ok(ret));
    }
    ExecutionState::with(|state| {
        if let Some(waiter) = state.current_mut().take_waiter() {
            state.get_mut(waiter).unblock();
        }
    });
}

/// A key into Shuttle's thread-local storage.
pub struct LocalKey<T: 'static> {
    #[doc(hidden)]
    pub init: fn() -> T,
    // A `LocalKey` holds no `T` of its own, only the means to reach each thread's, so like
    // `std::thread::LocalKey` it is `Send`, `Sync` and unwind safe whatever `T` is. `fn() -> T`
    // gives it all four.
    #[doc(hidden)]
    pub _p: PhantomData<fn() -> T>,
}

/// A [`LocalKey`] whose slot is destroyed after all of a thread's other thread-locals when the
/// thread finishes, so that their destructors can still use it however early it was created.
/// `shuttle-tokio` uses it for the slots of its task-locals, which in tokio are thread-locals that
/// outlive every task.
#[doc(hidden)]
pub struct DestroyedLastKey<T: 'static> {
    key: LocalKey<T>,
}

impl<T: 'static> DestroyedLastKey<T> {
    /// A key whose slots start out as what `init` returns.
    pub const fn new(init: fn() -> T) -> Self {
        Self {
            key: LocalKey { init, _p: PhantomData },
        }
    }

    /// Like [`LocalKey::try_with`].
    pub fn try_with<F, R>(&'static self, f: F) -> std::result::Result<R, AccessError>
    where
        F: FnOnce(&T) -> R,
    {
        self.key.try_with_slot(f, true)
    }
}

impl<T: 'static> std::fmt::Debug for DestroyedLastKey<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DestroyedLastKey").finish_non_exhaustive()
    }
}

impl<T: 'static> std::fmt::Debug for LocalKey<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalKey").finish_non_exhaustive()
    }
}

impl<T: 'static> LocalKey<T> {
    /// Acquires a reference to the value in this TLS key.
    ///
    /// This will lazily initialize the value if this thread has not referenced this key yet.
    pub fn with<F, R>(&'static self, f: F) -> R
    where
        F: FnOnce(&T) -> R,
    {
        self.try_with(f).expect(
            "cannot access a Thread Local Storage value \
            during or after destruction",
        )
    }

    /// Acquires a reference to the value in this TLS key.
    ///
    /// This will lazily initialize the value if this thread has not referenced this key yet. If the
    /// key has been destroyed (which may happen if this is called in a destructor), this function
    /// will return an AccessError.
    pub fn try_with<F, R>(&'static self, f: F) -> std::result::Result<R, AccessError>
    where
        F: FnOnce(&T) -> R,
    {
        self.try_with_slot(f, false)
    }

    /// `try_with`, creating the slot, if it has to, as one that is destroyed after the thread's other
    /// slots if `destroyed_last` is set.
    ///
    /// The current task's storage is kept out of `ExecutionState` (see `TASK_STORAGE`), so this
    /// works while Shuttle is updating its own state too, as in a `tracing` subscriber it calls then.
    fn try_with_slot<F, R>(&'static self, f: F, destroyed_last: bool) -> std::result::Result<R, AccessError>
    where
        F: FnOnce(&T) -> R,
    {
        let task = execution::expect_current_task_id();
        let value = match execution::task_local::<T>(task, self.into()) {
            Some(value) => value,
            None => {
                let value = (self.init)();
                // `init` may have used this key, and so created the slot, since there was none.
                // Keep that slot then.
                if execution::task_local::<T>(task, self.into()).is_none() {
                    execution::init_task_local(task, self.into(), value, destroyed_last);
                } else {
                    drop(value);
                }
                execution::task_local::<T>(task, self.into()).expect("the slot was just created")
            }
        };
        let value = value.map_err(|_| AccessError)?;

        Ok(f(value))
    }

    /// Writes `value` into this key's slot, creating the slot with it when the task has none, and
    /// runs `f` on the slot. Unlike [`with`](LocalKey::with), this does not run the key's
    /// initializer. `f` gets `value` back when there already was a slot, which is how
    /// [`LocalKey::<Cell<T>>::set`](LocalKey::set) writes it into the slot that is there.
    ///
    /// This is std's `LocalKey::initialize_with`, which std's `set` is built on.
    fn initialize_with<F, R>(&'static self, value: T, f: F) -> R
    where
        F: FnOnce(Option<T>, &T) -> R,
    {
        if let Some(slot) = self.slot() {
            return f(Some(value), Self::unwrap_slot(slot));
        }

        // Nothing runs between the lookup above and this, so nothing can have created the slot.
        let task = execution::expect_current_task_id();
        execution::init_task_local(task, self.into(), value, false);

        let slot = Self::unwrap_slot(self.slot().expect("the slot exists now"));
        f(None, slot)
    }

    fn unwrap_slot(slot: std::result::Result<&'static T, AccessError>) -> &'static T {
        slot.expect(
            "cannot access a Thread Local Storage value \
            during or after destruction",
        )
    }

    /// The current task's slot for this key: `None` if it was never created, an error if it was
    /// destroyed.
    fn slot(&'static self) -> Option<std::result::Result<&'static T, AccessError>> {
        let task = execution::expect_current_task_id();
        execution::task_local::<T>(task, self.into()).map(|value| value.map_err(|_| AccessError))
    }
}

impl<T: 'static> LocalKey<Cell<T>> {
    /// Sets or initializes the contained value.
    ///
    /// Unlike the other methods, this will *not* run the lazy initializer of the thread local.
    /// Instead, it will be directly initialized with the given value if it wasn't initialized yet.
    pub fn set(&'static self, value: T) {
        self.initialize_with(Cell::new(value), |value, cell| {
            if let Some(value) = value {
                // The slot was already there, so the cell this made was not used. Write what it
                // holds into the slot instead.
                cell.set(value.into_inner());
            }
        });
    }

    /// Returns a copy of the contained value.
    ///
    /// This will lazily initialize the value if this task has not referenced this key yet.
    pub fn get(&'static self) -> T
    where
        T: Copy,
    {
        self.with(Cell::get)
    }

    /// Takes the contained value, leaving `Default::default()` in its place.
    ///
    /// This will lazily initialize the value if this task has not referenced this key yet.
    pub fn take(&'static self) -> T
    where
        T: Default,
    {
        self.with(Cell::take)
    }

    /// Replaces the contained value, returning the old value.
    ///
    /// This will lazily initialize the value if this task has not referenced this key yet.
    pub fn replace(&'static self, value: T) -> T {
        self.with(|cell| cell.replace(value))
    }
}

impl<T: 'static> LocalKey<RefCell<T>> {
    /// Acquires a reference to the contained value.
    ///
    /// This will lazily initialize the value if this task has not referenced this key yet.
    ///
    /// # Panics
    ///
    /// Panics if the value is currently mutably borrowed.
    pub fn with_borrow<F, R>(&'static self, f: F) -> R
    where
        F: FnOnce(&T) -> R,
    {
        self.with(|cell| f(&cell.borrow()))
    }

    /// Acquires a mutable reference to the contained value.
    ///
    /// This will lazily initialize the value if this task has not referenced this key yet.
    ///
    /// # Panics
    ///
    /// Panics if the value is currently borrowed.
    pub fn with_borrow_mut<F, R>(&'static self, f: F) -> R
    where
        F: FnOnce(&mut T) -> R,
    {
        self.with(|cell| f(&mut cell.borrow_mut()))
    }

    /// Sets or initializes the contained value.
    ///
    /// Unlike the other methods, this will *not* run the lazy initializer of the thread local.
    /// Instead, it will be directly initialized with the given value if it wasn't initialized yet.
    ///
    /// # Panics
    ///
    /// Panics if the value is currently borrowed.
    pub fn set(&'static self, value: T) {
        self.initialize_with(RefCell::new(value), |value, cell| {
            if let Some(value) = value {
                // The slot was already there, so the cell this made was not used. Write what it
                // holds into the slot instead.
                *cell.borrow_mut() = value.into_inner();
            }
        });
    }

    /// Takes the contained value, leaving `Default::default()` in its place.
    ///
    /// This will lazily initialize the value if this task has not referenced this key yet.
    ///
    /// # Panics
    ///
    /// Panics if the value is currently borrowed.
    pub fn take(&'static self) -> T
    where
        T: Default,
    {
        self.with(RefCell::take)
    }

    /// Replaces the contained value, returning the old value.
    ///
    /// # Panics
    ///
    /// Panics if the value is currently borrowed.
    pub fn replace(&'static self, value: T) -> T {
        self.with(|cell| cell.replace(value))
    }
}

/// An error returned by [`LocalKey::try_with`]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub struct AccessError;

impl std::fmt::Display for AccessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt("already destroyed", f)
    }
}

impl std::error::Error for AccessError {}
