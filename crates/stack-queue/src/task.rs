use std::{
  cell::UnsafeCell,
  fmt,
  fmt::Debug,
  future::Future,
  marker::PhantomPinned,
  mem::MaybeUninit,
  ops::Deref,
  pin::Pin,
  ptr::addr_of,
  task::{Context, Poll, Waker},
};
#[cfg(not(loom))]
use std::{
  hint::unreachable_unchecked,
  sync::atomic::{AtomicUsize, Ordering},
  thread::yield_now,
};

#[cfg(feature = "diesel-associations")]
use diesel::associations::BelongsTo;
#[cfg(loom)]
use loom::{
  hint::unreachable_unchecked,
  sync::atomic::{AtomicUsize, Ordering},
  thread::yield_now,
};
use pin_project::{pin_project, pinned_drop};

use crate::queue::{LocalQueue, QueueFull, TaskQueue};

pub(crate) const SETTING_VALUE: usize = 1 << 0;
pub(crate) const VALUE_SET: usize = 1 << 1;
pub(crate) const RECEIVER_DROPPED: usize = 1 << 2;

/// A raw pointer backed reference to the pinned receiver of an enqueued [`AutoBatchedTask`]
pub struct TaskRef<T: TaskQueue> {
  state: UnsafeCell<AtomicUsize>,
  rx: UnsafeCell<MaybeUninit<*const Receiver<T>>>,
  task: UnsafeCell<MaybeUninit<T::Task>>,
}

impl<T> Deref for TaskRef<T>
where
  T: TaskQueue,
{
  type Target = T::Task;
  fn deref(&self) -> &Self::Target {
    self.task()
  }
}

impl<T> PartialEq for TaskRef<T>
where
  T: TaskQueue,
  T::Task: PartialEq,
{
  fn eq(&self, other: &Self) -> bool {
    self.task().eq(other.task())
  }
}

impl<T> PartialOrd for TaskRef<T>
where
  T: TaskQueue,
  T::Task: PartialOrd,
{
  fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
    self.task().partial_cmp(other.task())
  }
}

impl<T> TaskRef<T>
where
  T: TaskQueue,
{
  pub(crate) fn new_uninit() -> Self {
    TaskRef {
      state: UnsafeCell::new(AtomicUsize::new(0)),
      rx: UnsafeCell::new(MaybeUninit::uninit()),
      task: UnsafeCell::new(MaybeUninit::uninit()),
    }
  }

  #[inline(always)]
  pub(crate) fn state(&self) -> &AtomicUsize {
    unsafe { &*self.state.get() }
  }

  #[inline(always)]
  pub(crate) fn state_ptr(&self) -> *const AtomicUsize {
    self.state() as *const AtomicUsize
  }

  #[cfg(not(loom))]
  #[inline(always)]
  unsafe fn reset_state(&self) {
    *(*self.state.get()).get_mut() = 0;
  }

  #[cfg(loom)]
  #[inline(always)]
  unsafe fn reset_state(&self) {
    (*self.state.get()).with_mut(|state| *state = 0);
  }

  #[inline(always)]
  pub(crate) fn rx(&self) -> &Receiver<T> {
    unsafe { &**(*self.rx.get()).assume_init_ref() }
  }

  #[allow(clippy::mut_from_ref)]
  #[inline(always)]
  unsafe fn rx_mut(&self) -> &mut MaybeUninit<*const Receiver<T>> {
    &mut *self.rx.get()
  }

  #[inline(always)]
  pub fn task(&self) -> &T::Task {
    unsafe { (*self.task.get()).assume_init_ref() }
  }

  #[allow(clippy::mut_from_ref)]
  #[inline(always)]
  unsafe fn task_mut(&self) -> &mut MaybeUninit<T::Task> {
    &mut *self.task.get()
  }

  pub(crate) unsafe fn set_task(&self, task: T::Task, rx: *const Receiver<T>) {
    self.reset_state();
    self.rx_mut().write(rx);
    self.task_mut().write(task);
  }

  #[inline(always)]
  pub(crate) unsafe fn take_task_unchecked(&self) -> T::Task {
    std::mem::replace(&mut *self.task.get(), MaybeUninit::uninit()).assume_init()
  }

  /// Set value in receiver and wake if the receiver isn't already dropped. This takes &self because
  /// [`TaskRef`] by design is never dropped
  pub(crate) unsafe fn resolve_unchecked(&self, value: T::Value) {
    let state = self.state().fetch_or(SETTING_VALUE, Ordering::AcqRel);

    if (state & RECEIVER_DROPPED).eq(&0) {
      let rx = self.rx();
      *rx.value.get() = MaybeUninit::new(value);
      rx.take_waker_unchecked().wake();
      self
        .state()
        .fetch_xor(SETTING_VALUE | VALUE_SET, Ordering::Release);
    }
  }
}

impl<T> Debug for TaskRef<T>
where
  T: TaskQueue,
  <T as TaskQueue>::Task: Debug,
{
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "{:?}", self.task())
  }
}

