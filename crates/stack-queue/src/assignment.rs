#[cfg(not(loom))]
use std::sync::atomic::{fence, Ordering};
use std::{marker::PhantomData, mem, ops::Range};

use async_local::RefGuard;
#[cfg(loom)]
use loom::sync::atomic::{fence, Ordering};
#[cfg(not(loom))]
use tokio::task::{spawn_blocking, JoinHandle};

use crate::{
  helpers::one_shifted,
  queue::{Inner, TaskQueue, INDEX_SHIFT, PHASE},
  task::TaskRef,
  BufferCell,
};

/// The responsibilty to process a yet to be assigned set of tasks.
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
    let end_slot = self.queue.slot.fetch_xor(PHASE, Ordering::Relaxed);

    (self.base_slot >> INDEX_SHIFT)..(end_slot >> INDEX_SHIFT)
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

  /// Move [`PendingAssignment`] into a thread where blocking is acceptable.
  #[cfg(not(loom))]
  pub async fn with_blocking<F>(self, f: F) -> CompletionReceipt<T>
  where
    F: for<'b> FnOnce(PendingAssignment<'b, T, N>) -> CompletionReceipt<T> + Send + 'static,
  {
    let batch: PendingAssignment<'_, T, N> = unsafe { std::mem::transmute(self) };
    tokio::task::spawn_blocking(move || f(batch)).await.unwrap()
  }
}

unsafe impl<'a, T, const N: usize> Send for PendingAssignment<'a, T, N> where T: TaskQueue {}
unsafe impl<'a, T, const N: usize> Sync for PendingAssignment<'a, T, N> where T: TaskQueue {}

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

/// Assignment of a task range yet to be processed
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

  /// Returns a pair of slices which contain, in order, the contents of the assigned task range.
  pub fn as_slices(&self) -> (&[TaskRef<T>], &[TaskRef<T>]) {
    let start = self.task_range.start & (N - 1);
    let end = self.task_range.end & (N - 1);

    if end > start {
      unsafe { (self.queue.buffer.get_unchecked(start..end), &[]) }
    } else {
      unsafe {
        (
          self.queue.buffer.get_unchecked(start..N),
          self.queue.buffer.get_unchecked(0..end),
        )
      }
    }
  }

  /// An iterator over the assigned task range
  pub fn tasks(&self) -> impl Iterator<Item = &TaskRef<T>> {
    let tasks = self.as_slices();
    tasks.0.iter().chain(tasks.1.iter())
  }

  /// Resolve task assignment with an iterator where indexes align with tasks
  pub fn resolve_with_iter<I>(self, iter: I) -> CompletionReceipt<T>
  where
    I: IntoIterator<Item = T::Value>,
  {
    self.tasks().zip(iter).for_each(|(task_ref, value)| unsafe {
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
    self.tasks().for_each(|task_ref| unsafe {
      let task = task_ref.take_task_unchecked();
      task_ref.resolve_unchecked(op(task));
    });

    self.into_completion_receipt()
  }

  fn deoccupy_buffer(&self) {
    self.queue.occupancy.fetch_sub(
      one_shifted::<N>(self.task_range.start & (N - 1)),
      Ordering::Relaxed,
    );
  }

  fn into_completion_receipt(self) -> CompletionReceipt<T> {
    self.deoccupy_buffer();

    mem::forget(self);

    CompletionReceipt::new()
  }

  /// Move [`TaskAssignment`] into a thread where blocking is acceptable
  #[cfg(not(loom))]
  pub async fn with_blocking<F>(self, f: F) -> CompletionReceipt<T>
  where
    F: for<'b> FnOnce(TaskAssignment<'b, T, N>) -> CompletionReceipt<T> + Send + 'static,
  {
    let batch: TaskAssignment<'_, T, N> = unsafe { std::mem::transmute(self) };
    tokio::task::spawn_blocking(move || f(batch)).await.unwrap()
  }
}

impl<'a, T, const N: usize> Drop for TaskAssignment<'a, T, N>
where
  T: TaskQueue,
{
  fn drop(&mut self) {
    self
      .tasks()
      .for_each(|task_ref| unsafe { drop(task_ref.take_task_unchecked()) });

    self.deoccupy_buffer();
  }
}

