use std::{
  cell::UnsafeCell,
  fmt::Debug,
  mem::{needs_drop, MaybeUninit},
  ops::BitAnd,
  ptr::addr_of,
  sync::atomic::{AtomicUsize, Ordering},
  thread::LocalKey,
};

use async_t::async_trait;
use bit_bounds::{usize::Int, IsPowerOf2};
use cache_padded::CachePadded;
use const_assert::{Assert, IsFalse, IsTrue};
use tokio::runtime::Handle;

use crate::{
  assignment::{CompletionReceipt, PendingAssignment},
  helpers::*,
  task::{AutoBatch, Receiver, TaskRef},
};

#[cfg(target_pointer_width = "64")]
pub(crate) const INDEX_SHIFT: usize = 32;
#[cfg(target_pointer_width = "32")]
pub(crate) const INDEX_SHIFT: usize = 16;

pub(crate) const MIN_BUFFER_LEN: usize = 256;

#[cfg(target_pointer_width = "64")]
pub(crate) const MAX_BUFFER_LEN: usize = u32::MAX as usize;
#[cfg(target_pointer_width = "32")]
pub(crate) const MAX_BUFFER_LEN: usize = u16::MAX as usize;

#[async_trait]
pub trait TaskQueue: Send + Sync + Sized + 'static {
  type Task: Send + Sync + Sized + 'static;
  type Value: Send + Sync + Sized + 'static;

  fn queue() -> &'static LocalKey<StackQueue<Self>>;

  fn auto_batch(task: Self::Task) -> AutoBatch<Self> {
    AutoBatch::new(task)
  }

  async fn batch_process(assignment: PendingAssignment<Self>) -> CompletionReceipt<Self>;
}

pub(crate) struct Inner<T: TaskQueue, const N: usize = 2048>
where
  Int<N>: IsPowerOf2,
{
  pub(crate) slot: CachePadded<AtomicUsize>,
  pub(crate) occupancy: CachePadded<AtomicUsize>,
  pub(crate) buffer: [UnsafeCell<MaybeUninit<TaskRef<T>>>; N],
}
impl<T: TaskQueue, const N: usize> Inner<T, N>
where
  T: TaskQueue,
  Int<N>: IsPowerOf2,
{
  const fn new() -> Self {
    Inner {
      slot: CachePadded::new(AtomicUsize::new(0)),
      occupancy: CachePadded::new(AtomicUsize::new(0)),
      #[allow(clippy::uninit_assumed_init)]
      buffer: unsafe { MaybeUninit::uninit().assume_init() },
    }
  }
}

#[derive(Debug)]
pub(crate) struct QueueFull;

/// Task queue designed for facilitating heapless auto-batching of tasks
pub struct StackQueue<T: TaskQueue, const N: usize = 2048>
where
  Int<N>: IsPowerOf2,
{
  slot: CachePadded<UnsafeCell<usize>>,
  occupancy: CachePadded<UnsafeCell<usize>>,
  inner: Inner<T, N>,
}

