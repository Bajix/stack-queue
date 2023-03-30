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

pub async fn bench_batching(batch_size: &u64) {
  join_all((0..*batch_size).map(EchoQueue::auto_batch)).await;
}
