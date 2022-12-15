#[cfg(not(loom))]
use std::sync::atomic::{fence, Ordering};
use std::{
  marker::PhantomData,
  mem,
  ops::{BitAnd, Deref, Range},
};

use async_local::RefGuard;
#[cfg(loom)]
use loom::sync::atomic::{fence, Ordering};
use shutdown_barrier::defer_shutdown;

use crate::{
  helpers::{active_phase_bit, one_shifted},
  queue::{Inner, TaskQueue, INDEX_SHIFT},
  task::TaskRef,
  BackgroundQueue, BufferCell,
};

/// The responsibilty to process a yet to be assigned set of tasks on the queue.
pub struct PendingAssignment<'a, T: TaskQueue, const N: usize> {
  base_slot: usize,
  queue: RefGuard<'a, Inner<TaskRef<T>, N>>,
}

impl<'a, T, const N: usize> PendingAssignment<'a, T, N>
where
  T: TaskQueue,
{
  pub(crate) fn new(base_slot: usize, queue: RefGuard<'a, Inner<TaskRef<T>, N>>) -> Self {
    PendingAssignment { base_slot, queue }
  }

  fn set_assignment_bounds(&self) -> Range<usize> {
    let phase_bit = active_phase_bit::<N>(&self.base_slot);

    let end_slot = self.queue.slot.fetch_xor(phase_bit, Ordering::Relaxed);

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
  pub fn into_assignment(self) -> TaskAssignment<'a, T, N> {
    let task_range = self.set_assignment_bounds();
    let queue = self.queue;

    mem::forget(self);

    TaskAssignment::new(task_range, queue)
  }

  /// Resolve task assignment within a thread where blocking is acceptable. If the runtime would
  /// otherwise shutdown, instead it will suspend for the lifetime of this thread as to protect the
  /// underlying thread local data from being destroyed
  pub async fn with_blocking<F>(self, f: F) -> CompletionReceipt<T>
  where
    F: for<'b> FnOnce(PendingAssignment<'b, T, N>) -> CompletionReceipt<T> + Send + 'static,
  {
    let batch: PendingAssignment<'_, T, N> = unsafe { std::mem::transmute(self) };
    tokio::task::spawn_blocking(move || {
      defer_shutdown();
      f(batch)
    })
    .await
    .unwrap()
  }
}

unsafe impl<'a, T, const N: usize> Send for PendingAssignment<'a, T, N> where T: TaskQueue {}
unsafe impl<'a, T, const N: usize> Sync for PendingAssignment<'a, T, N>
where
  T: TaskQueue,
  <T as TaskQueue>::Task: Sync,
{
}

impl<'a, T, const N: usize> Drop for PendingAssignment<'a, T, N>
where
  T: TaskQueue,
{
  fn drop(&mut self) {
    let task_range = self.set_assignment_bounds();
    let queue = self.queue;

    TaskAssignment::new(task_range, queue);
  }
}

/// An assignment of a task range to be processed
pub struct TaskAssignment<'a, T: TaskQueue, const N: usize> {
  task_range: Range<usize>,
  queue: RefGuard<'a, Inner<TaskRef<T>, N>>,
}

impl<'a, T, const N: usize> TaskAssignment<'a, T, N>
where
  T: TaskQueue,
{
  fn new(task_range: Range<usize>, queue: RefGuard<'a, Inner<TaskRef<T>, N>>) -> Self {
    TaskAssignment { task_range, queue }
  }

  /// A slice of the assigned task range
  pub fn tasks(&self) -> &[TaskRef<T>] {
    unsafe { self.queue.buffer.get_unchecked(self.task_range.clone()) }
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
        drop(task_ref.take_task_unchecked());
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
      .queue
      .occupancy
      .fetch_sub(one_shifted::<N>(&self.task_range.start), Ordering::Relaxed);
  }

  fn into_completion_receipt(self) -> CompletionReceipt<T> {
    self.deoccupy_buffer();

    mem::forget(self);

    CompletionReceipt::new()
  }

  /// Resolve task assignment within a thread where blocking is acceptable. If the runtime would
  /// otherwise shutdown, instead it will suspend for the lifetime of this thread as to protect the
  /// underlying thread local data from being destroyed
  pub async fn with_blocking<F>(self, f: F) -> CompletionReceipt<T>
  where
    F: for<'b> FnOnce(TaskAssignment<'b, T, N>) -> CompletionReceipt<T> + Send + 'static,
  {
    let batch: TaskAssignment<'_, T, N> = unsafe { std::mem::transmute(self) };
    tokio::task::spawn_blocking(move || {
      defer_shutdown();
      f(batch)
    })
    .await
    .unwrap()
  }
}

impl<'a, T, const N: usize> Drop for TaskAssignment<'a, T, N>
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

unsafe impl<'a, T, const N: usize> Send for TaskAssignment<'a, T, N> where T: TaskQueue {}
unsafe impl<'a, T, const N: usize> Sync for TaskAssignment<'a, T, N>
where
  T: TaskQueue,
  <T as TaskQueue>::Task: Sync,
{
}

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

/// Exclusive reference over an unbounded task range
pub struct UnboundedSlice<'a, T: BackgroundQueue, const N: usize> {
  base_slot: usize,
  queue: RefGuard<'a, Inner<BufferCell<T::Task>, N>>,
}

impl<'a, T, const N: usize> UnboundedSlice<'a, T, N>
where
  T: BackgroundQueue,
{
  pub(crate) fn new(base_slot: usize, queue: RefGuard<'a, Inner<BufferCell<T::Task>, N>>) -> Self {
    UnboundedSlice { base_slot, queue }
  }

  fn set_bounds(&self) -> Range<usize> {
    let phase_bit = active_phase_bit::<N>(&self.base_slot);

    let end_slot = self.queue.slot.fetch_xor(phase_bit, Ordering::Relaxed);

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

  pub fn into_bounded(self) -> BoundedSlice<'a, T, N> {
    let task_range = self.set_bounds();
    let queue = self.queue;

    mem::forget(self);

    BoundedSlice::new(task_range, queue)
  }
}