unsafe impl<T> Sync for TaskRef<T> where T: TaskQueue {}

#[cfg(feature = "diesel-associations")]
impl<T, Parent> BelongsTo<Parent> for TaskRef<T>
where
  T: TaskQueue,
  T::Task: BelongsTo<Parent>,
{
  type ForeignKey = <T::Task as BelongsTo<Parent>>::ForeignKey;

  type ForeignKeyColumn = <T::Task as BelongsTo<Parent>>::ForeignKeyColumn;

  fn foreign_key(&self) -> Option<&Self::ForeignKey> {
    self.task().foreign_key()
  }

  fn foreign_key_column() -> Self::ForeignKeyColumn {
    <T::Task as BelongsTo<Parent>>::foreign_key_column()
  }
}

#[pin_project]
pub(crate) struct Receiver<T: TaskQueue> {
  state: *const AtomicUsize,
  value: UnsafeCell<MaybeUninit<T::Value>>,
  waker: UnsafeCell<MaybeUninit<Waker>>,
  pin: PhantomPinned,
}

impl<T> Receiver<T>
where
  T: TaskQueue,
{
  fn new(state: *const AtomicUsize, waker: Waker) -> Self {
    Receiver {
      state,
      value: UnsafeCell::new(MaybeUninit::uninit()),
      waker: UnsafeCell::new(MaybeUninit::new(waker)),
      pin: PhantomPinned,
    }
  }

  #[inline(always)]
  fn state(&self) -> &AtomicUsize {
    unsafe { &*self.state }
  }

  unsafe fn take_waker_unchecked(&self) -> Waker {
    std::mem::replace(&mut *self.waker.get(), MaybeUninit::uninit()).assume_init()
  }
}

// This is safe because state is guaranteed to be immovable and to exist
unsafe impl<T> Send for Receiver<T> where T: TaskQueue {}
unsafe impl<T> Sync for Receiver<T> where T: TaskQueue {}

#[pin_project(project = StateProj)]
enum State<T: TaskQueue> {
  Unbatched { task: T::Task },
  Batched(#[pin] Receiver<T>),
  Received,
}

/// An automatically batched task
#[pin_project(project = AutoBatchProj, PinnedDrop)]
pub struct AutoBatchedTask<T: LocalQueue<N>, const N: usize = 2048> {
  state: State<T>,
}

impl<T, const N: usize> AutoBatchedTask<T, N>
where
  T: LocalQueue<N>,
{
  /// Create a new auto batched task
  pub fn new(task: T::Task) -> Self {
    AutoBatchedTask {
      state: State::Unbatched { task },
    }
  }
}

impl<T, const N: usize> Future for AutoBatchedTask<T, N>
where
  T: LocalQueue<N>,
{
  type Output = T::Value;

  fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
    let this = self.as_mut().project();

    match this.state {
      State::Unbatched { task: _ } => {
        match T::queue().with(|queue| {
          queue.enqueue(|state_ptr| {
            let task = {
              let state = std::mem::replace(
                this.state,
                State::Batched(Receiver::new(state_ptr, cx.waker().to_owned())),
              );

              match state {
                State::Unbatched { task } => task,
                _ => unsafe { unreachable_unchecked() },
              }
            };

            let rx = match this.state {
              State::Batched(batched) => {
                addr_of!(*batched)
              }
              _ => unsafe { unreachable_unchecked() },
            };

            (task, rx)
          })
        }) {
          Ok(Some(assignment)) => {
            tokio::task::spawn(async move {
              T::batch_process::<N>(assignment).await;
            });
          }
          Err(QueueFull) => {
            cx.waker().wake_by_ref();
          }
          _ => {}
        }

        Poll::Pending
      }
      State::Batched(_) => {
        let value = match std::mem::replace(this.state, State::Received) {
          State::Batched(rx) => unsafe { (*rx.value.get()).assume_init_read() },
          _ => unsafe { unreachable_unchecked() },
        };

        Poll::Ready(value)
      }
      State::Received => unsafe { unreachable_unchecked() },
    }
  }
}

#[pinned_drop]
impl<T, const N: usize> PinnedDrop for AutoBatchedTask<T, N>
where
  T: LocalQueue<N>,
{
  fn drop(self: Pin<&mut Self>) {
    if let State::Batched(rx) = &self.state {
      let mut state = rx.state().fetch_or(RECEIVER_DROPPED, Ordering::AcqRel);

      if (state & SETTING_VALUE).eq(&0) {
        unsafe {
          drop(rx.take_waker_unchecked());
        }
      }

      // This cannot be safely deallocated until after the value is set
      while state & SETTING_VALUE == SETTING_VALUE {
        yield_now();
        state = rx.state().load(Ordering::Acquire);
      }

      if (state & VALUE_SET).eq(&VALUE_SET) {
        unsafe { (*rx.value.get()).assume_init_drop() }
      }
    }
  }
}
