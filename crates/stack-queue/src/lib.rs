#![cfg_attr(
  not(feature = "boxed"),
  feature(type_alias_impl_trait, impl_trait_in_assoc_type)
)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(loom, allow(dead_code))]

extern crate self as stack_queue;

#[doc(hidden)]
pub extern crate async_t;

#[doc(hidden)]
#[cfg(loom)]
pub extern crate loom;

const MIN_BUFFER_LEN: usize = 64;

#[cfg(target_pointer_width = "64")]
const MAX_BUFFER_LEN: usize = u32::MAX as usize;
#[cfg(target_pointer_width = "32")]
const MAX_BUFFER_LEN: usize = u16::MAX as usize;

pub mod assignment;
mod helpers;
mod queue;
pub mod task;

pub use derive_stack_queue::local_queue;
pub use queue::{BackgroundQueue, BatchReducer, BufferCell, LocalQueue, StackQueue, TaskQueue};
