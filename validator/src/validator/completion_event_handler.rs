use crate::validator::metrics::ValidatorMetrics;
use crate::validator::neuron_data::{ConnectionState, NeuronData, Score};
use crate::validator::worker::{
    ProcessingCompletionResult, ProcessingCompletionState, ProcessingRequest,
};
use std::net::TcpStream;
use std::sync::mpmc::{Receiver, Sender};
use tracing::{debug, info};

pub async fn handle_completion_events(
    neurons: &[NeuronData],
    available_worker_sender: &Sender<(u16, &'_ mut TcpStream)>,
    completion_receiver: &Receiver<ProcessingCompletionResult>,
    work_queue_sender: &Sender<ProcessingRequest>,
    byte_count: u64,
    metrics: &ValidatorMetrics,
) -> Vec<u8> {
    let mut time_per_uid_byte = Vec::new();
    let mut data_processed = 0;

    while data_processed < byte_count {
        let event = completion_receiver.recv().unwrap();

        let uid = event.uid;

        let score = neurons[uid as usize].score.get();

        match event.state {
            ProcessingCompletionState::Completed(processed, connection, duration) => {
                debug!("Miner {uid} finished assigned work");

                data_processed += processed;

                unsafe {
                    *score += processed as u128;
                }

                time_per_uid_byte.push((uid, duration, processed));

                available_worker_sender.send((uid, connection)).expect("");
            }
            ProcessingCompletionState::Failed(processed, remaining, duration) => {
                info!("Miner {uid} failed at assigned work, disconnecting");

                unsafe {
                    *neurons[uid as usize].connection.get() = ConnectionState::Disconnected;
                }

                metrics.connected_miners.add(-1, &[]);
                data_processed += processed;

                unsafe {
                    *score += processed as u128;
                }

                if let Some((_, d, p)) = time_per_uid_byte.iter_mut().find(|(id, _, _)| *id == uid)
                {
                    *d += duration;
                    *p += processed;
                } else {
                    time_per_uid_byte.push((uid, duration, processed));
                }

                work_queue_sender
                    .send(remaining)
                    .expect("Work queue channel should not be closed");
            }
            ProcessingCompletionState::Cheated(range) => {
                info!("Miner {uid} marked as cheater");

                unsafe {
                    *neurons[uid as usize].connection.get() = ConnectionState::Unusable;
                }

                metrics.connected_miners.add(-1, &[]);
                metrics.cheater_count.add(1, &[]);

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
            continue;
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

    let mut weights = vec![0u8; neurons.len()];

    for (uid, time) in uid_times {
        if range == 0 {
            weights[uid as usize] = u8::MAX;
            continue;
        }

        let contribution_time = u8::MAX as u128 * (time - minimum_time);
        let inverse_weight = contribution_time / range;

        weights[uid as usize] = u8::MAX - inverse_weight as u8;
    }

    weights
}
