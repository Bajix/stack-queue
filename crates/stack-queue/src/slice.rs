#[cfg(not(loom))]
use std::{
  cell::UnsafeCell,
  sync::atomic::{fence, Ordering},
};
use std::{
  marker::PhantomData,
  mem::{self, MaybeUninit},
  ops::{BitAnd, Deref, Range},
};

#[cfg(loom)]
use loom::{
  cell::UnsafeCell,
  sync::atomic::{fence, Ordering},
};

use crate::{
  helpers::{active_phase_bit, one_shifted},
  queue::{BackgroundQueue, Inner, INDEX_SHIFT},
};

pub struct UnboundedSlice<'a, T: BackgroundQueue, const N: usize> {
  base_slot: usize,
  queue_ptr: *const Inner<UnsafeCell<MaybeUninit<T::Task>>, N>,
  _phantom: PhantomData<&'a ()>,
}

impl<'a, T, const N: usize> UnboundedSlice<'a, T, N>
where
  T: BackgroundQueue,
{
  pub(crate) fn new(
    base_slot: usize,
    queue_ptr: *const Inner<UnsafeCell<MaybeUninit<T::Task>>, N>,
  ) -> Self {
    UnboundedSlice {
      base_slot,
      queue_ptr,
      _phantom: PhantomData,
    }
  }

  #[inline(always)]
  fn queue(&self) -> &Inner<UnsafeCell<MaybeUninit<T::Task>>, N> {
    // This can be safely dereferenced because Inner is immovable by design and will suspend it's
    // thread termination as necessary until no references exist
    unsafe { &*self.queue_ptr }
  }

  fn set_bounds(&self) -> Range<usize> {
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

  pub fn into_bounded(self) -> BoundedSlice<'a, T, N> {
    let task_range = self.set_bounds();
    let queue_ptr = self.queue_ptr;

    mem::forget(self);

    BoundedSlice::new(task_range, queue_ptr)
  }
}

impl<'a, T, const N: usize> Drop for UnboundedSlice<'a, T, N>
where
  T: BackgroundQueue,
{
  fn drop(&mut self) {
    let task_range = self.set_bounds();

    self
      .queue()
      .occupancy
      .fetch_sub(one_shifted::<N>(&task_range.start), Ordering::Relaxed);
  }
}

// This is safe because queue_ptr is guaranteed to be immovable and non-null while
// references exist
unsafe impl<'a, T, const N: usize> Send for UnboundedSlice<'a, T, N> where T: BackgroundQueue {}
unsafe impl<'a, T, const N: usize> Sync for UnboundedSlice<'a, T, N>
where
  T: BackgroundQueue,
  <T as BackgroundQueue>::Task: Sync,
{
}

// A guarded task range to a thread local queue.
pub struct BoundedSlice<'a, T: BackgroundQueue, const N: usize> {
  task_range: Range<usize>,
  queue_ptr: *const Inner<UnsafeCell<MaybeUninit<T::Task>>, N>,
  _phantom: PhantomData<&'a ()>,
}

impl<'a, T, const N: usize> BoundedSlice<'a, T, N>
where
  T: BackgroundQueue,
{
  fn new(
    task_range: Range<usize>,
    queue_ptr: *const Inner<UnsafeCell<MaybeUninit<T::Task>>, N>,
  ) -> Self {
    BoundedSlice {
      task_range,
      queue_ptr,
      _phantom: PhantomData,
    }
  }

  #[inline(always)]
  fn queue(&self) -> &Inner<UnsafeCell<MaybeUninit<T::Task>>, N> {
    // This can be safely dereferenced because Inner is immovable by design and will block it's
    // thread termination as necessary until no references exist
    unsafe { &*self.queue_ptr }
  }

  /// A slice of the bounded task range
  pub fn tasks(&self) -> &[T::Task] {
    unsafe { mem::transmute(self.queue().buffer.get_unchecked(self.task_range.clone())) }
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

    buffer
  }

  fn deoccupy_buffer(&self) {
    self
      .queue()
      .occupancy
      .fetch_sub(one_shifted::<N>(&self.task_range.start), Ordering::Relaxed);
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
    fence(Ordering::Release);
    self.deoccupy_buffer();
  }
}

// This is safe because queue_ptr is guaranteed to be immovable and non-null while
// references exist
unsafe impl<'a, T, const N: usize> Send for BoundedSlice<'a, T, N> where T: BackgroundQueue {}
unsafe impl<'a, T, const N: usize> Sync for BoundedSlice<'a, T, N>
where
  T: BackgroundQueue,
  <T as BackgroundQueue>::Task: Sync,
{
}
