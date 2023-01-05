# Stack Queue
![License](https://img.shields.io/badge/license-MIT-green.svg)
[![Cargo](https://img.shields.io/crates/v/stack-queue.svg)](https://crates.io/crates/stack-queue)
[![Documentation](https://docs.rs/stack-queue/badge.svg)](https://docs.rs/stack-queue)

## Performance oriented heapless auto batching queue

By utilizing a barrier to guard thread local data until runtime threads rendezvous during shutdown, it becomes possible to create thread-safe pointers to thread-local data owned by runtime worker threads and in doing so this facilitates heapless allocations where otherwise an `std::sync::Arc` would be necessitated (see [async-local](https://crates.io/crates/async-local) for more details). Building on top of barrier-protected thread-owned buffers, [stack-queue](https://crates.io/crates/stack-queue) delivers a set of algorithms for asynchronously batching tasks in a way that allows for constant-time deferrable take-all batch collection. By using fixed-size circular buffers, writes index without atomic synchronization and write confirmations instead synchronized to observe interlaced batch boundaries under a data-race free relaxed atomic model. Not only does this cut in half the number of atomic calls per write operation, but this creates an asymmetrical performance profile with task ranges bounded by a single atomic operation per batch, making this ideal for use-cases in which it's desireable for batches to grow while awaiting resources, such as for deferring batch bounding until after a database connection is available for a batched query. As push operations enqueue within an async context, receivers are already pinned and can be written to directly without heap allocation and should the current threads queue be full, work-stealing will redistribute tasks.

## Benchmark results // batching 16 tasks

| `crossbeam`             | `flume`                        | `stack-queue::TaskQueue`          | `stack-queue::BackgroundQueue`          | `tokio::mpsc`                   |
|:------------------------|:-------------------------------|:----------------------------------|:----------------------------------------|:------------------------------- |
| `1.74 us` (✅ **1.00x**) | `2.01 us` (❌ *1.16x slower*)   | `974.99 ns` (✅ **1.78x faster**)  | `644.55 ns` (🚀 **2.69x faster**)        | `1.96 us` (❌ *1.13x slower*)    |

---

## Stable Usage

This crate conditionally makes use of the nightly only feature [type_alias_impl_trait](https://rust-lang.github.io/rfcs/2515-type_alias_impl_trait.html) to allow async fns in traits to be unboxed. To compile on `stable` the `boxed` feature flag can be used to downgrade [async_t::async_trait](https://docs.rs/async_t/latest/async_t/attr.async_trait.html) to [async_trait::async_trait](https://docs.rs/async-trait/latest/async_trait).
