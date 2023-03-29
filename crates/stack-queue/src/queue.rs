use std::{
  array,
  fmt::Debug,
  future::Future,
  mem::MaybeUninit,
  ops::{BitAnd, Deref},
  pin::Pin,
};
#[cfg(not(loom))]
use std::{
  cell::UnsafeCell,
  sync::atomic::{AtomicUsize, Ordering},
  thread::LocalKey,
};

use async_local::{AsContext, AsyncLocal, Context};
use async_t::async_trait;
use cache_padded::CachePadded;
#[cfg(loom)]
use loom::{
  cell::UnsafeCell,
  sync::atomic::{AtomicUsize, Ordering},
  thread::LocalKey,
};
#[cfg(not(loom))]
use tokio::task::{spawn, yield_now};

use crate::{
  assignment::{CompletionReceipt, PendingAssignment, UnboundedRange},
  helpers::*,
  task::{AutoBatchedTask, Receiver, TaskRef},
  MAX_BUFFER_LEN, MIN_BUFFER_LEN,
};

pub(crate) const PHASE: usize = 1;

#[cfg(target_pointer_width = "64")]
pub(crate) const INDEX_SHIFT: usize = 32;
#[cfg(target_pointer_width = "32")]
pub(crate) const INDEX_SHIFT: usize = 16;

#[doc(hidden)]
pub struct BufferCell<T: Send + Sync + Sized + 'static>(UnsafeCell<MaybeUninit<T>>);

impl<T> Deref for BufferCell<T>
where
  T: Send + Sync + Sized + 'static,
{
  type Target = UnsafeCell<MaybeUninit<T>>;
  fn deref(&self) -> &Self::Target {
    &self.0
  }
}

impl<T> BufferCell<T>
where
  T: Send + Sync + Sized + 'static,
{
  fn new_uninit() -> Self {
    BufferCell(UnsafeCell::new(MaybeUninit::uninit()))
  }
}

unsafe impl<T> Send for BufferCell<T> where T: Send + Sync + Sized + 'static {}
unsafe impl<T> Sync for BufferCell<T> where T: Send + Sync + Sized + 'static {}

/// Auto-batched queue whereby each task resolves to a value
#[async_trait]
pub trait TaskQueue: Send + Sync + Sized + 'static {
  type Task: Send + Sync + Sized + 'static;
  type Value: Send;

  #[allow(clippy::needless_lifetimes)]
  async fn batch_process<const N: usize>(
    assignment: PendingAssignment<'async_trait, Self, N>,
  ) -> CompletionReceipt<Self>;

  fn auto_batch<const N: usize>(task: Self::Task) -> AutoBatchedTask<Self, N>
  where
    Self: LocalQueue<N, BufferCell = TaskRef<Self>>,
  {
    AutoBatchedTask::new(task)
  }
}

/// Fire and forget auto-batched queue
///
/// # Example
///
/// ```rust
/// struct EchoQueue;
///
/// #[local_queue]
/// impl BackgroundQueue for EchoQueue {
///   type Task = (usize, oneshot::Sender<usize>);
///
///   async fn batch_process<const N: usize>(tasks: UnboundedRange<'async_trait, Self::Task, N>) {///
///     for (val, tx) in tasks.into_bounded().into_iter() {
///       tx.send(val).ok();
///     }
///   }
/// }
/// ```
#[async_trait]
pub trait BackgroundQueue: Send + Sync + Sized + 'static {
  type Task: Send + Sync + Sized + 'static;

  #[allow(clippy::needless_lifetimes)]
  async fn batch_process<const N: usize>(tasks: UnboundedRange<'async_trait, Self::Task, N>);

  #[cfg(not(loom))]
  fn auto_batch<const N: usize>(task: Self::Task)
  where
    Self: LocalQueue<N, BufferCell = BufferCell<Self::Task>>,
  {
    StackQueue::background_process::<Self>(task);
  }
}

