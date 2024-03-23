# Stack Queue
![License](https://img.shields.io/badge/license-MIT-green.svg)
[![Cargo](https://img.shields.io/crates/v/stack-queue.svg)](https://crates.io/crates/stack-queue)
[![Documentation](https://docs.rs/stack-queue/badge.svg)](https://docs.rs/stack-queue)

A heapless auto-batching queue featuring deferrable batching by way of negotiating exclusive access over task ranges on thread-owned circular buffers. As tasks continue to be enqueued until batches are bounded, doing so can be deferred until after a database connection has been acquired as to allow for opportunitistic batching. This delivers optimal batching at all workload levels without batch collection overhead, superfluous timeouts, nor unnecessary allocations.

## Usage

Impl one of the following while using the [local_queue](https://docs.rs/stack-queue/latest/stack_queue/attr.local_queue.html) macro:

* [`TaskQueue`](https://docs.rs/stack-queue/latest/stack_queue/trait.TaskQueue.html), for batching with per-task receivers
* [`BackgroundQueue`](https://docs.rs/stack-queue/latest/stack_queue/trait.BackgroundQueue.html), for background processsing task batches without receivers
* [`BatchReducer`](https://docs.rs/stack-queue/latest/stack_queue/trait.BatchReducer.html), for collecting or reducing batched data

## Optimal Runtime Configuration

For best performance, exclusively use the Tokio runtime as configured via the [tokio::main](https://docs.rs/tokio/latest/tokio/attr.main.html) or [tokio::test](https://docs.rs/tokio/latest/tokio/attr.test.html) macro with the `crate` attribute set to `async_local` while the `barrier-protected-runtime` feature is enabled on [`async-local`](https://crates.io/crates/async-local). Doing so configures the Tokio runtime with a barrier that rendezvous runtime worker threads during shutdown in a way that ensures tasks never outlive thread local data owned by runtime worker threads and obviates the need for [Box::leak](https://doc.rust-lang.org/std/boxed/struct.Box.html#method.leak) as a means of lifetime extension.

## Benchmark results // batching 16 tasks


| `crossbeam`             | `flume`                        | `TaskQueue`                      | `BackgroundQueue`                | `tokio::mpsc`                   |
|:------------------------|:-------------------------------|:---------------------------------|:---------------------------------|:------------------------------- |
| `1.67 us` (✅ **1.00x**) | `1.95 us` (❌ *1.17x slower*)   | `942.72 ns` (✅ **1.77x faster**) | `638.45 ns` (🚀 **2.62x faster**) | `1.91 us` (❌ *1.14x slower*)    |
