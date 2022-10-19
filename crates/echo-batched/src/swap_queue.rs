use futures::future::join_all;
use swap_queue::Worker;
use tokio::{
  runtime::Handle,
  sync::oneshot::{channel, Sender},
};

thread_local! {
  static QUEUE: Worker<(u64, Sender<u64>)> = Worker::new();
}

pub async fn push_echo(i: u64) -> u64 {
  {
    let (tx, rx) = channel();

    QUEUE.with(|queue| {
      if let Some(stealer) = queue.push((i, tx)) {
        Handle::current().spawn(async move {
          let batch = stealer.take().await;

          batch.into_iter().for_each(|(i, tx)| {
            tx.send(i).ok();
          });
        });
      }
    });

    rx
  }
  .await
  .unwrap()
}

pub async fn bench_batching(batch_size: &u64) {
  let batch: Vec<u64> = join_all((0..*batch_size).map(push_echo)).await;

  assert_eq!(batch, (0..*batch_size).collect::<Vec<u64>>());
}
