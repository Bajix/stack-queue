#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(loom, allow(dead_code))]
#![allow(clippy::missing_transmute_annotations)]

#[macro_use(assert_cfg)]
extern crate static_assertions;
extern crate self as stack_queue;

assert_cfg!(not(target_pointer_width = "16"));

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
#[doc(hidden)]
pub use queue::BufferCell;
pub use queue::{BackgroundQueue, BatchReducer, LocalQueue, StackQueue, TaskQueue};
