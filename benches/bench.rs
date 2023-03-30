#![feature(type_alias_impl_trait)]

use std::{
  iter,
  time::{Duration, Instant},
};

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, SamplingMode};
use futures::future::join_all;
use stack_queue::{
  assignment::{CompletionReceipt, PendingAssignment, UnboundedRange},
  local_queue, BackgroundQueue, BatchReducer, ReducerExt, TaskQueue,
};
use tokio::{runtime::Builder, sync::oneshot};

#[global_allocator]
static GLOBAL: jemallocator::Jemalloc = jemallocator::Jemalloc;

struct AssignmentTimeQueue;

#[local_queue]
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

struct EnqueueTimeQueue;

#[local_queue]
impl TaskQueue for EnqueueTimeQueue {
  type Task = Instant;
  type Value = Duration;

  async fn batch_process<const N: usize>(
    batch: PendingAssignment<'async_trait, Self, N>,
  ) -> CompletionReceipt<Self> {
    let assignment = batch.into_assignment();
    let batched_at = Instant::now();

    assignment.map(|enqueued_at| batched_at.duration_since(enqueued_at))
  }
}

struct ReceiveTimeQueue;

#[local_queue]
impl TaskQueue for ReceiveTimeQueue {
  type Task = ();
  type Value = Instant;

  async fn batch_process<const N: usize>(
    batch: PendingAssignment<'async_trait, Self, N>,
  ) -> CompletionReceipt<Self> {
    let assignment = batch.into_assignment();
    let batched_at = Instant::now();

    assignment.resolve_with_iter(iter::repeat(batched_at))
  }
}

struct BackgroundTimerQueue;

#[local_queue]
impl BackgroundQueue for BackgroundTimerQueue {
  type Task = (Instant, oneshot::Sender<Duration>);

  async fn batch_process<const N: usize>(batch: UnboundedRange<'async_trait, Self::Task, N>) {
    let tasks = batch.into_bounded().into_iter();
    let collected_at = Instant::now();

    tasks.for_each(|(enqueued_at, tx)| {
      tx.send(collected_at.duration_since(enqueued_at)).unwrap();
    });
  }
}

struct Accumulator;

#[local_queue]
impl BatchReducer for Accumulator {
  type Task = usize;
}

fn criterion_benchmark(c: &mut Criterion) {
  let rt = Builder::new_current_thread().build().unwrap();

  let mut task_benches = c.benchmark_group("Task");
  task_benches.sampling_mode(SamplingMode::Linear);
  task_benches.warm_up_time(Duration::from_secs(1));
  task_benches.sample_size(10);

  task_benches.bench_function("enqueue", |bencher| {
    bencher.to_async(&rt).iter_custom(|iters| async move {
      let mut total = Duration::from_secs(0);

      for _i in 0..iters {
        total = total.saturating_add(EnqueueTimeQueue::auto_batch(Instant::now()).await);
      }

      total
    });
  });

  task_benches.bench_function("into_assignment", |bencher| {
    bencher.to_async(&rt).iter_custom(|iters| async move {
      let mut total = Duration::from_secs(0);

      for _i in 0..iters {
        total = total.saturating_add(AssignmentTimeQueue::auto_batch(()).await);
      }

      total
    });
  });

  task_benches.bench_function("receive value", |bencher| {
    bencher.to_async(&rt).iter_custom(|iters| async move {
      let mut total = Duration::from_secs(0);

      for _i in 0..iters {
        total = total.saturating_add(ReceiveTimeQueue::auto_batch(()).await.elapsed());
      }

      total
    });
  });

  task_benches.finish();

  let mut comparison_benches = c.benchmark_group("Tokio");
  comparison_benches.sampling_mode(SamplingMode::Linear);
  comparison_benches.warm_up_time(Duration::from_secs(1));
  comparison_benches.sample_size(10);

  comparison_benches.bench_function("oneshot receive", |bencher| {
    bencher.to_async(&rt).iter_custom(|iters| async move {
      let mut total = Duration::from_secs(0);

      for _i in 0..iters {
        let (tx, rx) = oneshot::channel();

        tokio::task::spawn(async move {
          tx.send(Instant::now()).ok();
        });

        total = total.saturating_add(rx.await.unwrap().elapsed());
      }

      total
    });
  });

  comparison_benches.finish();

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
      BenchmarkId::new("TaskQueue", batch_size),
      &batch_size,
      |b, batch_size| {
        b.to_async(&rt)
          .iter(|| echo_batched::stack_queue::bench_batching(batch_size))
      },
    );

    batching_benches.bench_with_input(
      BenchmarkId::new("BackgroundQueue", batch_size),
      &batch_size,
      |b, batch_size| {
        b.to_async(&rt).iter_custom(|iters| async move {
          let mut total = Duration::from_secs(0);

          for _ in 0..iters {
            let receivers: Vec<_> = (0..*batch_size)
              .map(|_| {
                let (tx, rx) = oneshot::channel();
                let enqueued_at = Instant::now();
                BackgroundTimerQueue::auto_batch((enqueued_at, tx));
                rx
              })
              .collect();

            tokio::task::yield_now().await;

            let rx = receivers.into_iter().next().unwrap();
            total = total.saturating_add(rx.await.unwrap());
          }

          total
        })
      },
    );

    batching_benches.bench_with_input(
      BenchmarkId::new("ReduceExt::batch_reduce", batch_size),
      &batch_size,
      |b, batch_size| {
        b.to_async(&rt).iter(|| {
          join_all(
            (0..*batch_size)
              .map(|i| Accumulator::batch_reduce(i as usize, |iter| iter.sum::<usize>())),
          )
        })
      },
    );

    batching_benches.bench_with_input(
      BenchmarkId::new("ReduceExt::batch_collect", batch_size),
      &batch_size,
      |b, batch_size| {
        b.to_async(&rt)
          .iter(|| join_all((0..*batch_size).map(|i| Accumulator::batch_collect(i as usize))))
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
