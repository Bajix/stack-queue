use std::thread::LocalKey;

use async_t::async_trait;
use futures::future::join_all;
use stack_queue::{CompletionReceipt, PendingAssignment, StackQueue, TaskQueue};

struct EchoQueue;

#[async_trait]
impl TaskQueue for EchoQueue {
  type Task = u64;
  type Value = u64;

  fn queue() -> &'static LocalKey<StackQueue<Self>> {
    thread_local! {
      static QUEUE: StackQueue<EchoQueue> = StackQueue::new();
    }

    &QUEUE
  }

  async fn batch_process(batch: PendingAssignment<Self>) -> CompletionReceipt<Self> {
    batch.into_assignment().map(|val| val)
  }
}

pub async fn push_echo(i: u64) -> u64 {
  EchoQueue::auto_batch(i).await
}

pub async fn bench_batching(batch_size: &u64) {
  let batch: Vec<u64> = join_all((0..*batch_size).map(push_echo)).await;

  assert_eq!(batch, (0..*batch_size).collect::<Vec<u64>>());
}