impl<T: TaskQueue, const N: usize> StackQueue<T, N>
where
  T: TaskQueue,
  Int<N>: IsPowerOf2,
{
  pub const fn new() -> Self
  where
    Assert<{ N >= MIN_BUFFER_LEN }>: IsTrue,
    Assert<{ N <= MAX_BUFFER_LEN }>: IsTrue,
    Assert<{ needs_drop::<T::Task>() }>: IsFalse,
  {
    StackQueue {
      slot: CachePadded::new(UnsafeCell::new(N << INDEX_SHIFT)),
      occupancy: CachePadded::new(UnsafeCell::new(0)),
      inner: Inner::new(),
    }
  }

  #[inline(always)]
  fn current_write_index(&self) -> usize {
    // This algorithm can utilize an UnsafeCell for the index counter because where the current task
    // is written is independent of when a phase change would result in a new task batch owning a
    // new range of the buffer; only ownership is determined by atomic synchronization, not location
    let slot = unsafe { &*self.slot.get() };

    slot_index::<N>(slot)
  }

  fn check_regional_occupancy(&self, index: &usize) -> Result<(), QueueFull> {
    let region_mask = region_mask::<N>(index);

    // If this is out of sync, then the region could be incorrectly marked as full, but never
    // incorrectly marked as free, and so this optimization allows us to avoid the overhead of an
    // atomic call so long as regions are cleared within a full cycle
    if unsafe { &*self.occupancy.get() }.bitand(region_mask).eq(&0) {
      return Ok(());
    }

    // Usually this slow path won't occur because occupancy syncs when new batches are created
    let occupancy = self.inner.occupancy.load(Ordering::Relaxed);
    let regional_occupancy = occupancy.bitand(region_mask);

    unsafe { *self.occupancy.get() = occupancy };

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

    unsafe { *self.occupancy.get() = occupancy };
  }

  fn write_with<F>(&self, index: &usize, write_with: F)
  where
    F: FnOnce(*const AtomicUsize) -> (T::Task, *const Receiver<T>),
  {
    let cell = unsafe { &mut *self.inner.buffer.get_unchecked(*index).get() };
    cell.write(TaskRef::new_uninit());
    let task_ref = unsafe { cell.assume_init_mut() };
    let (task, rx) = write_with(task_ref.state_ptr());
    task_ref.init(task, rx);
  }

  #[inline(always)]
  fn replace_slot(&self, slot: usize) -> usize {
    std::mem::replace(unsafe { &mut *self.slot.get() }, slot)
  }

  pub(crate) fn enqueue<F>(
    &self,
    write_with: F,
  ) -> Result<Option<PendingAssignment<T, N>>, QueueFull>
  where
    F: FnOnce(*const AtomicUsize) -> (T::Task, *const Receiver<T>),
  {
    let write_index = self.current_write_index();

    // Regions sizes are always a power of 2, and so this acts as an optimized modulus operation
    if write_index.bitand(region_size::<N>() - 1).eq(&0) {
      self.check_regional_occupancy(&write_index)?;
    }

    self.write_with(&write_index, write_with);

    let base_slot = self
      .inner
      .slot
      .fetch_add(1 << INDEX_SHIFT, Ordering::Relaxed);

    let prev_slot = self.replace_slot(base_slot.wrapping_add(1 << INDEX_SHIFT));

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
impl<T: TaskQueue, const N: usize> Drop for StackQueue<T, N>
where
  T: TaskQueue,
  Int<N>: IsPowerOf2,
{
  fn drop(&mut self) {
    while self.inner.occupancy.load(Ordering::Relaxed).ne(&0) {
      Handle::current().block_on(tokio::task::yield_now());
    }
  }
}

#[cfg(all(test))]
mod test {
  use std::{thread::LocalKey, time::Duration};

  use async_t::async_trait;
  use futures::{future::join_all, stream::FuturesUnordered, StreamExt};
  use tokio::{task::yield_now, time::sleep};

  use super::{StackQueue, TaskQueue};
  use crate::assignment::{CompletionReceipt, PendingAssignment};

  struct EchoQueue;

  #[async_trait]
  impl TaskQueue for EchoQueue {
    type Task = usize;
    type Value = usize;

    fn queue() -> &'static LocalKey<super::StackQueue<Self>> {
      thread_local! {
        static QUEUE: StackQueue<EchoQueue> = StackQueue::new();
      }

      &QUEUE
    }

    async fn batch_process(batch: PendingAssignment<Self>) -> CompletionReceipt<Self> {
      let assignment = batch.into_assignment();
      assignment.map(|val| val)
    }
  }

  #[tokio::test(flavor = "multi_thread")]

  async fn it_process_tasks() {
    let batch: Vec<usize> = join_all((0..100).map(|i| EchoQueue::auto_batch(i))).await;

    assert_eq!(batch, (0..100).collect::<Vec<usize>>());
  }

  #[tokio::test(flavor = "multi_thread")]

  async fn it_cycles() {
    for i in 0..4096 {
      EchoQueue::auto_batch(i).await;
    }
  }

  struct SlowQueue;

  #[async_trait]
  impl TaskQueue for SlowQueue {
    type Task = usize;
    type Value = usize;

    fn queue() -> &'static LocalKey<super::StackQueue<Self>> {
      thread_local! {
        static QUEUE: StackQueue<SlowQueue> = StackQueue::new();
      }

      &QUEUE
    }

    async fn batch_process(batch: PendingAssignment<Self>) -> CompletionReceipt<Self> {
      let assignment = batch.into_assignment();

      sleep(Duration::from_millis(50)).await;

      assignment.map(|val| val)
    }
  }

  #[tokio::test(flavor = "multi_thread")]

  async fn it_has_drop_safety() {
    let handle = tokio::task::spawn(async {
      SlowQueue::auto_batch(0).await;
    });

    sleep(Duration::from_millis(1)).await;

    handle.abort();
  }

  struct YieldQueue;

  #[async_trait]
  impl TaskQueue for YieldQueue {
    type Task = usize;
    type Value = usize;

    fn queue() -> &'static LocalKey<super::StackQueue<Self>> {
      thread_local! {
        static QUEUE: StackQueue<YieldQueue> = StackQueue::new();
      }

      &QUEUE
    }

    async fn batch_process(batch: PendingAssignment<Self>) -> CompletionReceipt<Self> {
      let assignment = batch.into_assignment();

      yield_now().await;

      assignment.map(|val| val)
    }
  }

  #[tokio::test(flavor = "multi_thread")]

  async fn it_negotiates_receiver_drop() {
    let tasks: FuturesUnordered<_> = (0..8)
      .map(|_| {
        tokio::task::spawn(async {
          let tasks: FuturesUnordered<_> = (0..16384)
            .map(|i| async move {
              let handle = tokio::task::spawn(async move {
                YieldQueue::auto_batch(i).await;
              });

              yield_now().await;

              handle.abort()
            })
            .collect();

          let _: Vec<_> = tasks.collect().await;
        })
      })
      .collect::<FuturesUnordered<_>>();

    tasks.collect::<Vec<_>>().await;
  }
}