/// Auto-batched queue whereby tasks are reduced or collected
///
/// # Example
///
/// ```rust
/// struct Accumulator;
///
/// #[local_queue]
/// impl BatchReducer for Accumulator {
///   type Task = usize;
/// }
///
/// let sum = Accumulator::batch_reduce(9000, |range| {
///   Box::pin(async move { range.into_bounded().iter().sum::<usize>() })
/// }).await;
/// ```
pub trait BatchReducer: Send + Sync + Sized + 'static {
  type Task: Send + Sync + Sized + 'static;
}

/// Extension trait for types that impl [`BatchReducer`]
#[async_trait]
pub trait ReducerExt: Send + Sync + Sized + 'static {
  type Task: Send + Sync + Sized + 'static;

  /// Enqueue and auto-batch task, using reducer fn once per batch
  async fn batch_reduce<const N: usize, F, R>(task: Self::Task, f: F) -> Option<R>
  where
    Self: LocalQueue<N, BufferCell = BufferCell<Self::Task>>,
    F: for<'a> FnOnce(
        UnboundedRange<'a, Self::Task, N>,
      ) -> Pin<Box<dyn Future<Output = R> + Send + 'a>>
      + Send;

  /// Collect into batches
  async fn batch_collect<const N: usize>(task: Self::Task) -> Option<Vec<Self::Task>>
  where
    Self: LocalQueue<N, BufferCell = BufferCell<Self::Task>>;
}

#[cfg(not(loom))]
#[async_trait]
impl<T> ReducerExt for T
where
  T: BatchReducer,
{
  type Task = <T as BatchReducer>::Task;

  async fn batch_reduce<const N: usize, F, R>(mut task: Self::Task, f: F) -> Option<R>
  where
    Self: LocalQueue<N, BufferCell = BufferCell<Self::Task>>,
    F: for<'a> FnOnce(
        UnboundedRange<'a, Self::Task, N>,
      ) -> Pin<Box<dyn Future<Output = R> + Send + 'a>>
      + Send,
  {
    loop {
      match <Self as stack_queue::LocalQueue<N>>::queue().with(|queue| unsafe { queue.push(task) })
      {
        Ok(Some(batch)) => {
          tokio::task::yield_now().await;
          break Some(f(batch).await);
        }
        Ok(None) => break None,
        Err(value) => {
          task = value;
          tokio::task::yield_now().await;
        }
      }
    }
  }

  async fn batch_collect<const N: usize>(mut task: Self::Task) -> Option<Vec<Self::Task>>
  where
    Self: LocalQueue<N, BufferCell = BufferCell<Self::Task>>,
  {
    loop {
      match <Self as LocalQueue<N>>::queue().with(|queue| unsafe { queue.push(task) }) {
        Ok(Some(batch)) => {
          tokio::task::yield_now().await;
          break Some(batch.into_bounded().to_vec());
        }
        Ok(None) => break None,
        Err(value) => {
          task = value;
          tokio::task::yield_now().await;
        }
      }
    }
  }
}

/// Thread local context for enqueuing tasks on [`StackQueue`]
pub trait LocalQueue<const N: usize> {
  type BufferCell: Send + Sync + Sized + 'static;

  fn queue() -> &'static LocalKey<StackQueue<Self::BufferCell, N>>;
}

#[doc(hidden)]
pub struct Inner<T: Sync + Sized + 'static, const N: usize = 512> {
  pub(crate) slot: CachePadded<AtomicUsize>,
  pub(crate) occupancy: CachePadded<AtomicUsize>,
  pub(crate) buffer: [T; N],
}

impl<T, const N: usize> Default for Inner<BufferCell<T>, N>
where
  T: Send + Sync + Sized + 'static,
{
  fn default() -> Self {
    let buffer = array::from_fn(|_| BufferCell::new_uninit());

    Inner {
      slot: CachePadded::new(AtomicUsize::new(0)),
      occupancy: CachePadded::new(AtomicUsize::new(0)),
      buffer,
    }
  }
}

impl<T, const N: usize> Default for Inner<TaskRef<T>, N>
where
  T: TaskQueue,
{
  fn default() -> Self {
    let buffer = array::from_fn(|_| TaskRef::new_uninit());

    Inner {
      slot: CachePadded::new(AtomicUsize::new(0)),
      occupancy: CachePadded::new(AtomicUsize::new(0)),
      buffer,
    }
  }
}