unsafe impl<'a, T, const N: usize> Send for TaskAssignment<'a, T, N> where T: TaskQueue {}
unsafe impl<'a, T, const N: usize> Sync for TaskAssignment<'a, T, N> where T: TaskQueue {}

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
/// A guard granting exclusive access over an unbounded range of a buffer
pub struct UnboundedRange<'a, T: Send + Sync + Sized + 'static, const N: usize> {
  base_slot: usize,
  queue: RefGuard<'a, Inner<BufferCell<T>, N>>,
}

impl<'a, T, const N: usize> UnboundedRange<'a, T, N>
where
  T: Send + Sync + Sized + 'static,
{
  pub(crate) fn new(base_slot: usize, queue: RefGuard<'a, Inner<BufferCell<T>, N>>) -> Self {
    UnboundedRange { base_slot, queue }
  }

  fn set_bounds(&self) -> Range<usize> {
    let end_slot = self.queue.slot.fetch_xor(PHASE, Ordering::Relaxed);
    (self.base_slot >> INDEX_SHIFT)..(end_slot >> INDEX_SHIFT)
  }

  /// Establish exclusive access over a [`StackQueue`](crate::StackQueue) buffer range
  pub fn into_bounded(self) -> BoundedRange<'a, T, N> {
    let range = self.set_bounds();
    let queue = self.queue;

    mem::forget(self);

    BoundedRange::new(range, queue)
  }

  /// Move [`UnboundedRange`] into a thread where blocking is acceptable.
  #[cfg(not(loom))]
  pub fn with_blocking<F, R>(self, f: F) -> JoinHandle<R>
  where
    F: for<'b> FnOnce(UnboundedRange<'b, T, N>) -> R + Send + 'static,
    R: Send + 'static,
  {
    let batch: UnboundedRange<'_, T, N> = unsafe { std::mem::transmute(self) };
    spawn_blocking(move || f(batch))
  }
}

impl<'a, T, const N: usize> Drop for UnboundedRange<'a, T, N>
where
  T: Send + Sync + Sized + 'static,
{
  fn drop(&mut self) {
    let task_range = self.set_bounds();
    let one_shifted = one_shifted::<N>(task_range.start & (N - 1));

    let queue = self.queue;

    for index in task_range {
      unsafe {
        queue.with_buffer_cell(|cell| (*cell).assume_init_drop(), index & (N - 1));
      }
    }

    fence(Ordering::Release);

    self
      .queue
      .occupancy
      .fetch_sub(one_shifted, Ordering::Relaxed);
  }
}

unsafe impl<'a, T, const N: usize> Send for UnboundedRange<'a, T, N> where
  T: Send + Sync + Sized + 'static
{
}
unsafe impl<'a, T, const N: usize> Sync for UnboundedRange<'a, T, N> where
  T: Send + Sync + Sized + 'static
{
}

/// A guard granting exclusive access over a bounded range of a [`StackQueue`](crate::StackQueue)
/// buffer
pub struct BoundedRange<'a, T: Send + Sync + Sized + 'static, const N: usize> {
  range: Range<usize>,
  queue: RefGuard<'a, Inner<BufferCell<T>, N>>,
}

