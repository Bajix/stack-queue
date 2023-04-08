use std::iter;

use criterion::{measurement::WallTime, BenchmarkGroup, BenchmarkId};
use crossbeam_deque::{Steal, Worker};
use futures::future::join_all;
use tokio::{
  runtime::Runtime,
  spawn,
  sync::oneshot::{channel, Sender},
};

thread_local! {
  static QUEUE: Worker<(u64, Sender<u64>)> = Worker::new_fifo();
}

pub async fn push_echo(i: u64) -> u64 {
  let (tx, rx) = channel();

  QUEUE.with(|queue| {
    // crossbeam_deque::Worker could be patched to return slot written, so we're going to give this
    // the benefit of that potential optimization
    if i.eq(&0) {
      let stealer = queue.stealer();

      spawn(async move {
        iter::from_fn(|| loop {
          match stealer.steal() {
            Steal::Success(task) => break Some(task),
            Steal::Retry => continue,
            Steal::Empty => break None,
          }
        })
        .for_each(|(i, tx)| {
          tx.send(i).ok();
        });
      });
    }

    queue.push((i, tx));
  });

  rx.await.unwrap()
}

pub fn bench_batching(rt: &Runtime, bench: &mut BenchmarkGroup<WallTime>, batch_size: u64) {
  bench.bench_with_input(
    BenchmarkId::new("crossbeam", batch_size),
    &batch_size,
    |b, batch_size| {
      b.to_async(rt)
        .iter(|| join_all((0..*batch_size).map(push_echo)))
    },
  );
}