impl<T, const N: usize> Inner<BufferCell<T>, N>
where
  T: Send + Sync + Sized + 'static,
{
  #[cfg(not(loom))]
  #[inline(always)]
  pub(crate) unsafe fn with_buffer_cell<F, R>(&self, f: F, index: usize) -> R
  where
    F: FnOnce(*mut MaybeUninit<T>) -> R,
  {
    let cell = self.buffer.get_unchecked(index);
    f(cell.get())
  }

  #[cfg(loom)]
  #[inline(always)]
  pub(crate) unsafe fn with_buffer_cell<F, R>(&self, f: F, index: usize) -> R
  where
    F: FnOnce(*mut MaybeUninit<T>) -> R,
  {
    let cell = self.buffer.get_unchecked(index);
    cell.get_mut().with(f)
  }
}

unsafe impl<T, const N: usize> Sync for Inner<T, N> where T: Sync + Sized + 'static {}

#[derive(Debug)]
pub(crate) struct QueueFull;

/// Task queue designed for facilitating heapless auto-batching of tasks
#[derive(AsContext)]
pub struct StackQueue<T: Sync + Sized + 'static, const N: usize = 512> {
  slot: CachePadded<UnsafeCell<usize>>,
  occupancy: CachePadded<UnsafeCell<usize>>,
  inner: Context<Inner<T, N>>,
}

impl<T, const N: usize> Default for StackQueue<T, N>
where
  T: Sync + Sized + 'static,
  Inner<T, N>: Default,
{
  fn default() -> Self {
    assert_eq!(
      N,
      N.next_power_of_two(),
      "StackQueue buffer size must be power of 2"
    );
    assert!(N >= MIN_BUFFER_LEN);
    assert!(N <= MAX_BUFFER_LEN);

    StackQueue {
      slot: CachePadded::new(UnsafeCell::new(PHASE)),
      occupancy: CachePadded::new(UnsafeCell::new(0)),
      inner: Context::new(Inner::default()),
    }
  }
}

impl<T, const N: usize> StackQueue<T, N>
where
  T: Sync + Sized + 'static,
{
  #[cfg(not(loom))]
  #[inline(always)]
  unsafe fn with_slot<F, R>(&self, f: F) -> R
  where
    F: FnOnce(*const usize) -> R,
  {
    f(self.slot.get())
  }

  #[cfg(loom)]
  #[inline(always)]
  unsafe fn with_slot<F, R>(&self, f: F) -> R
  where
    F: FnOnce(*const usize) -> R,
  {
    self.slot.get().with(f)
  }

  #[cfg(not(loom))]
  #[inline(always)]
  unsafe fn with_slot_mut<F, R>(&self, f: F) -> R
  where
    F: FnOnce(*mut usize) -> R,
  {
    f(self.slot.get())
  }

  #[cfg(loom)]
  #[inline(always)]
  unsafe fn with_slot_mut<F, R>(&self, f: F) -> R
  where
    F: FnOnce(*mut usize) -> R,
  {
    self.slot.get_mut().with(f)
  }

  #[cfg(not(loom))]
  #[inline(always)]
  unsafe fn with_occupancy<F, R>(&self, f: F) -> R
  where
    F: FnOnce(*const usize) -> R,
  {
    f(self.occupancy.get())
  }

  #[cfg(loom)]
  #[inline(always)]
  unsafe fn with_occupancy<F, R>(&self, f: F) -> R
  where
    F: FnOnce(*const usize) -> R,
  {
    self.occupancy.get().with(f)
  }

  #[cfg(not(loom))]
  #[inline(always)]
  unsafe fn with_occupancy_mut<F, R>(&self, f: F) -> R
  where
    F: FnOnce(*mut usize) -> R,
  {
    f(self.occupancy.get())
  }

  #[cfg(loom)]
  #[inline(always)]
  unsafe fn with_occupancy_mut<F, R>(&self, f: F) -> R
  where
    F: FnOnce(*mut usize) -> R,
  {
    self.occupancy.get_mut().with(f)
  }

  #[inline(always)]
  fn current_write_index(&self) -> usize {
    // This algorithm can utilize an UnsafeCell for the index counter because where the current task
    // is written is independent of when a phase change would result in a new task batch owning a
    // new range of the buffer; only ownership is determined by atomic synchronization, not location

    unsafe { self.with_slot(|val| slot_index::<N>(*val)) }
  }

  fn check_regional_occupancy(&self, index: usize) -> Result<(), QueueFull> {
    let region_mask = region_mask::<N>(index);

    // If this is out of sync, then the region could be incorrectly marked as full, but never
    // incorrectly marked as free, and so this optimization allows us to avoid the overhead of an
    // atomic call so long as regions are cleared within a full cycle

    let regional_occupancy =
      unsafe { self.with_occupancy(|occupancy| (*occupancy).bitand(region_mask)) };

    if regional_occupancy.eq(&0) {
      return Ok(());
    }

    // Usually this slow path won't occur because occupancy syncs when new batches are created
    let occupancy = self.inner.occupancy.load(Ordering::Acquire);
    let regional_occupancy = occupancy.bitand(region_mask);

    unsafe {
      self.with_occupancy_mut(move |val| *val = occupancy);
    }

    if regional_occupancy.eq(&0) {
      Ok(())
    } else {
      Err(QueueFull)
    }
  }

  fn occupy_region(&self, index: usize) {
    // Add one relative to the the current region. In the unlikely event of an overflow, the next
    // region checkpoint will result in QueueFull until fewer than 256 task batches exist.
    let shifted_add = one_shifted::<N>(index);

    let occupancy = self
      .inner
      .occupancy
      .fetch_add(shifted_add, Ordering::AcqRel)
      .wrapping_add(shifted_add);

    unsafe {
      self.with_occupancy_mut(move |val| *val = occupancy);
    }
  }

  #[inline(always)]
  unsafe fn replace_slot(&self, slot: usize) -> usize {
    self.with_slot_mut(move |val| std::mem::replace(&mut *val, slot))
  }
}