impl<'a, T, const N: usize> Drop for UnboundedSlice<'a, T, N>
where
  T: BackgroundQueue,
{
  fn drop(&mut self) {
    let task_range = self.set_bounds();
    let one_shifted = one_shifted::<N>(&task_range.start);

    let queue = self.queue;

    for index in task_range {
      unsafe {
        queue.with_buffer_cell(|cell| (*cell).assume_init_drop(), index);
      }
    }

    fence(Ordering::Release);

    self
      .queue
      .occupancy
      .fetch_sub(one_shifted, Ordering::Relaxed);
  }
}

unsafe impl<'a, T, const N: usize> Send for UnboundedSlice<'a, T, N> where T: BackgroundQueue {}
unsafe impl<'a, T, const N: usize> Sync for UnboundedSlice<'a, T, N>
where
  T: BackgroundQueue,
  <T as BackgroundQueue>::Task: Sync,
{
}

/// Exclusive reference over a bounded task range
pub struct BoundedSlice<'a, T: BackgroundQueue, const N: usize> {
  task_range: Range<usize>,
  queue: RefGuard<'a, Inner<BufferCell<T::Task>, N>>,
}

impl<'a, T, const N: usize> BoundedSlice<'a, T, N>
where
  T: BackgroundQueue,
{
  fn new(task_range: Range<usize>, queue: RefGuard<'a, Inner<BufferCell<T::Task>, N>>) -> Self {
    BoundedSlice { task_range, queue }
  }

  /// A slice of the bounded task range
  pub fn tasks(&self) -> &[T::Task] {
    unsafe { mem::transmute(self.queue.buffer.get_unchecked(self.task_range.clone())) }
  }

  pub fn to_vec(self) -> Vec<T::Task> {
    let tasks = self.tasks();
    let len = tasks.len();
    let mut buffer = Vec::new();
    buffer.reserve_exact(len);

    unsafe {
      std::ptr::copy_nonoverlapping(tasks.as_ptr(), buffer.as_mut_ptr(), len);
      buffer.set_len(len);
    }

    self.deoccupy_buffer();

    mem::forget(self);

    buffer
  }

  fn deoccupy_buffer(&self) {
    self
      .queue
      .occupancy
      .fetch_sub(one_shifted::<N>(&self.task_range.start), Ordering::Release);
  }
}

impl<'a, T, const N: usize> Deref for BoundedSlice<'a, T, N>
where
  T: BackgroundQueue,
{
  type Target = [T::Task];
  fn deref(&self) -> &Self::Target {
    self.tasks()
  }
}

impl<'a, T, const N: usize> Drop for BoundedSlice<'a, T, N>
where
  T: BackgroundQueue,
{
  fn drop(&mut self) {
    for index in self.task_range.clone() {
      unsafe {
        self
          .queue
          .with_buffer_cell(|cell| (*cell).assume_init_drop(), index);
      }
    }

    self.deoccupy_buffer();
  }
}

unsafe impl<'a, T, const N: usize> Send for BoundedSlice<'a, T, N> where T: BackgroundQueue {}
unsafe impl<'a, T, const N: usize> Sync for BoundedSlice<'a, T, N>
where
  T: BackgroundQueue,
  <T as BackgroundQueue>::Task: Sync,
{
}

/// An iterator over an exclusively referenced range of tasks
pub struct TaskIter<'a, T: BackgroundQueue, const N: usize> {
  current: Option<usize>,
  task_range: Range<usize>,
  queue: RefGuard<'a, Inner<BufferCell<T::Task>, N>>,
}

impl<'a, T, const N: usize> TaskIter<'a, T, N>
where
  T: BackgroundQueue,
{
  fn deoccupy_buffer(&self) {
    self
      .queue
      .occupancy
      .fetch_sub(one_shifted::<N>(&self.task_range.start), Ordering::Relaxed);
  }
}

impl<'a, T, const N: usize> Iterator for TaskIter<'a, T, N>
where
  T: BackgroundQueue,
{
  type Item = T::Task;

  fn next(&mut self) -> Option<Self::Item> {
    if let Some(index) = self.current {
      self.current = if self.task_range.end > index {
        Some(index + 1)
      } else {
        None
      };

      let task = unsafe {
        self
          .queue
          .with_buffer_cell(|cell| (*cell).assume_init_read(), index)
      };

      Some(task)
    } else {
      None
    }
  }
}

unsafe impl<'a, T, const N: usize> Send for TaskIter<'a, T, N> where T: BackgroundQueue {}
unsafe impl<'a, T, const N: usize> Sync for TaskIter<'a, T, N>
where
  T: BackgroundQueue,
  <T as BackgroundQueue>::Task: Sync,
{
}

impl<'a, T, const N: usize> Drop for TaskIter<'a, T, N>
where
  T: BackgroundQueue,
{
  fn drop(&mut self) {
    while self.next().is_some() {}
    self.deoccupy_buffer();
  }
}

impl<'a, T, const N: usize> IntoIterator for BoundedSlice<'a, T, N>
where
  T: BackgroundQueue,
{
  type Item = T::Task;
  type IntoIter = TaskIter<'a, T, N>;

  fn into_iter(self) -> Self::IntoIter {
    let iter = TaskIter {
      current: Some(self.task_range.start),
      task_range: self.task_range.clone(),
      queue: self.queue,
    };

    mem::forget(self);

    iter
  }
}
