use std::time::Duration;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, SamplingMode};
use tokio::runtime::Builder;

#[global_allocator]
static GLOBAL: jemallocator::Jemalloc = jemallocator::Jemalloc;

fn criterion_benchmark(c: &mut Criterion) {
  let rt = Builder::new_current_thread().build().unwrap();

  let mut batching_tests = c.benchmark_group("Batching");
  batching_tests.sampling_mode(SamplingMode::Linear);
  batching_tests.warm_up_time(Duration::from_secs(1));
  batching_tests.sample_size(10);

  for n in 0..=6 {
    let batch_size: u64 = 1 << n;

    batching_tests.bench_with_input(
      BenchmarkId::new("crossbeam", batch_size),
      &batch_size,
      |b, batch_size| {
        b.to_async(&rt)
          .iter(|| echo_batched::crossbeam::bench_batching(batch_size))
      },
    );

    batching_tests.bench_with_input(
      BenchmarkId::new("flume", batch_size),
      &batch_size,
      |b, batch_size| {
        b.to_async(&rt)
          .iter(|| echo_batched::flume::bench_batching(batch_size))
      },
    );

    batching_tests.bench_with_input(
      BenchmarkId::new("kanal", batch_size),
      &batch_size,
      |b, batch_size| {
        b.to_async(&rt)
          .iter(|| echo_batched::kanal::bench_batching(batch_size))
      },
    );

    batching_tests.bench_with_input(
      BenchmarkId::new("stack-queue", batch_size),
      &batch_size,
      |b, batch_size| {
        b.to_async(&rt)
          .iter(|| echo_batched::stack_queue::bench_batching(batch_size))
      },
    );

    batching_tests.bench_with_input(
      BenchmarkId::new("swap-queue", batch_size),
      &batch_size,
      |b, batch_size| {
        b.to_async(&rt)
          .iter(|| echo_batched::swap_queue::bench_batching(batch_size))
      },
    );

    batching_tests.bench_with_input(
      BenchmarkId::new("tokio::mpsc", batch_size),
      &batch_size,
      |b, batch_size| {
        b.to_async(&rt)
          .iter(|| echo_batched::tokio::bench_batching(batch_size))
      },
    );
  }

  batching_tests.finish();
}

criterion_group!(benches, criterion_benchmark);
criterion_main!(benches);
