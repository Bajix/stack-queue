#![feature(type_alias_impl_trait)]

use std::{
  iter,
  time::{Duration, Instant},
};

use async_t::async_trait;
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, SamplingMode};
use stack_queue::{
  assignment::{CompletionReceipt, PendingAssignment},
  LocalQueue, TaskQueue,
};
use tokio::runtime::Builder;

#[global_allocator]
static GLOBAL: jemallocator::Jemalloc = jemallocator::Jemalloc;

#[derive(LocalQueue)]
pub struct AssignmentTimeQueue;

#[async_trait]
impl TaskQueue for AssignmentTimeQueue {
  type Task = ();
  type Value = Duration;

  async fn batch_process<const N: usize>(
    batch: PendingAssignment<'async_trait, Self, N>,
  ) -> CompletionReceipt<Self> {
    let start = Instant::now();
    let assignment = batch.into_assignment();
    let elapsed = start.elapsed();

    assignment.resolve_with_iter(iter::repeat(elapsed))
  }
}

fn criterion_benchmark(c: &mut Criterion) {
  let rt = Builder::new_current_thread().build().unwrap();

  let mut assignment_benches = c.benchmark_group("Task Collection");
  assignment_benches.sampling_mode(SamplingMode::Linear);
  assignment_benches.warm_up_time(Duration::from_secs(1));
  assignment_benches.sample_size(10);

  assignment_benches.bench_function("into_assignment", |bencher| {
    bencher.to_async(&rt).iter_custom(|iters| async move {
      let mut total = Duration::from_secs(0);

      for _i in 0..iters {
        total = total.saturating_add(AssignmentTimeQueue::auto_batch(()).await);
      }

      total
    });
  });

  assignment_benches.finish();

  let mut batching_benches = c.benchmark_group("Batching");
  batching_benches.sampling_mode(SamplingMode::Linear);
  batching_benches.warm_up_time(Duration::from_secs(1));
  batching_benches.sample_size(10);

  for n in 0..=6 {
    let batch_size: u64 = 1 << n;

    batching_benches.bench_with_input(
      BenchmarkId::new("crossbeam", batch_size),
      &batch_size,
      |b, batch_size| {
        b.to_async(&rt)
          .iter(|| echo_batched::crossbeam::bench_batching(batch_size))
      },
    );

    batching_benches.bench_with_input(
      BenchmarkId::new("flume", batch_size),
      &batch_size,
      |b, batch_size| {
        b.to_async(&rt)
          .iter(|| echo_batched::flume::bench_batching(batch_size))
      },
    );

    batching_benches.bench_with_input(
      BenchmarkId::new("stack-queue", batch_size),
      &batch_size,
      |b, batch_size| {
        b.to_async(&rt)
          .iter(|| echo_batched::stack_queue::bench_batching(batch_size))
      },
    );

    batching_benches.bench_with_input(
      BenchmarkId::new("swap-queue", batch_size),
      &batch_size,
      |b, batch_size| {
        b.to_async(&rt)
          .iter(|| echo_batched::swap_queue::bench_batching(batch_size))
      },
    );

    batching_benches.bench_with_input(
      BenchmarkId::new("tokio::mpsc", batch_size),
      &batch_size,
      |b, batch_size| {
        b.to_async(&rt)
          .iter(|| echo_batched::tokio::bench_batching(batch_size))
      },
    );
  }

  batching_benches.finish();
}

criterion_group!(benches, criterion_benchmark);
criterion_main!(benches);
