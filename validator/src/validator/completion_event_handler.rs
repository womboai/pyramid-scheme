use crate::validator::metrics::ValidatorMetrics;
use crate::validator::neuron_data::{ConnectionState, NeuronData, Score};
use crate::validator::worker::{ProcessingCompletionResult, ProcessingCompletionState};
use std::ops::Range;
use std::sync::mpmc::{Receiver, Sender};
use std::time::Duration;
use tracing::{debug, info};

pub async fn handle_completion_events(
    neurons: &[NeuronData],
    completion_receiver: &Receiver<ProcessingCompletionResult>,
    work_queue_sender: &Sender<Range<u64>>,
    byte_count: u64,
    neuron_count: usize,
    metrics: &ValidatorMetrics,
) -> Vec<u8> {
    let mut time_per_uid_byte = Vec::new();
    let mut data_processed = 0;

    while data_processed < byte_count {
        let Ok(event) = completion_receiver.try_recv() else {
            tokio::time::sleep(Duration::from_millis(1)).await;
            continue;
        };

        let uid = event.uid;

        let score = neurons[uid as usize].score.get();

        match event.state {
            ProcessingCompletionState::Completed(processed, duration) => {
                debug!("Miner {uid} finished assigned work");

                data_processed += processed;

                unsafe {
                    *score += processed as u128;
                }

                time_per_uid_byte.push((uid, duration, processed));
            }
            ProcessingCompletionState::Failed(processed, remaining, mut connection, duration) => {
                info!("Miner {uid} failed at assigned work, disconnecting");

                *connection.guard = ConnectionState::Disconnected;
                metrics.connected_miners.add(-1, &[]);
                data_processed += processed;

                unsafe {
                    *score += processed as u128;
                }

                if let Some((_, d, p)) = time_per_uid_byte.iter_mut().find(|(id, _, _)| *id == uid) {
                    *d += duration;
                    *p += processed;
                } else {
                    time_per_uid_byte.push((uid, duration, processed));
                }

                work_queue_sender
                    .send(remaining)
                    .expect("Work queue channel should not be closed");
            }
            ProcessingCompletionState::Cheated(range, mut connection) => {
                info!("Miner {uid} marked as cheater");

                *connection.guard = ConnectionState::Unusable;
                metrics.connected_miners.add(-1, &[]);

                unsafe {
                    *score = Score::Cheater;
                }

                work_queue_sender
                    .send(range)
                    .expect("Work queue channel should not be closed");
            }
        }
    }

    let mut uid_times = Vec::new();

    let mut minimum_time = u128::MAX;
    let mut maximum_time = 0;

    for (uid, duration, processed) in time_per_uid_byte {
        if processed == 0 {
            continue
        }

        let time_per_byte = duration.as_nanos() / processed as u128;

        if time_per_byte < minimum_time {
            minimum_time = time_per_byte;
        }

        if time_per_byte > maximum_time {
            maximum_time = time_per_byte;
        }

        uid_times.push((uid, time_per_byte));
    }

    let range = maximum_time - minimum_time;

    let weights = vec![0u8; neuron_count];

    for (uid, time) in uid_times {
        if range == 0 {
            return (*uid, u8::MAX);
        }

        let contribution_time = u8::MAX as u128 * (*time - minimum_time);
        let inverse_weight = contribution_time / range;

        weights[*uid as usize] = u8::MAX - inverse_weight as u8;
    }

    weights
}
