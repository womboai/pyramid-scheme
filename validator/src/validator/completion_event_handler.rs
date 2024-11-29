use crate::validator::metrics::ValidatorMetrics;
use crate::validator::neuron_data::{ConnectionState, NeuronData, Score};
use crate::validator::worker::{ProcessingCompletionResult, ProcessingCompletionState};
use std::ops::Range;
use std::sync::mpmc::{Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;
use tracing::debug;

pub async fn handle_completion_events(
    neurons: &[NeuronData],
    completion_receiver: Receiver<ProcessingCompletionResult>,
    work_queue_sender: Sender<Range<u64>>,
    byte_count: u64,
    metrics: Arc<ValidatorMetrics>,
) {
    let mut data_processed = 0;

    while data_processed < byte_count {
        let Ok(event): Result<ProcessingCompletionResult, _> = completion_receiver.try_recv()
        else {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        };

        let uid = event.uid;

        let mut score = neurons[uid as usize].score.write().unwrap();

        match event.state {
            ProcessingCompletionState::Completed(processed) => {
                debug!("Miner {uid} finished assigned work");

                data_processed += processed;
                *score += processed as u128;
            }
            ProcessingCompletionState::Failed(processed, remaining, mut connection) => {
                debug!("Miner {uid} failed at assigned work");

                *connection.guard = ConnectionState::Disconnected;
                metrics.connected_miners.add(-1, &[]);
                data_processed += processed;
                *score += processed as u128;

                work_queue_sender
                    .send(remaining)
                    .expect("Work queue channel should not be closed");
            }
            ProcessingCompletionState::Cheated(range) => {
                debug!("Miner {uid} marked as cheater");

                *neurons[uid as usize].connection.lock().unwrap() = ConnectionState::Unusable;
                metrics.connected_miners.add(-1, &[]);
                *score = Score::Cheater;

                work_queue_sender
                    .send(range)
                    .expect("Work queue channel should not be closed");
            }
        }
    }
}
