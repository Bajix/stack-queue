use std::time::Duration;

use criterion::{criterion_group, criterion_main, Criterion, SamplingMode};
use tokio::runtime::Builder;

#[global_allocator]
static GLOBAL: jemallocator::Jemalloc = jemallocator::Jemalloc;

fn criterion_benchmark(c: &mut Criterion) {
  let rt = Builder::new_current_thread().build().unwrap();

  let mut task_benches = c.benchmark_group("Task");
  task_benches.sampling_mode(SamplingMode::Linear);
  task_benches.warm_up_time(Duration::from_secs(1));
  task_benches.sample_size(10);

  bencher::stack_queue::bench_tasks(&rt, &mut task_benches);
  bencher::tokio::bench_tasks(&rt, &mut task_benches);

  task_benches.finish();

  let mut batching_benches = c.benchmark_group("Batching");
  batching_benches.sampling_mode(SamplingMode::Linear);
  batching_benches.warm_up_time(Duration::from_secs(1));
  batching_benches.sample_size(10);

  for n in 0..=6 {
    let batch_size: u64 = 1 << n;

    bencher::crossbeam::bench_batching(&rt, &mut batching_benches, batch_size);
    bencher::flume::bench_batching(&rt, &mut batching_benches, batch_size);
    bencher::stack_queue::bench_batching(&rt, &mut batching_benches, batch_size);
    bencher::tokio::bench_batching(&rt, &mut batching_benches, batch_size);
  }

  batching_benches.finish();
}

criterion_group!(benches, criterion_benchmark);
criterion_main!(benches);
