#[cfg(not(loom))]
use std::{
  cell::UnsafeCell,
  fmt,
  fmt::Debug,
  future::Future,
  hint::unreachable_unchecked,
  mem,
  ops::Deref,
  ptr::addr_of,
  sync::atomic::{AtomicUsize, Ordering},
  task::{Context, Poll},
  thread::yield_now,
};
use std::{marker::PhantomPinned, mem::MaybeUninit, pin::Pin, task::Waker};

#[cfg(feature = "diesel-associations")]
use diesel::associations::BelongsTo;
#[cfg(loom)]
use loom::{
  cell::UnsafeCell,
  sync::atomic::{AtomicUsize, Ordering},
  thread::yield_now,
};
use pin_project::{pin_project, pinned_drop};
#[cfg(feature = "redis-args")]
use redis::{RedisWrite, ToRedisArgs};
#[cfg(not(loom))]
use tokio::task::spawn;

use crate::{
  assignment::{BufferIter, UnboundedRange},
  queue::{LocalQueue, TaskQueue},
  BatchReducer,
};
#[cfg(not(loom))]
use crate::{queue::QueueFull, BufferCell};

pub(crate) const SETTING_VALUE: usize = 1 << 0;
pub(crate) const VALUE_SET: usize = 1 << 1;
pub(crate) const RECEIVER_DROPPED: usize = 1 << 2;

/// A pointer to the pinned receiver of an enqueued [`BatchedTask`]
pub struct TaskRef<T: TaskQueue> {
  state: UnsafeCell<AtomicUsize>,
  rx: UnsafeCell<MaybeUninit<*const Receiver<T>>>,
  task: UnsafeCell<MaybeUninit<T::Task>>,
}

#[cfg(not(loom))]
impl<T> Deref for TaskRef<T>
where
  T: TaskQueue,
{
  type Target = T::Task;
  fn deref(&self) -> &Self::Target {
    self.task()
  }
}

#[cfg(not(loom))]
impl<T> PartialEq for TaskRef<T>
where
  T: TaskQueue,
  T::Task: PartialEq,
{
  fn eq(&self, other: &Self) -> bool {
    self.task().eq(other.task())
  }
}

