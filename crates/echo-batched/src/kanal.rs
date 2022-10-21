use futures::future::join_all;
use kanal::{unbounded_async, AsyncSender};
use tokio::{runtime::Handle, sync::oneshot};

fn make_reactor() -> AsyncSender<(u64, oneshot::Sender<u64>)> {
  let (tx, rx) = unbounded_async::<(u64, oneshot::Sender<u64>)>();

  Handle::current().spawn(async move {
    loop {
      if let Ok((i, tx)) = rx.recv().await {
        tx.send(i).ok();

        while let Ok(Some((i, tx))) = rx.try_recv() {
          tx.send(i).ok();
        }
      }
    }
  });

  tx
}

pub async fn push_echo(i: u64) -> u64 {
  thread_local! {
    static QUEUE: AsyncSender<(u64, oneshot::Sender<u64>)> = make_reactor();
  }

  let (tx, rx) = oneshot::channel();

  QUEUE.with(|queue_tx| queue_tx.try_send((i, tx)).ok());

  rx.await.unwrap()
}

pub async fn bench_batching(batch_size: &u64) {
  let batch: Vec<u64> = join_all((0..*batch_size).map(push_echo)).await;

  assert_eq!(batch, (0..*batch_size).collect::<Vec<u64>>())
}