impl<T, const N: usize> StackQueue<TaskRef<T>, N>
where
  T: TaskQueue,
{
  unsafe fn write_with<F>(&self, index: &usize, write_with: F)
  where
    F: FnOnce(*const AtomicUsize) -> (T::Task, *const Receiver<T>),
  {
    let task_ref = self.inner.buffer.get_unchecked(*index);
    let (task, rx) = write_with(task_ref.state_ptr());
    task_ref.set_task(task, rx);
  }

  pub(crate) fn enqueue<'a, F>(
    &self,
    write_with: F,
  ) -> Result<Option<PendingAssignment<'a, T, N>>, QueueFull>
  where
    T: LocalQueue<N, BufferCell = TaskRef<T>>,
    F: FnOnce(*const AtomicUsize) -> (T::Task, *const Receiver<T>),
  {
    let write_index = self.current_write_index();

    // Regions sizes are always a power of 2, and so this acts as an optimized modulus operation
    if write_index.bitand(region_size::<N>() - 1).eq(&0) {
      self.check_regional_occupancy(write_index)?;
    }

    unsafe {
      self.write_with(&write_index, write_with);
    }

    let base_slot = self
      .inner
      .slot
      .fetch_add(1 << INDEX_SHIFT, Ordering::Relaxed);

    let prev_slot = unsafe { self.replace_slot(base_slot.wrapping_add(1 << INDEX_SHIFT)) };

    if ((base_slot ^ prev_slot) & PHASE).eq(&0) {
      Ok(None)
    } else {
      self.occupy_region(write_index);

      let queue = unsafe { T::queue().guarded_ref() };

      Ok(Some(PendingAssignment::new(base_slot, queue)))
    }
  }
}

