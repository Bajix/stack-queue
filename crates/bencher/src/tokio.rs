use std::{
  iter,
  time::{Duration, Instant},
};

use criterion::{measurement::WallTime, BenchmarkGroup, BenchmarkId};
use futures::future::join_all;
use tokio::{
  runtime::Runtime,
  spawn,
  sync::{mpsc, oneshot},
};

fn make_reactor() -> mpsc::UnboundedSender<(u64, oneshot::Sender<u64>)> {
  let (tx, mut rx) = mpsc::unbounded_channel::<(u64, oneshot::Sender<u64>)>();

  spawn(async move {
    loop {
      if let Some(task) = rx.recv().await {
        iter::once(task)
          .chain(iter::from_fn(|| rx.try_recv().ok()))
          .for_each(|(i, tx)| {
            tx.send(i).ok();
          });
      }
    }
  });

  tx
}

async fn push_echo(i: u64) -> u64 {
  thread_local! {
    static QUEUE: mpsc::UnboundedSender<(u64, oneshot::Sender<u64>)> = make_reactor();
  }

  let (tx, rx) = oneshot::channel();

  QUEUE.with(|queue_tx| {
    queue_tx.send((i, tx)).ok();
  });

  rx.await.unwrap()
}

pub fn bench_tasks(rt: &Runtime, bench: &mut BenchmarkGroup<WallTime>) {
  bench.bench_function("Tokio::sync::oneshot receive", |bencher| {
    bencher.to_async(rt).iter_custom(|iters| async move {
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
}

pub fn bench_batching(rt: &Runtime, bench: &mut BenchmarkGroup<WallTime>, batch_size: u64) {
  bench.bench_with_input(
    BenchmarkId::new("tokio::mpsc", batch_size),
    &batch_size,
    |b, batch_size| {
      b.to_async(rt)
        .iter(|| join_all((0..*batch_size).map(push_echo)))
    },
  );
}
