use futures::future::join_all;
use stack_queue::{
  assignment::{CompletionReceipt, PendingAssignment},
  local_queue, TaskQueue,
};

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

pub async fn push_echo(i: u64) -> u64 {
  EchoQueue::auto_batch(i).await
}

pub async fn bench_batching(batch_size: &u64) {
  let batch: Vec<u64> = join_all((0..*batch_size).map(push_echo)).await;

  assert_eq!(batch, (0..*batch_size).collect::<Vec<u64>>());
}
