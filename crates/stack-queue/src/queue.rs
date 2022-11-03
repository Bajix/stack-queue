use std::{array, fmt::Debug, ops::BitAnd, ptr::addr_of};
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

use crate::{
  assignment::{CompletionReceipt, PendingAssignment},
  helpers::*,
  task::{AutoBatchedTask, Receiver, TaskRef},
  MAX_BUFFER_LEN, MIN_BUFFER_LEN,
};

#[cfg(target_pointer_width = "64")]
pub(crate) const INDEX_SHIFT: usize = 32;
#[cfg(target_pointer_width = "32")]
pub(crate) const INDEX_SHIFT: usize = 16;
#[async_trait]
pub trait TaskQueue: Send + Sync + Sized + 'static {
  type Task: Send + Sync + Sized + 'static;
  type Value: Send + Sync + Sized + 'static;

  #[allow(clippy::needless_lifetimes)]
  async fn batch_process<const N: usize>(
    assignment: PendingAssignment<'async_trait, Self, N>,
  ) -> CompletionReceipt<Self>;
}

pub trait LocalQueue<const N: usize>: TaskQueue {
  fn queue() -> &'static LocalKey<StackQueue<Self, N>>;

  fn auto_batch(task: Self::Task) -> AutoBatchedTask<Self, N> {
    AutoBatchedTask::new(task)
  }
}

pub(crate) struct Inner<T: TaskQueue, const N: usize = 2048> {
  pub(crate) slot: CachePadded<AtomicUsize>,
  pub(crate) occupancy: CachePadded<AtomicUsize>,
  pub(crate) buffer: [TaskRef<T>; N],
}
impl<T: TaskQueue, const N: usize> Inner<T, N>
where
  T: TaskQueue,
{
  fn new() -> Self {
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
pub struct StackQueue<T: TaskQueue, const N: usize = 2048> {
  slot: CachePadded<UnsafeCell<usize>>,
  occupancy: CachePadded<UnsafeCell<usize>>,
  inner: Inner<T, N>,
}

impl<T: TaskQueue, const N: usize> Default for StackQueue<T, N>
where
  T: TaskQueue,
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
      inner: Inner::new(),
    }
  }
}

impl<T: TaskQueue, const N: usize> StackQueue<T, N>
where
  T: TaskQueue,
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

  unsafe fn write_with<F>(&self, index: &usize, write_with: F)
  where
    F: FnOnce(*const AtomicUsize) -> (T::Task, *const Receiver<T>),
  {
    let task_ref = self.inner.buffer.get_unchecked(*index);
    let (task, rx) = write_with(task_ref.state_ptr());
    task_ref.set_task(task, rx);
  }

  #[inline(always)]
  unsafe fn replace_slot(&self, slot: usize) -> usize {
    self.with_slot_mut(move |val| std::mem::replace(&mut *val, slot))
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

// The purpose of this drop is to block deallocation to ensure the validity of references until
// dropped as a way of sequencing shutdowns without introducing undefined behavior. While this
// approach is not ideal, this is at worst an edge-case that can only happen after a shutdown signal
// has occurred, and so this is given preference over other techniques that would come at a
// continuous performance cost. This approach allows us to avoid the necessity for condvar / mutexes
#[cfg(not(loom))]
impl<T: TaskQueue, const N: usize> Drop for StackQueue<T, N>
where
  T: TaskQueue,
{
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
  use derive_stack_queue::LocalQueue;
  #[cfg(not(loom))]
  use futures::{future::join_all, stream::FuturesUnordered, StreamExt};
  use tokio::task::yield_now;

  #[cfg(not(loom))]
  use super::LocalQueue;
  use super::TaskQueue;
  use crate::assignment::{CompletionReceipt, PendingAssignment};

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
}
