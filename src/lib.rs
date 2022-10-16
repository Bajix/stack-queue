#![allow(incomplete_features)]
#![feature(type_alias_impl_trait)]
#![feature(generic_const_exprs)]

mod assignment;
mod batch;
mod helpers;
mod queue;

pub use assignment::{CompletionReceipt, TaskAssignment};
pub use batch::{AutoBatch, TaskRef};
pub use queue::{StackQueue, TaskQueue};
