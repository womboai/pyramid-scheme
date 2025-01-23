use crate::validator::metrics::ValidatorMetrics;
use crate::validator::neuron_data::{NeuronData, Rate};
use crate::validator::worker::{
    ProcessingCompletionResult, ProcessingCompletionState, ProcessingRequest,
};
use std::net::TcpStream;
use std::sync::mpmc::{Receiver, Sender};
use tracing::{debug, info};

pub async fn handle_completion_events(
    neurons: &mut [NeuronData],
    available_worker_sender: &Sender<(u16, &'_ mut TcpStream)>,
    completion_receiver: &Receiver<ProcessingCompletionResult>,
    work_queue_sender: &Sender<ProcessingRequest>,
    byte_count: u64,
    metrics: &ValidatorMetrics,
) {
    let mut data_processed = 0;

    while data_processed < byte_count {
        let event = completion_receiver.recv().unwrap();

        let uid = event.uid;

        match event.state {
            ProcessingCompletionState::Completed(processed, connection, duration) => {
                debug!("Miner {uid} finished assigned work");

                data_processed += processed;

                neurons[uid as usize].rate += processed as f64 / duration.as_secs_f64();

                available_worker_sender
                    .send((uid, connection))
                    .expect("Available worker channel should not be closed");
            }
            ProcessingCompletionState::Failed(processed, remaining, duration) => {
                info!("Miner {uid} failed at assigned work, disconnecting");

                unsafe {
                    *neurons[uid as usize].connection.get() = None;
                }

                metrics.connected_miners.add(-1, &[]);
                data_processed += processed;

                neurons[uid as usize].rate += processed as f64 / duration.as_secs_f64();

                work_queue_sender
                    .send(remaining)
                    .expect("Work queue channel should not be closed");
            }
            ProcessingCompletionState::Cheated(range) => {
                info!("Miner {uid} marked as cheater");

                unsafe {
                    *neurons[uid as usize].connection.get() = None;
                }

                metrics.connected_miners.add(-1, &[]);
                metrics.cheater_count.add(1, &[]);

                neurons[uid as usize].rate = Rate::Cheater;

                work_queue_sender
                    .send(range)
                    .expect("Work queue channel should not be closed");
            }
        }
    }
}