#[cfg(not(loom))]
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
  #[cfg(not(loom))]
  #[inline(always)]
  pub(crate) fn with_state<F, R>(&self, f: F) -> R
  where
    F: FnOnce(*const AtomicUsize) -> R,
  {
    f(self.state.get())
  }

  #[cfg(loom)]
  #[inline(always)]
  pub(crate) fn with_state<F, R>(&self, f: F) -> R
  where
    F: FnOnce(*const AtomicUsize) -> R,
  {
    self.state.get().with(f)
  }

  #[inline(always)]
  pub(crate) fn state_ptr(&self) -> *const AtomicUsize {
    self.with_state(std::convert::identity)
  }

  #[cfg(not(loom))]
  #[inline(always)]
  unsafe fn with_state_mut<F, R>(&self, f: F) -> R
  where
    F: FnOnce(*mut AtomicUsize) -> R,
  {
    f(self.state.get())
  }

  #[cfg(loom)]
  #[inline(always)]
  unsafe fn with_state_mut<F, R>(&self, f: F) -> R
  where
    F: FnOnce(*mut AtomicUsize) -> R,
  {
    self.state.get_mut().with(f)
  }

  #[cfg(not(loom))]
  #[inline(always)]
  unsafe fn reset_state(&self) {
    self.with_state_mut(|val| *(*val).get_mut() = 0);
  }

  #[cfg(loom)]
  #[inline(always)]
  unsafe fn reset_state(&self) {
    self.with_state_mut(|val| (*val).with_mut(|val| *val = 0));
  }

  #[cfg(not(loom))]
  #[inline(always)]
  pub(crate) fn rx(&self) -> &Receiver<T> {
    unsafe { &**(*self.rx.get()).assume_init_ref() }
  }

  #[cfg(loom)]
  #[inline(always)]
  pub(crate) fn rx(&self) -> &Receiver<T> {
    unsafe { &**(*self.rx.get().deref()).assume_init_ref() }
  }

  #[cfg(not(loom))]
  #[inline(always)]
  unsafe fn with_rx_mut<F, R>(&self, f: F) -> R
  where
    F: FnOnce(*mut MaybeUninit<*const Receiver<T>>) -> R,
  {
    f(self.rx.get())
  }

  #[cfg(loom)]
  #[inline(always)]
  unsafe fn with_rx_mut<F, R>(&self, f: F) -> R
  where
    F: FnOnce(*mut MaybeUninit<*const Receiver<T>>) -> R,
  {
    self.rx.get_mut().with(f)
  }

  #[cfg(not(loom))]
  #[inline(always)]
  pub fn task(&self) -> &T::Task {
    unsafe { (*self.task.get()).assume_init_ref() }
  }

  #[cfg(not(loom))]
  #[inline(always)]
  unsafe fn with_task_mut<F, R>(&self, f: F) -> R
  where
    F: FnOnce(*mut MaybeUninit<T::Task>) -> R,
  {
    f(self.task.get())
  }

  #[cfg(loom)]
  #[inline(always)]
  unsafe fn with_task_mut<F, R>(&self, f: F) -> R
  where
    F: FnOnce(*mut MaybeUninit<T::Task>) -> R,
  {
    self.task.get_mut().with(f)
  }

  pub(crate) unsafe fn set_task(&self, task: T::Task, rx: *const Receiver<T>) {
    self.reset_state();
    self.with_rx_mut(|val| val.write(MaybeUninit::new(rx)));
    self.with_task_mut(|val| val.write(MaybeUninit::new(task)));
  }

  #[inline(always)]
  pub(crate) unsafe fn take_task_unchecked(&self) -> T::Task {
    self.with_task_mut(|val| std::mem::replace(&mut *val, MaybeUninit::uninit()).assume_init())
  }

  /// Set value in receiver and wake if the receiver isn't already dropped. This takes &self because
  /// [`TaskRef`] by design is never dropped
  pub(crate) unsafe fn resolve_unchecked(&self, value: T::Value) {
    let state = self.with_state(|val| (*val).fetch_or(SETTING_VALUE, Ordering::Release));

    if (state & RECEIVER_DROPPED).eq(&0) {
      let rx = self.rx();
      rx.with_value_mut(|val| {
        val.write(MaybeUninit::new(value));
      });
      rx.waker.wake_by_ref();
      self.with_state(|val| {
        (*val).fetch_xor(SETTING_VALUE | VALUE_SET, Ordering::Release);
      });
    }
  }
}

#[cfg(not(loom))]
impl<T> Debug for TaskRef<T>
where
  T: TaskQueue,
  <T as TaskQueue>::Task: Debug,
{
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "{:?}", self.task())
  }
}

unsafe impl<T> Send for TaskRef<T> where T: TaskQueue {}
unsafe impl<T> Sync for TaskRef<T> where T: TaskQueue {}

#[cfg_attr(docsrs, doc(cfg(feature = "diesel-associations")))]
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

#[cfg_attr(docsrs, doc(cfg(feature = "redis-args")))]
#[cfg(feature = "redis-args")]
impl<T> ToRedisArgs for TaskRef<T>
where
  T: TaskQueue,
  T::Task: ToRedisArgs,
{
  fn write_redis_args<W>(&self, out: &mut W)
  where
    W: ?Sized + RedisWrite,
  {
    self.task().write_redis_args(out)
  }
}

#[pin_project]
pub(crate) struct Receiver<T: TaskQueue> {
  state: *const AtomicUsize,
  value: UnsafeCell<MaybeUninit<T::Value>>,
  waker: Waker,
  pin: PhantomPinned,
}

