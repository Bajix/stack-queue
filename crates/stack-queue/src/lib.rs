#![allow(incomplete_features)]
#![feature(type_alias_impl_trait)]
#![feature(generic_const_exprs)]
extern crate self as stack_queue;

#[doc(hidden)]
pub extern crate static_assertions as sa;

#[doc(hidden)]
pub const MIN_BUFFER_LEN: usize = 256;

#[doc(hidden)]
#[cfg(target_pointer_width = "64")]
pub const MAX_BUFFER_LEN: usize = u32::MAX as usize;
#[doc(hidden)]
#[cfg(target_pointer_width = "32")]
pub const MAX_BUFFER_LEN: usize = u16::MAX as usize;

pub mod assignment;
mod helpers;
mod queue;
pub mod task;

pub use derive_stack_queue::LocalQueue;
pub use queue::{LocalQueue, StackQueue, TaskQueue};