impl<T, const N: usize> StackQueue<BufferCell<T>, N>
where
  T: Send + Sync + Sized + 'static,
{
  pub(crate) unsafe fn push<'a>(&self, task: T) -> Result<Option<UnboundedRange<'a, T, N>>, T> {
    let write_index = self.current_write_index();

    // Regions sizes are always a power of 2, and so this acts as an optimized modulus operation
    if write_index.bitand(region_size::<N>() - 1).eq(&0)
      && self.check_regional_occupancy(write_index).is_err()
    {
      return Err(task);
    }

    unsafe {
      self
        .inner
        .with_buffer_cell(|cell| cell.write(MaybeUninit::new(task)), write_index);
    }

    let base_slot = self
      .inner
      .slot
      .fetch_add(1 << INDEX_SHIFT, Ordering::Relaxed);

    let prev_slot = unsafe { self.replace_slot(base_slot.wrapping_add(1 << INDEX_SHIFT)) };

    if ((base_slot ^ prev_slot) & PHASE).eq(&0) {
      Ok(None)
    } else {
      self.occupy_region(write_index);

      let queue = unsafe { self.inner.guarded_ref() };

      Ok(Some(UnboundedRange::new(base_slot, queue)))
    }
  }

  #[cfg(not(loom))]
  fn background_process<Q>(task: T)
  where
    Q: BackgroundQueue<Task = T> + LocalQueue<N, BufferCell = BufferCell<T>>,
  {
    Q::queue().with(|queue| match unsafe { queue.push(task) } {
      Ok(Some(assignment)) => {
        spawn(async move {
          Q::batch_process::<N>(assignment).await;
        });
      }
      Ok(None) => {}
      Err(mut task) => {
        spawn(async move {
          loop {
            task = match Q::queue().with(|queue| unsafe { queue.push(task) }) {
              Ok(Some(assignment)) => {
                Q::batch_process::<N>(assignment).await;
                return;
              }
              Ok(None) => {
                return;
              }
              Err(task) => {
                yield_now().await;
                task
              }
            };
          }
        });
      }
    });
  }
}
#[cfg(test)]
mod test {
  #[cfg(not(loom))]
  use std::{thread, time::Duration};

  #[cfg(not(loom))]
  use futures::{stream::FuturesUnordered, StreamExt};
  #[cfg(not(loom))]
  use tokio::{
    sync::{oneshot, Barrier},
    task::{spawn, yield_now},
  };

  use crate::{
    assignment::{CompletionReceipt, PendingAssignment},
    local_queue, TaskQueue,
  };
  #[cfg(not(loom))]
  use crate::{queue::UnboundedRange, BackgroundQueue};

  struct EchoQueue;

  #[local_queue(buffer_size = 64)]
  impl TaskQueue for EchoQueue {
    type Task = usize;
    type Value = usize;

    async fn batch_process<const N: usize>(
      batch: PendingAssignment<'async_trait, Self, N>,
    ) -> CompletionReceipt<Self> {
      batch.into_assignment().map(|val| val)
    }
  }

  #[cfg(not(loom))]
  #[cfg_attr(not(loom), tokio::test(crate = "async_local"))]

  async fn it_process_tasks() {
    use rand::{distributions::Standard, prelude::*};
    let mut rng = rand::thread_rng();

    let seed: Vec<usize> = (&mut rng).sample_iter(Standard).take(65536).collect();

    let expected_total: u128 = seed
      .iter()
      .fold(0, |total, val| total.wrapping_add(*val as u128));

    let mut seed = seed.into_iter();
    let mut total: u128 = 0;

    while seed.len().gt(&0) {
      let mut tasks: FuturesUnordered<_> = (&mut seed)
        .take(rng.gen_range(0..4096))
        .map(EchoQueue::auto_batch)
        .collect();

      while let Some(val) = tasks.next().await {
        total = total.wrapping_add(val as u128);
      }
    }

    assert_eq!(total, expected_total);
  }

  #[cfg(not(loom))]
  #[cfg_attr(not(loom), tokio::test(crate = "async_local", flavor = "multi_thread"))]

  async fn it_cycles() {
    for i in 0..512 {
      EchoQueue::auto_batch(i).await;
    }
  }

  #[cfg(not(loom))]
  struct SlowQueue;

  #[cfg(not(loom))]
  #[local_queue(buffer_size = 64)]
  impl TaskQueue for SlowQueue {
    type Task = usize;
    type Value = usize;