impl<T> Receiver<T>
where
  T: TaskQueue,
{
  pub(crate) fn new(state: *const AtomicUsize, waker: Waker) -> Self {
    Receiver {
      state,
      value: UnsafeCell::new(MaybeUninit::uninit()),
      waker,
      pin: PhantomPinned,
    }
  }

  #[inline(always)]
  fn state(&self) -> &AtomicUsize {
    unsafe { &*self.state }
  }

  #[cfg(not(loom))]
  #[inline(always)]
  unsafe fn with_value_mut<F, R>(&self, f: F) -> R
  where
    F: FnOnce(*mut MaybeUninit<T::Value>) -> R,
  {
    f(self.value.get())
  }

  #[cfg(loom)]
  #[inline(always)]
  unsafe fn with_value_mut<F, R>(&self, f: F) -> R
  where
    F: FnOnce(*mut MaybeUninit<T::Value>) -> R,
  {
    self.value.get_mut().with(f)
  }
}

unsafe impl<T> Send for Receiver<T> where T: TaskQueue {}

#[pin_project(project = StateProj)]
pub(crate) enum State<T: TaskQueue> {
  Unbatched { task: T::Task },
  Batched(#[pin] Receiver<T>),
  Received,
}

/// An automatically batched task
#[pin_project(project = AutoBatchProj, PinnedDrop)]
pub struct BatchedTask<T: TaskQueue, const N: usize = 512> {
  pub(crate) state: State<T>,
}

impl<T, const N: usize> BatchedTask<T, N>
where
  T: TaskQueue,
  T: LocalQueue<N, BufferCell = TaskRef<T>>,
{
  /// Create a new auto batched task
  pub fn new(task: T::Task) -> Self {
    BatchedTask {
      state: State::Unbatched { task },
    }
  }
}

#[cfg(not(loom))]
impl<T, const N: usize> Future for BatchedTask<T, N>
where
  T: TaskQueue,
  T: LocalQueue<N, BufferCell = TaskRef<T>>,
{
  type Output = T::Value;

  fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
    let this = self.as_mut().project();

    match this.state {
      State::Unbatched { task: _ } => {
        match T::queue().with(|queue| {
          queue.enqueue(|state_ptr| {
            let task = {
              let state = mem::replace(
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
            spawn(async move {
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
        let value = match mem::replace(this.state, State::Received) {
          State::Batched(rx) => unsafe { rx.with_value_mut(|val| (*val).assume_init_read()) },
          _ => unsafe { unreachable_unchecked() },
        };

        Poll::Ready(value)
      }
      // If already received, block forever. See https://doc.rust-lang.org/std/future/trait.Future.html#panics
      State::Received => Poll::Pending,
    }
  }
}

#[pinned_drop]
impl<T, const N: usize> PinnedDrop for BatchedTask<T, N>
where
  T: TaskQueue,
{
  fn drop(self: Pin<&mut Self>) {
    if let State::Batched(rx) = &self.state {
      let mut state = rx.state().fetch_or(RECEIVER_DROPPED, Ordering::AcqRel);

      // This cannot be safely deallocated until after the value is set
      while state & SETTING_VALUE == SETTING_VALUE {
        yield_now();
        state = rx.state().load(Ordering::Acquire);
      }

      if (state & VALUE_SET).eq(&VALUE_SET) {
        unsafe {
          rx.with_value_mut(|val| {
            (*val).assume_init_drop();
          });
        }
      }
    }
  }
}

#[pin_project(project = ReduceProj)]
pub struct BatchReduce<'a, T, F, R, const N: usize>
where
  T: BatchReducer,
  F: for<'b> FnOnce(BufferIter<'b, T::Task, N>) -> R + Send,
{
  state: ReduceState<'a, T, F, R, N>,
}

impl<'a, T, F, R, const N: usize> BatchReduce<'a, T, F, R, N>
where
  T: BatchReducer,
  F: for<'b> FnOnce(BufferIter<'b, T::Task, N>) -> R + Send,
{
  pub(crate) fn new(task: T::Task, reducer: F) -> Self {
    BatchReduce {
      state: ReduceState::Unbatched { task, reducer },
    }
  }
}

enum ReduceState<'a, T, F, R, const N: usize>
where
  T: BatchReducer,
  F: for<'b> FnOnce(BufferIter<'b, T::Task, N>) -> R + Send,
{
  Unbatched {
    task: T::Task,
    reducer: F,
  },
  Collecting {
    batch: UnboundedRange<'a, T::Task, N>,
    reducer: F,
  },
  Batched,
}

#[cfg(not(loom))]
impl<'a, T, F, R, const N: usize> Future for BatchReduce<'a, T, F, R, N>
where
  T: BatchReducer,
  T: LocalQueue<N, BufferCell = BufferCell<T::Task>>,
  F: for<'b> FnOnce(BufferIter<'b, T::Task, N>) -> R + Send,
{
  type Output = Option<R>;
  fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
    let this = self.as_mut().project();

    match this.state {
      ReduceState::Unbatched {
        task: _,
        reducer: _,
      } => match mem::replace(this.state, ReduceState::Batched) {
        ReduceState::Unbatched { task, reducer } => {
          match T::queue().with(|queue| unsafe { queue.push(task) }) {
            Ok(Some(batch)) => {
              let _ = mem::replace(this.state, ReduceState::Collecting { batch, reducer });
              cx.waker().wake_by_ref();
              Poll::Pending
            }
            Ok(None) => Poll::Ready(None),
            Err(task) => {
              let _ = mem::replace(this.state, ReduceState::Unbatched { task, reducer });
              cx.waker().wake_by_ref();
              Poll::Pending
            }
          }
        }
        _ => unsafe {
          unreachable_unchecked();
        },
      },
      ReduceState::Collecting {
        batch: _,
        reducer: _,
      } => match mem::replace(this.state, ReduceState::Batched) {
        ReduceState::Collecting { batch, reducer } => {
          Poll::Ready(Some(reducer(batch.into_bounded().into_iter())))
        }
        _ => unsafe {
          unreachable_unchecked();
        },
      },
      ReduceState::Batched => Poll::Ready(None),
    }
  }
}

#[pin_project(project = CollectProj)]
pub struct BatchCollect<'a, T, const N: usize>
where
  T: BatchReducer,
{
  state: CollectState<'a, T, N>,
}

impl<'a, T, const N: usize> BatchCollect<'a, T, N>
where
  T: BatchReducer,
{
  pub(crate) fn new(task: T::Task) -> Self {
    BatchCollect {
      state: CollectState::Unbatched { task },
    }
  }
}
enum CollectState<'a, T, const N: usize>
where
  T: BatchReducer,
{
  Unbatched {
    task: T::Task,
  },
  Collecting {
    batch: UnboundedRange<'a, T::Task, N>,
  },
  Batched,
}

#[cfg(not(loom))]
impl<'a, T, const N: usize> Future for BatchCollect<'a, T, N>
where
  T: BatchReducer,
  T: LocalQueue<N, BufferCell = BufferCell<T::Task>>,
{
  type Output = Option<Vec<T::Task>>;
  fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
    let this = self.as_mut().project();

    match this.state {
      CollectState::Unbatched { task: _ } => {
        match mem::replace(this.state, CollectState::Batched) {
          CollectState::Unbatched { task } => {
            match T::queue().with(|queue| unsafe { queue.push(task) }) {
              Ok(Some(batch)) => {
                let _ = mem::replace(this.state, CollectState::Collecting { batch });
                cx.waker().wake_by_ref();
                Poll::Pending
              }
              Ok(None) => Poll::Ready(None),
              Err(task) => {
                let _ = mem::replace(this.state, CollectState::Unbatched { task });
                cx.waker().wake_by_ref();
                Poll::Pending
              }
            }
          }
          _ => unsafe {
            unreachable_unchecked();
          },
        }
      }
      CollectState::Collecting { batch: _ } => {
        match mem::replace(this.state, CollectState::Batched) {
          CollectState::Collecting { batch } => Poll::Ready(Some(batch.into_bounded().to_vec())),
          _ => unsafe {
            unreachable_unchecked();
          },
        }
      }
      CollectState::Batched => Poll::Ready(None),
    }
  }
}
