use std::thread;

use futures::future::join_all;
use kanal::{unbounded, Sender};
use tokio::sync::oneshot;

fn make_reactor() -> Sender<(u64, oneshot::Sender<u64>)> {
  let (tx, rx) = unbounded::<(u64, oneshot::Sender<u64>)>();

  thread::spawn(move || loop {
    if let Ok((i, tx)) = rx.recv() {
      tx.send(i).ok();

      while let Ok(Some((i, tx))) = rx.try_recv() {
        tx.send(i).ok();
      }
    }
  });

  tx
}

pub async fn push_echo(i: u64) -> u64 {
  thread_local! {
    static QUEUE: Sender<(u64, oneshot::Sender<u64>)> = make_reactor();
  }

  let (tx, rx) = oneshot::channel();

  QUEUE.with(|queue| queue.send((i, tx)).ok());

  rx.await.unwrap()
}

pub async fn bench_batching(batch_size: &u64) {
  let batch: Vec<u64> = join_all((0..*batch_size).map(push_echo)).await;

  assert_eq!(batch, (0..*batch_size).collect::<Vec<u64>>())
}
