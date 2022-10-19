use std::{
  marker::PhantomData,
  mem,
  ops::{BitAnd, Range},
  sync::atomic::Ordering,
};

use bit_bounds::{usize::Int, IsPowerOf2};

use crate::{
  helpers::{active_phase_bit, one_shifted, slot_index},
  queue::{Inner, TaskQueue, INDEX_SHIFT},
  TaskRef,
};

/// The responsibilty to process a yet to be assigned set of tasks on the queue. Once converted
/// into a [`TaskAssignment`] the task range responsible for processing will be bounded and further
/// tasks enqueued will be of a new batch. Assignment of a task range can be deferred until
/// resources such as database connections are ready as a way to process tasks in larger batches.
pub struct PendingAssignment<T: TaskQueue, const N: usize = 2048>
where
  Int<N>: IsPowerOf2,
{
  base_slot: usize,
  queue_ptr: *const Inner<T, N>,
}

impl<T, const N: usize> PendingAssignment<T, N>
where
  T: TaskQueue,
  Int<N>: IsPowerOf2,
{
  pub(crate) fn new(base_slot: usize, queue_ptr: *const Inner<T, N>) -> Self {
    PendingAssignment {
      base_slot,
      queue_ptr,
    }
  }

  #[inline(always)]
  fn queue(&self) -> &Inner<T, N> {
    // This can be safely dereferenced because Inner is immovable by design and will suspend it's
    // thread termination as necessary until no references exist
    unsafe { &*self.queue_ptr }
  }

  // Converted into a bounded task assignment
  pub fn into_assignment(self) -> TaskAssignment<T, N> {
    let phase_bit = active_phase_bit::<N>(&self.base_slot);

    let end_slot = self.queue().slot.fetch_xor(phase_bit, Ordering::Relaxed);

    // If the N bit has changed it means the queue has wrapped around the beginning
    let task_range = if (N << INDEX_SHIFT).bitand(self.base_slot ^ end_slot).eq(&0) {
      // (base_slot >> INDEX_SHIFT) extracts the current index
      // Because N is a power of 2, N - 1 will be a mask with N bits set
      // For all values N, index & (N - 1) is always index % N however with fewer instructions
      let start = (self.base_slot >> INDEX_SHIFT) & (N - 1);

      // It can be known that the end slot will never wrap around because usize::MAX is divisable by
      // all values N
      let end = (end_slot >> INDEX_SHIFT) & (N - 1);

      start..end
    } else {
      // The active phase bit alternated when the N bit changed as a consequence of this wrapping
      // around to the beginning, and so this batch goes to the end and the queue will create a new
      // task batch while ignoring that the now inactive phase bit was changed.
      (self.base_slot >> INDEX_SHIFT) & (N - 1)..N
    };

    let queue_ptr = self.queue_ptr;

    mem::forget(self);

    TaskAssignment::new(task_range, queue_ptr)
  }

  fn deoccupy_buffer(&self) {
    let index = slot_index::<N>(&self.base_slot);
    let shifted_sub = one_shifted::<N>(&index);

    self
      .queue()
      .occupancy
      .fetch_sub(shifted_sub, Ordering::Release);
  }
}

// This is safe because queue_ptr is guaranteed to be immovable and non-null while
// references exist
unsafe impl<T> Send for PendingAssignment<T> where T: TaskQueue {}
unsafe impl<T> Sync for PendingAssignment<T> where T: TaskQueue {}

impl<T, const N: usize> Drop for PendingAssignment<T, N>
where
  T: TaskQueue,
  Int<N>: IsPowerOf2,
{
  fn drop(&mut self) {
    self.deoccupy_buffer();
  }
}

pub struct TaskAssignment<T: TaskQueue, const N: usize = 2048>
where
  Int<N>: IsPowerOf2,
{
  task_range: Range<usize>,
  queue_ptr: *const Inner<T, N>,
}

impl<T, const N: usize> TaskAssignment<T, N>
where
  T: TaskQueue,
  Int<N>: IsPowerOf2,
{
  fn new(task_range: Range<usize>, queue_ptr: *const Inner<T, N>) -> Self {
    TaskAssignment {
      task_range,
      queue_ptr,
    }
  }

  #[inline(always)]
  fn queue(&self) -> &Inner<T, N> {
    // This can be safely dereferenced because Inner is immovable by design and will block it's
    // thread termination as necessary until no references exist
    unsafe { &*self.queue_ptr }
  }

  // A transmuted slice of the assigned task range
  pub fn tasks(&self) -> &[TaskRef<T>] {
    unsafe { mem::transmute(self.queue().buffer.get_unchecked(self.task_range.clone())) }
  }

  // Map each task into it's respective value and resolve, in parallel
  pub fn map<F>(self, op: F) -> CompletionReceipt<T>
  where
    F: Fn(T::Task) -> T::Value + Sync,
  {
    self.tasks().iter().for_each(|task_ref| unsafe {
      let task = task_ref.take_task_unchecked();
      task_ref.resolve_unchecked(op(task));
    });

    self.into_completion_receipt()
  }

  fn deoccupy_buffer(&self) {
    let shifted_sub = one_shifted::<N>(&self.task_range.start);

    self
      .queue()
      .occupancy
      .fetch_sub(shifted_sub, Ordering::Release);
  }

  fn into_completion_receipt(self) -> CompletionReceipt<T> {
    self.deoccupy_buffer();

    mem::forget(self);

    CompletionReceipt::new()
  }
}

impl<T, const N: usize> Drop for TaskAssignment<T, N>
where
  T: TaskQueue,
  Int<N>: IsPowerOf2,
{
  fn drop(&mut self) {
    self.deoccupy_buffer();
  }
}

// This is safe because queue_ptr is guaranteed to be immovable and non-null while
// references exist
unsafe impl<T> Send for TaskAssignment<T> where T: TaskQueue {}
unsafe impl<T> Sync for TaskAssignment<T> where T: TaskQueue {}

// A type-state proof of completion for a task assignment. This ensures all task are eventually
// processed while not precluding the usage of [`tokio::task::spawn_blocking`]
pub struct CompletionReceipt<T: TaskQueue>(PhantomData<T>);

impl<T> CompletionReceipt<T>
where
  T: TaskQueue,
{
  fn new() -> Self {
    CompletionReceipt(PhantomData)
  }
}
