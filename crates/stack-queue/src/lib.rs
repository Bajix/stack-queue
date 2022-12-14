#![cfg_attr(not(feature = "boxed"), feature(type_alias_impl_trait))]

assert_cfg!(all(
  not(all(
    feature = "tokio-runtime",
    feature = "async-std-runtime"
  )),
  any(feature = "tokio-runtime", feature = "async-std-runtime",)
));
extern crate self as stack_queue;

#[doc(hidden)]
pub extern crate async_t;

#[doc(hidden)]
#[cfg(loom)]
pub extern crate loom;

use static_assertions::assert_cfg;

const MIN_BUFFER_LEN: usize = 256;

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