impl<'a, T, const N: usize> BoundedRange<'a, T, N>
where
  T: Send + Sync + Sized + 'static,
{
  fn new(range: Range<usize>, queue: RefGuard<'a, Inner<BufferCell<T>, N>>) -> Self {
    BoundedRange { range, queue }
  }

  /// Returns a pair of slices which contain, in order, the contents of the owned range from a
  /// [`StackQueue`](crate::StackQueue) buffer.
  #[cfg(not(loom))]
  pub fn as_slices(&self) -> (&[T], &[T]) {
    let start = self.range.start & (N - 1);
    let end = self.range.end & (N - 1);

    if end > start {
      unsafe {
        mem::transmute::<(&[BufferCell<T>], &[BufferCell<T>]), _>((
          self.queue.buffer.get_unchecked(start..end),
          &[],
        ))
      }
    } else {
      unsafe {
        mem::transmute((
          self.queue.buffer.get_unchecked(start..N),
          self.queue.buffer.get_unchecked(0..end),
        ))
      }
    }
  }

  /// An iterator over the owned task range
  #[cfg(not(loom))]
  pub fn iter(&self) -> impl Iterator<Item = &T> {
    let tasks = self.as_slices();
    tasks.0.iter().chain(tasks.1.iter())
  }

  #[cfg(not(loom))]
  pub fn to_vec(self) -> Vec<T> {
    let items = self.as_slices();
    let front_len = items.0.len();
    let back_len = items.1.len();
    let total_len = front_len + back_len;
    let mut buffer = Vec::new();
    buffer.reserve_exact(total_len);

    unsafe {
      std::ptr::copy_nonoverlapping(items.0.as_ptr(), buffer.as_mut_ptr(), front_len);
      if back_len > 0 {
        std::ptr::copy_nonoverlapping(
          items.1.as_ptr(),
          buffer.as_mut_ptr().add(front_len),
          back_len,
        );
      }
      buffer.set_len(total_len);
    }

    self.deoccupy_buffer();

    mem::forget(self);

    buffer
  }

  /// Move [`BoundedRange`] into a thread where blocking is acceptable.
  #[cfg(not(loom))]
  pub fn with_blocking<F, R>(self, f: F) -> JoinHandle<R>
  where
    F: for<'b> FnOnce(BoundedRange<'b, T, N>) -> R + Send + 'static,
    R: Send + 'static,
  {
    let batch: BoundedRange<'_, T, N> = unsafe { std::mem::transmute(self) };
    batch.queue.with_blocking(move |_| f(batch))
  }

  fn deoccupy_buffer(&self) {
    self.queue.occupancy.fetch_sub(
      one_shifted::<N>(self.range.start & (N - 1)),
      Ordering::Release,
    );
  }
}

impl<'a, T, const N: usize> Drop for BoundedRange<'a, T, N>
where
  T: Send + Sync + Sized + 'static,
{
  fn drop(&mut self) {
    for index in self.range.clone() {
      unsafe {
        self
          .queue
          .with_buffer_cell(|cell| (*cell).assume_init_drop(), index & (N - 1));
      }
    }

    self.deoccupy_buffer();
  }
}

unsafe impl<'a, T, const N: usize> Send for BoundedRange<'a, T, N> where
  T: Send + Sync + Sized + 'static
{
}
unsafe impl<'a, T, const N: usize> Sync for BoundedRange<'a, T, N> where
  T: Send + Sync + Sized + 'static
{
}

/// An iterator over an owned range of tasks from a [`StackQueue`](crate::StackQueue) buffer
pub struct BufferIter<'a, T: Send + Sync + Sized + 'static, const N: usize> {
  current: usize,
  range: Range<usize>,
  queue: RefGuard<'a, Inner<BufferCell<T>, N>>,
}

impl<'a, T, const N: usize> BufferIter<'a, T, N>
where
  T: Send + Sync + Sized + 'static,
{
  fn deoccupy_buffer(&self) {
    self.queue.occupancy.fetch_sub(
      one_shifted::<N>(self.range.start & (N - 1)),
      Ordering::Relaxed,
    );
  }
}

impl<'a, T, const N: usize> Iterator for BufferIter<'a, T, N>
where
  T: Send + Sync + Sized + 'static,
{
  type Item = T;

  fn next(&mut self) -> Option<Self::Item> {
    if self.current < self.range.end {
      let task = unsafe {
        self
          .queue
          .with_buffer_cell(|cell| (*cell).assume_init_read(), self.current & (N - 1))
      };

      self.current += 1;

      Some(task)
    } else {
      None
    }
  }
}

unsafe impl<'a, T, const N: usize> Send for BufferIter<'a, T, N> where
  T: Send + Sync + Sized + 'static
{
}
unsafe impl<'a, T, const N: usize> Sync for BufferIter<'a, T, N> where
  T: Send + Sync + Sized + 'static
{
}

impl<'a, T, const N: usize> Drop for BufferIter<'a, T, N>
where
  T: Send + Sync + Sized + 'static,
{
  fn drop(&mut self) {
    while self.next().is_some() {}
    self.deoccupy_buffer();
  }
}

impl<'a, T, const N: usize> IntoIterator for BoundedRange<'a, T, N>
where
  T: Send + Sync + Sized + 'static,
{
  type Item = T;
  type IntoIter = BufferIter<'a, T, N>;

  fn into_iter(self) -> Self::IntoIter {
    let iter = BufferIter {
      current: self.range.start,
      range: self.range.clone(),
      queue: self.queue,
    };

    mem::forget(self);

    iter
  }
}
