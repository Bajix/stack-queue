#![allow(incomplete_features)]
#![feature(type_alias_impl_trait)]
#![feature(generic_const_exprs)]
extern crate self as stack_queue;

pub mod assignment;
mod helpers;
mod queue;
pub mod task;

pub use derive_stack_queue::LocalQueue;
pub use queue::{LocalQueue, StackQueue, TaskQueue};
