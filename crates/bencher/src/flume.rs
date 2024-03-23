use std::iter;

use criterion::{measurement::WallTime, BenchmarkGroup, BenchmarkId};
use flume::{Sender};
use futures::future::join_all;
use tokio::{runtime::Runtime, spawn, sync::oneshot};

fn make_reactor() -> Sender<(u64, oneshot::Sender<u64>)> {
  let (tx, rx) = flume::unbounded::<(u64, oneshot::Sender<u64>)>();

  spawn(async move {
    loop {
      if let Ok(task) = rx.recv_async().await {
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
    static QUEUE: Sender<(u64, oneshot::Sender<u64>)> = make_reactor();
  }

  let (tx, rx) = oneshot::channel();

  QUEUE.with(|queue_tx| {
    queue_tx.send((i, tx)).ok();
  });

  rx.await.unwrap()
}

pub fn bench_batching(rt: &Runtime, bench: &mut BenchmarkGroup<WallTime>, batch_size: u64) {
  bench.bench_with_input(
    BenchmarkId::new("flume", batch_size),
    &batch_size,
    |b, batch_size| {
      b.to_async(rt)
        .iter(|| join_all((0..*batch_size).map(push_echo)))
    },
  );
}
