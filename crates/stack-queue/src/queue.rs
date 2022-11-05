use std::{array, fmt::Debug, mem::MaybeUninit, ops::BitAnd, ptr::addr_of};
#[cfg(not(loom))]
use std::{
  cell::UnsafeCell,
  sync::atomic::{AtomicUsize, Ordering},
  thread::LocalKey,
};

use async_t::async_trait;
use cache_padded::CachePadded;
#[cfg(loom)]
use loom::{
  cell::UnsafeCell,
  sync::atomic::{AtomicUsize, Ordering},
  thread::LocalKey,
};
#[cfg(not(loom))]
use tokio::runtime::Handle;
use tokio::task::yield_now;

use crate::{
  assignment::{CompletionReceipt, PendingAssignment},
  helpers::*,
  slice::UnboundedSlice,
  task::{AutoBatchedTask, Receiver, TaskRef},
  MAX_BUFFER_LEN, MIN_BUFFER_LEN,
};

#[cfg(target_pointer_width = "64")]
pub(crate) const INDEX_SHIFT: usize = 32;
#[cfg(target_pointer_width = "32")]
pub(crate) const INDEX_SHIFT: usize = 16;
#[async_trait]
pub trait TaskQueue: Send + Sync + Sized + 'static {
  type Task: Send;
  type Value: Send;

  #[allow(clippy::needless_lifetimes)]
  async fn batch_process<const N: usize>(
    assignment: PendingAssignment<'async_trait, Self, N>,
  ) -> CompletionReceipt<Self>;
}

#[async_trait]
pub trait SliceQueue: Send + Sync + Sized + 'static {
  type Task: Send;

  #[allow(clippy::needless_lifetimes)]
  async fn batch_process<const N: usize>(tasks: UnboundedSlice<'async_trait, Self, N>);
}

pub trait LocalQueue<const N: usize>: TaskQueue {
  fn queue() -> &'static LocalKey<StackQueue<TaskRef<Self>, N>>;

  fn auto_batch(task: Self::Task) -> AutoBatchedTask<Self, N> {
    AutoBatchedTask::new(task)
  }
}

pub trait LocalSliceQueue<const N: usize>: SliceQueue {
  fn queue() -> &'static LocalKey<StackQueue<UnsafeCell<MaybeUninit<Self::Task>>, N>>;

  fn auto_batch(task: Self::Task) {
    StackQueue::<UnsafeCell<MaybeUninit<Self::Task>>, N>::auto_batch::<Self>(task);
  }
}

pub(crate) struct Inner<T, const N: usize = 2048> {
  pub(crate) slot: CachePadded<AtomicUsize>,
  pub(crate) occupancy: CachePadded<AtomicUsize>,
  pub(crate) buffer: [T; N],
}

impl<T, const N: usize> Default for Inner<UnsafeCell<MaybeUninit<T>>, N> {
  fn default() -> Self {
    let buffer = array::from_fn(|_| UnsafeCell::new(MaybeUninit::uninit()));

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

#[derive(Debug)]
pub(crate) struct QueueFull;

#[doc(hidden)]
/// Task queue designed for facilitating heapless auto-batching of tasks
pub struct StackQueue<T, const N: usize = 2048> {
  slot: CachePadded<UnsafeCell<usize>>,
  occupancy: CachePadded<UnsafeCell<usize>>,
  inner: Inner<T, N>,
}

impl<T, const N: usize> Default for StackQueue<T, N>
where
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
      slot: CachePadded::new(UnsafeCell::new(N << INDEX_SHIFT)),
      occupancy: CachePadded::new(UnsafeCell::new(0)),
      inner: Inner::default(),
    }
  }
}

impl<T, const N: usize> StackQueue<T, N> {
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

