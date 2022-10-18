use crossbeam_deque::{Steal, Worker};
use futures::future::join_all;
use tokio::{
  runtime::Handle,
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

      Handle::current().spawn(async move {
        let batch: Vec<(u64, Sender<u64>)> = std::iter::from_fn(|| loop {
          match stealer.steal() {
            Steal::Success(task) => break Some(task),
            Steal::Retry => continue,
            Steal::Empty => break None,
          }
        })
        .collect();

        batch.into_iter().for_each(|(i, tx)| {
          tx.send(i).ok();
        });
      });
    }

    queue.push((i, tx));
  });

  rx.await.unwrap()
}

pub async fn bench_batching(batch_size: &u64) {
  let batch: Vec<u64> = join_all((0..*batch_size).map(|i| push_echo(i))).await;

  assert_eq!(batch, (0..*batch_size).collect::<Vec<u64>>())
}
