#![allow(incomplete_features)]
#![feature(type_alias_impl_trait)]
#![feature(generic_const_exprs)]

pub mod assignment;
mod helpers;
mod queue;
pub mod task;

pub use queue::{StackQueue, TaskQueue};
