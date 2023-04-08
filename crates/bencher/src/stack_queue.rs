use std::{
  iter,
  time::{Duration, Instant},
};

use criterion::{measurement::WallTime, BenchmarkGroup, BenchmarkId};
use futures::future::join_all;
use stack_queue::{
  assignment::{CompletionReceipt, PendingAssignment, UnboundedRange},
  local_queue, BackgroundQueue, BatchReducer, TaskQueue,
};
use tokio::{runtime::Runtime, sync::oneshot};

pub fn bench_tasks(rt: &Runtime, bench: &mut BenchmarkGroup<WallTime>) {
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

  bench.bench_function("enqueue", |bencher| {
    bencher.to_async(rt).iter_custom(|iters| async move {
      let mut total = Duration::from_secs(0);

      for _i in 0..iters {
        total = total.saturating_add(EnqueueTimeQueue::auto_batch(Instant::now()).await);
      }

      total
    });
  });

  bench.bench_function("into_assignment", |bencher| {
    bencher.to_async(rt).iter_custom(|iters| async move {
      let mut total = Duration::from_secs(0);

      for _i in 0..iters {
        total = total.saturating_add(AssignmentTimeQueue::auto_batch(()).await);
      }

      total
    });
  });

  bench.bench_function("receive value", |bencher| {
    bencher.to_async(rt).iter_custom(|iters| async move {
      let mut total = Duration::from_secs(0);

      for _i in 0..iters {
        total = total.saturating_add(ReceiveTimeQueue::auto_batch(()).await.elapsed());
      }

      total
    });
  });
}

pub fn bench_batching(rt: &Runtime, bench: &mut BenchmarkGroup<WallTime>, batch_size: u64) {
  struct EchoQueue;

  #[local_queue]
  impl TaskQueue for EchoQueue {
    type Task = u64;
    type Value = u64;

    async fn batch_process<const N: usize>(
      batch: PendingAssignment<'async_trait, Self, N>,
    ) -> CompletionReceipt<Self> {
      batch.into_assignment().map(|val| val)
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

  bench.bench_with_input(
    BenchmarkId::new("TaskQueue", batch_size),
    &batch_size,
    |b, batch_size| {
      b.to_async(rt).iter(|| async move {
        join_all((0..*batch_size).map(EchoQueue::auto_batch)).await;
      })
    },
  );

  bench.bench_with_input(
    BenchmarkId::new("BackgroundQueue", batch_size),
    &batch_size,
    |b, batch_size| {
      b.to_async(rt).iter_custom(|iters| async move {
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

  bench.bench_with_input(
    BenchmarkId::new("BatchReducer::batch_reduce", batch_size),
    &batch_size,
    |b, batch_size| {
      b.to_async(rt).iter(|| {
        join_all(
          (0..*batch_size)
            .map(|i| Accumulator::batch_reduce(i as usize, |iter| iter.sum::<usize>())),
        )
      })
    },
  );

  bench.bench_with_input(
    BenchmarkId::new("BatchReducer::batch_collect", batch_size),
    &batch_size,
    |b, batch_size| {
      b.to_async(rt)
        .iter(|| join_all((0..*batch_size).map(|i| Accumulator::batch_collect(i as usize))))
    },
  );
}
