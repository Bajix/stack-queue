use std::{
  marker::PhantomData,
  mem,
  ops::{BitAnd, Range},
  sync::atomic::Ordering,
};

use crate::{
  helpers::{active_phase_bit, one_shifted},
  queue::{Inner, TaskQueue, INDEX_SHIFT},
  task::TaskRef,
};

/// The responsibilty to process a yet to be assigned set of tasks on the queue.
pub struct PendingAssignment<T: TaskQueue, const N: usize> {
  base_slot: usize,
  queue_ptr: *const Inner<T, N>,
}

impl<T, const N: usize> PendingAssignment<T, N>
where
  T: TaskQueue,
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

  fn set_assignment_bounds(&self) -> Range<usize> {
    let phase_bit = active_phase_bit::<N>(&self.base_slot);

    let end_slot = self.queue().slot.fetch_xor(phase_bit, Ordering::Relaxed);

    // If the N bit has changed it means the queue has wrapped around the beginning
    if (N << INDEX_SHIFT).bitand(self.base_slot ^ end_slot).eq(&0) {
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
    }
  }

  /// By converting into a [`TaskAssignment`] the task range responsible for processing will be
  /// bounded and further tasks enqueued will be of a new batch. Assignment of a task range can be
  /// deferred until resources such as database connections are ready as a way to process tasks in
  /// larger batches. This operation is constant time and wait-free
  pub fn into_assignment(self) -> TaskAssignment<T, N> {
    let task_range = self.set_assignment_bounds();
    let queue_ptr = self.queue_ptr;

    mem::forget(self);

    TaskAssignment::new(task_range, queue_ptr)
  }
}

// This is safe because queue_ptr is guaranteed to be immovable and non-null while
// references exist
unsafe impl<T, const N: usize> Send for PendingAssignment<T, N> where T: TaskQueue {}
unsafe impl<T, const N: usize> Sync for PendingAssignment<T, N> where T: TaskQueue {}

impl<T, const N: usize> Drop for PendingAssignment<T, N>
where
  T: TaskQueue,
{
  fn drop(&mut self) {
    let task_range = self.set_assignment_bounds();
    let queue_ptr = self.queue_ptr;

    TaskAssignment::new(task_range, queue_ptr);
  }
}

/// An assignment of a task range to be processed
pub struct TaskAssignment<T: TaskQueue, const N: usize> {
  task_range: Range<usize>,
  queue_ptr: *const Inner<T, N>,
}

impl<T, const N: usize> TaskAssignment<T, N>
where
  T: TaskQueue,
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

  /// A slice of the assigned task range
  pub fn tasks(&self) -> &[TaskRef<T>] {
    unsafe { self.queue().buffer.get_unchecked(self.task_range.clone()) }
  }

  /// Resolve task assignment with an iterator where indexes align with tasks
  pub fn resolve_with_iter<I>(self, iter: I) -> CompletionReceipt<T>
  where
    I: IntoIterator<Item = T::Value>,
  {
    self
      .tasks()
      .iter()
      .zip(iter)
      .for_each(|(task_ref, value)| unsafe {
        task_ref.resolve_unchecked(value);
      });

    self.into_completion_receipt()
  }

  /// Resolve task assignment by mapping each task into it's respective value
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
    self
      .queue()
      .occupancy
      .fetch_sub(one_shifted::<N>(&self.task_range.start), Ordering::Relaxed);
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
{
  fn drop(&mut self) {
    self
      .tasks()
      .iter()
      .for_each(|task_ref| unsafe { drop(task_ref.take_task_unchecked()) });

    self.deoccupy_buffer();
  }
}

// This is safe because queue_ptr is guaranteed to be immovable and non-null while
// references exist
unsafe impl<T, const N: usize> Send for TaskAssignment<T, N> where T: TaskQueue {}
unsafe impl<T, const N: usize> Sync for TaskAssignment<T, N> where T: TaskQueue {}

/// A type-state proof of completion for a task assignment
pub struct CompletionReceipt<T: TaskQueue>(PhantomData<T>);

impl<T> CompletionReceipt<T>
where
  T: TaskQueue,
{
  fn new() -> Self {
    CompletionReceipt(PhantomData)
  }
}
