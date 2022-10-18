use flume::{self, Sender};
use futures::future::join_all;
use tokio::{runtime::Handle, sync::oneshot};

fn make_reactor() -> Sender<(u64, oneshot::Sender<u64>)> {
  let (tx, rx) = flume::unbounded();

  Handle::current().spawn(async move {
    loop {
      if let Some(task) = rx.recv_async().await.ok() {
        let batch: Vec<(u64, oneshot::Sender<u64>)> = std::iter::once(task)
          .chain(std::iter::from_fn(|| rx.try_recv().ok()))
          .collect();

        batch.into_iter().for_each(|(i, tx)| {
          tx.send(i).ok();
        });
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

  QUEUE.with(|queue_tx| {
    queue_tx.send((i, tx)).ok();
  });

  rx.await.unwrap()
}

pub async fn bench_batching(batch_size: &u64) {
  let batch: Vec<u64> = join_all((0..*batch_size).map(|i| push_echo(i))).await;

  assert_eq!(batch, (0..*batch_size).collect::<Vec<u64>>())
}