    unsafe { self.with_slot(|val| slot_index::<N>(&*val)) }
  }

  fn check_regional_occupancy(&self, index: &usize) -> Result<(), QueueFull> {
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
    let occupancy = self.inner.occupancy.load(Ordering::Relaxed);
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

  fn occupy_region(&self, index: &usize) {
    // Add one relative to the the current region. In the unlikely event of an overflow, the next
    // region checkpoint will result in QueueFull until fewer than 256 task batches exist.
    let shifted_add = one_shifted::<N>(index);

    let occupancy = self
      .inner
      .occupancy
      .fetch_add(shifted_add, Ordering::Relaxed)
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
    F: FnOnce(*const AtomicUsize) -> (T::Task, *const Receiver<T>),
  {
    let write_index = self.current_write_index();

    // Regions sizes are always a power of 2, and so this acts as an optimized modulus operation
    if write_index.bitand(region_size::<N>() - 1).eq(&0) {
      self.check_regional_occupancy(&write_index)?;
    }

    unsafe {
      self.write_with(&write_index, write_with);
    }

    let base_slot = self
      .inner
      .slot
      .fetch_add(1 << INDEX_SHIFT, Ordering::Relaxed);

    let prev_slot = unsafe { self.replace_slot(base_slot.wrapping_add(1 << INDEX_SHIFT)) };

    if write_index.ne(&0) && ((base_slot ^ prev_slot) & active_phase_bit::<N>(&base_slot)).eq(&0) {
      Ok(None)
    } else {
      self.occupy_region(&write_index);

      let queue_ptr = addr_of!(self.inner);

      Ok(Some(PendingAssignment::new(base_slot, queue_ptr)))
    }
  }
}

impl<T, const N: usize> StackQueue<UnsafeCell<MaybeUninit<T>>, N> {
  #[cfg(not(loom))]
  #[inline(always)]
  unsafe fn with_buffer_cell<F, R>(&self, f: F, index: usize) -> R
  where
    F: FnOnce(*mut MaybeUninit<T>) -> R,
  {
    let cell = self.inner.buffer.get_unchecked(index);
    f(cell.get())
  }

  #[cfg(loom)]
  #[inline(always)]
  unsafe fn with_buffer_cell<F, R>(&self, f: F, index: usize) -> R
  where
    F: FnOnce(*mut MaybeUninit<T>) -> R,
  {
    let cell = self.inner.buffer.get_unchecked(index);
    cell.get_mut().with(f)
  }

  pub(crate) fn push<'a, Q>(&self, task: T) -> Result<Option<UnboundedSlice<'a, Q, N>>, T>
  where
    Q: SliceQueue<Task = T>,
  {
    let write_index = self.current_write_index();

    // Regions sizes are always a power of 2, and so this acts as an optimized modulus operation
    if write_index.bitand(region_size::<N>() - 1).eq(&0)
      && self.check_regional_occupancy(&write_index).is_err()
    {
      return Err(task);
    }

    unsafe {
      self.with_buffer_cell(|cell| cell.write(MaybeUninit::new(task)), write_index);
    }

    let base_slot = self
      .inner
      .slot
      .fetch_add(1 << INDEX_SHIFT, Ordering::Relaxed);

    let prev_slot = unsafe { self.replace_slot(base_slot.wrapping_add(1 << INDEX_SHIFT)) };

    if write_index.ne(&0) && ((base_slot ^ prev_slot) & active_phase_bit::<N>(&base_slot)).eq(&0) {
      Ok(None)
    } else {
      self.occupy_region(&write_index);

      let queue_ptr = addr_of!(self.inner);

      Ok(Some(UnboundedSlice::new(base_slot, queue_ptr)))
    }
  }

  pub fn auto_batch<Q>(task: Q::Task)
  where
    Q: LocalSliceQueue<N>,
  {
    Q::queue().with(|queue| match queue.push::<Q>(task) {
      Ok(Some(assignment)) => {
        tokio::task::spawn(async move {
          Q::batch_process::<N>(assignment).await;
        });
      }
      Ok(None) => {}
      Err(mut task) => {
        tokio::task::spawn(async move {
          loop {
            task = match Q::queue().with(|queue| queue.push::<Q>(task)) {
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

// The purpose of this drop is to block deallocation to ensure the validity of references until
// dropped as a way of sequencing shutdowns without introducing undefined behavior. While this
// approach is not ideal, this is at worst an edge-case that can only happen after a shutdown signal
// has occurred, and so this is given preference over other techniques that would come at a
// continuous performance cost. This approach allows us to avoid the necessity for condvar / mutexes
#[cfg(not(loom))]
impl<T, const N: usize> Drop for StackQueue<T, N> {
  fn drop(&mut self) {
    while self.inner.occupancy.load(Ordering::Acquire).ne(&0) {
      match Handle::try_current() {
        Ok(handle) => handle.block_on(tokio::task::yield_now()),
        Err(_) => break,
      }
    }
  }
}
#[cfg(all(test))]
mod test {
  use std::{thread, time::Duration};

  use async_t::async_trait;
  #[cfg(not(loom))]
  use futures::{future::join_all, stream::FuturesUnordered, StreamExt};
  use tokio::{sync::oneshot, task::yield_now};

  use crate::{
    assignment::{CompletionReceipt, PendingAssignment},
    slice::UnboundedSlice,
    LocalQueue, LocalSliceQueue, SliceQueue, TaskQueue,
  };

  #[derive(LocalQueue)]
  #[local_queue(buffer_size = 256)]
  struct EchoQueue;

  #[async_trait]
  impl TaskQueue for EchoQueue {
    type Task = usize;
    type Value = usize;

    async fn batch_process<const N: usize>(
      batch: PendingAssignment<'async_trait, Self, N>,
    ) -> CompletionReceipt<Self> {
      yield_now().await;
      batch.into_assignment().map(|val| val)
    }
  }

  #[cfg(not(loom))]
  #[tokio::test(flavor = "multi_thread")]

  async fn it_process_tasks() {
    let batch: Vec<usize> = join_all((0..100).map(EchoQueue::auto_batch)).await;

    assert_eq!(batch, (0..100).collect::<Vec<usize>>());
  }

  #[cfg(not(loom))]
  #[tokio::test(flavor = "multi_thread")]

  async fn it_cycles() {
    for i in 0..512 {
      EchoQueue::auto_batch(i).await;
    }
  }

  #[derive(LocalQueue)]
  struct SlowQueue;

  #[async_trait]
  impl TaskQueue for SlowQueue {
    type Task = usize;
    type Value = usize;

    async fn batch_process<const N: usize>(
      batch: PendingAssignment<'async_trait, Self, N>,
    ) -> CompletionReceipt<Self> {
      let assignment = batch.into_assignment();

      assignment
        .resolve_blocking(|tasks| {
          thread::sleep(Duration::from_millis(50));

          tasks
        })
        .await
    }
  }

  #[cfg(not(loom))]
  #[tokio::test(flavor = "multi_thread")]

  async fn it_has_drop_safety() {
    let handle = tokio::task::spawn(async {
      SlowQueue::auto_batch(0).await;
    });

    yield_now().await;

    handle.abort();
  }

  #[derive(LocalQueue)]
  struct YieldQueue;

  #[async_trait]
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
  #[tokio::test(flavor = "multi_thread")]

  async fn it_negotiates_receiver_drop() {
    use std::sync::Arc;

    use tokio::sync::Barrier;

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

  #[derive(LocalSliceQueue)]
  struct EchoSliceQueue;

  #[async_trait]
  impl SliceQueue for EchoSliceQueue {
    type Task = (usize, oneshot::Sender<usize>);

    async fn batch_process<const N: usize>(tasks: UnboundedSlice<'async_trait, Self, N>) {
      let tasks = tasks.into_bounded().to_vec();

      for (val, tx) in tasks.into_iter() {
        tx.send(val).ok();
      }
    }
  }

  #[cfg(not(loom))]
  #[tokio::test(flavor = "multi_thread")]

  async fn it_process_background_tasks() {
    let receivers: Vec<_> = (0..100_usize)
      .into_iter()
      .map(|i| {
        let (tx, rx) = oneshot::channel::<usize>();
        EchoSliceQueue::auto_batch((i, tx));
        rx
      })
      .collect();

    for (i, rx) in receivers.into_iter().enumerate() {
      assert_eq!(rx.await, Ok(i));
    }
  }
}