    async fn batch_process<const N: usize>(
      batch: PendingAssignment<'async_trait, Self, N>,
    ) -> CompletionReceipt<Self> {
      batch
        .with_blocking(|batch| {
          let assignment = batch.into_assignment();
          thread::sleep(Duration::from_millis(50));
          assignment.map(|task| task)
        })
        .await
    }
  }

  #[cfg(not(loom))]
  #[cfg_attr(not(loom), tokio::test(crate = "async_local", flavor = "multi_thread"))]

  async fn it_has_drop_safety() {
    let handle = spawn(async {
      SlowQueue::auto_batch(0).await;
    });

    yield_now().await;

    handle.abort();
  }

  #[cfg(not(loom))]
  struct YieldQueue;

  #[cfg(not(loom))]
  #[local_queue(buffer_size = 64)]
  impl TaskQueue for YieldQueue {
    type Task = usize;
    type Value = usize;

    async fn batch_process<const N: usize>(
      batch: PendingAssignment<'async_trait, Self, N>,
    ) -> CompletionReceipt<Self> {
      let assignment = batch.into_assignment();

      yield_now().await;

      assignment.map(|val| val)
    }
  }

  #[cfg(not(loom))]
  #[cfg_attr(not(loom), tokio::test(crate = "async_local", flavor = "multi_thread"))]

  async fn it_negotiates_receiver_drop() {
    use std::sync::Arc;

    let tasks: FuturesUnordered<_> = (0..256)
      .map(|i| async move {
        let barrier = Arc::new(Barrier::new(2));

        let task_barrier = barrier.clone();

        let handle = tokio::task::spawn(async move {
          task_barrier.wait().await;
          YieldQueue::auto_batch(i).await;
        });

        barrier.wait().await;
        yield_now().await;

        handle.abort()
      })
      .collect();

    tasks.collect::<Vec<_>>().await;
  }

  #[cfg(loom)]
  #[test]
  fn stack_queue_drops() {
    use crate::{BufferCell, StackQueue};

    loom::model(|| {
      let queue: StackQueue<BufferCell<usize>, 64> = StackQueue::default();
      drop(queue);
    });
  }

  #[cfg(loom)]
  #[test]
  fn the_occupancy_model_synchronizes() {
    use loom::{
      sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
      },
      thread,
    };

    loom::model(|| {
      let occupancy = Arc::new(AtomicUsize::new(0));

      assert_eq!(occupancy.fetch_add(1, Ordering::AcqRel), 0);

      {
        let occupancy = occupancy.clone();
        thread::spawn(move || {
          occupancy.fetch_sub(1, Ordering::Release);
        })
      }
      .join()
      .unwrap();

      assert_eq!(occupancy.load(Ordering::Acquire), 0);
    });
  }

  #[cfg(loom)]
  #[test]
  fn it_manages_occupancy() {
    use crate::{queue::UnboundedRange, BufferCell, StackQueue};

    let expected_total = (0..256).into_iter().sum::<usize>();

    loom::model(move || {
      let queue: StackQueue<BufferCell<usize>, 64> = StackQueue::default();
      let mut batch: Option<UnboundedRange<usize, 64>> = None;
      let mut i = 0;
      let mut total = 0;

      while i < 256 {
        match unsafe { queue.push(i) } {
          Ok(Some(unbounded_batch)) => {
            batch = Some(unbounded_batch);
            i += 1;
          }
          Ok(None) => {
            i += 1;
          }
          Err(_) => {
            if let Some(batch) = batch.take() {
              total += batch.into_bounded().into_iter().sum::<usize>();
            } else {
              panic!("queue full despite buffer unoccupied");
            }
            continue;
          }
        }
      }

      if let Some(batch) = batch.take() {
        total += batch.into_bounded().into_iter().sum::<usize>();
      }

      assert_eq!(total, expected_total);
    });
  }

  #[cfg(loom)]
  #[test]
  fn it_negotiates_receiver_drop() {
    use std::{hint::unreachable_unchecked, ptr::addr_of};

    use futures::pin_mut;
    use futures_test::task::noop_waker;
    use loom::sync::{Arc, Condvar, Mutex};

    use crate::task::{AutoBatchedTask, Receiver, State, TaskRef};

    loom::model(|| {
      let task: Arc<TaskRef<EchoQueue>> = Arc::new(TaskRef::new_uninit());
      let barrier = Arc::new((Mutex::new(false), Condvar::new()));

      let resolver_handle = {
        let task = task.clone();
        let barrier = barrier.clone();

        loom::thread::spawn(move || {
          let (lock, cvar) = &*barrier;
          let mut task_initialized = lock.lock().unwrap();
          while !*task_initialized {
            task_initialized = cvar.wait(task_initialized).unwrap();
          }

          unsafe {
            task.resolve_unchecked(9001);
          }
        })
      };

      let receiver_handle = {
        loom::thread::spawn(move || {
          let waker = noop_waker();

          let rx: Receiver<EchoQueue> = Receiver::new(task.state_ptr(), waker);

          let auto_batched_task: AutoBatchedTask<EchoQueue, 256> = AutoBatchedTask {
            state: State::Batched(rx),
          };

          pin_mut!(auto_batched_task);

          let rx = match &auto_batched_task.state {
            State::Batched(rx) => {
              addr_of!(*rx)
            }
            _ => unsafe { unreachable_unchecked() },
          };

          unsafe { task.set_task(9001, rx) };

          let (lock, cvar) = &*barrier;
          let mut task_initialized = lock.lock().unwrap();
          *task_initialized = true;
          cvar.notify_one();

          #[allow(clippy::drop_non_drop)]
          drop(auto_batched_task);
        })
      };

      resolver_handle.join().unwrap();
      receiver_handle.join().unwrap();
    });
  }

  #[cfg(not(loom))]
  struct EchoBackgroundQueue;

  #[cfg(not(loom))]
  #[local_queue]
  impl BackgroundQueue for EchoBackgroundQueue {
    type Task = (usize, oneshot::Sender<usize>);

    async fn batch_process<const N: usize>(tasks: UnboundedRange<'async_trait, Self::Task, N>) {
      let tasks = tasks.into_bounded().into_iter();

      for (val, tx) in tasks {
        tx.send(val).ok();
      }
    }
  }

  #[cfg(not(loom))]
  #[cfg_attr(not(loom), tokio::test(crate = "async_local", flavor = "multi_thread"))]

  async fn it_process_background_tasks() {
    #[allow(clippy::needless_collect)]
    let receivers: Vec<_> = (0..10_usize)
      .map(|i| {
        let (tx, rx) = oneshot::channel::<usize>();
        EchoBackgroundQueue::auto_batch((i, tx));
        rx
      })
      .collect();

    for (i, rx) in receivers.into_iter().enumerate() {
      assert_eq!(rx.await, Ok(i));
    }
  }

  #[cfg(not(loom))]
  #[cfg_attr(not(loom), tokio::test(crate = "async_local", flavor = "multi_thread"))]
  async fn it_batch_reduces() {
    use crate::ReducerExt;

    struct Accumulator;

    #[local_queue]
    impl BatchReducer for Accumulator {
      type Task = usize;
    }

    let tasks: FuturesUnordered<_> = (0..10000)
      .map(|i| {
        Accumulator::batch_reduce(i, |range| {
          Box::pin(async move { range.into_bounded().iter().sum::<usize>() })
        })
      })
      .collect();

    let total = tasks
      .fold(0_usize, |total, value| async move {
        total + value.unwrap_or_default()
      })
      .await;

    assert_eq!(total, (0..10000).sum());
  }

  #[cfg(not(loom))]
  #[cfg_attr(not(loom), tokio::test(crate = "async_local", flavor = "multi_thread"))]
  async fn it_batch_collects() {
    use crate::ReducerExt;

    struct Accumulator;

    #[local_queue]
    impl BatchReducer for Accumulator {
      type Task = usize;
    }

    let mut tasks: FuturesUnordered<_> = (0..10000).map(Accumulator::batch_collect).collect();

    let mut total = 0;

    while let Some(batch) = tasks.next().await {
      total += batch.map_or(0, |batch| batch.into_iter().sum::<usize>());
    }

    assert_eq!(total, (0..10000).sum());
  }
}
