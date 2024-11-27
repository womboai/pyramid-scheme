use crate::validator::connection::find_suitable_connection;
use crate::validator::memory_storage::MemoryMappedStorage;
use crate::validator::metrics::ValidatorMetrics;
use crate::validator::neuron_data::{ConnectionGuard, NeuronData};
use crate::validator::CurrentRow;
use rusttensor::wallet::Signer;
use std::cmp::min;
use std::io::{Read, Write};
use std::ops::Range;
use std::random::random;
use std::sync::mpmc::{Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tracing::{debug, info, warn};

const VALIDATION_CHANCE: f32 = 0.3;

pub enum ProcessingCompletionState {
    Completed(u64),
    Failed(u64, Range<u64>, ConnectionGuard),
    Cheated(Range<u64>),
}

pub struct ProcessingCompletionResult {
    pub uid: u16,
    pub state: ProcessingCompletionState,
}

fn verify_result(original: &[u8], result: &[u8]) -> bool {
    let mut last = 0;

    for (i, x) in original.iter().enumerate() {
        let expected = x ^ (x << 1 | x << 2 | last >> 7 | last >> 6);

        if result[i] != expected {
            return false;
        }

        last = *x;
    }

    true
}

fn handle_connection(
    current_row: &mut MemoryMappedStorage,
    mut connection: ConnectionGuard,
    start: u64,
    end: u64,
    uid: u16,
    completion_events: Sender<ProcessingCompletionResult>,
    metrics: &ValidatorMetrics,
) {
    let buffer_size = min(end - start, 8 * 4 * 256);
    let mut buffer = Vec::with_capacity(buffer_size as usize);

    unsafe { buffer.set_len(buffer_size as usize) }

    let iterations = (end - start).div_ceil(buffer_size);

    let chance = if iterations > 1 {
        VALIDATION_CHANCE * iterations as f32 - VALIDATION_CHANCE.powi(iterations as i32)
    } else {
        VALIDATION_CHANCE
    };

    let validation_start_index = if random::<u16>() as f32 / u16::MAX as f32 <= chance {
        let t = random::<u16>() as f32 / u16::MAX as f32;
        let validation_start_index = ((end - start) as f32 * t) as u64;

        Some(validation_start_index)
    } else {
        None
    };

    let mut total_processed = 0;

    for i in 0..iterations {
        let mut processed = 0;
        let from = (start + i * buffer_size) as usize;
        let to = (start + (i + 1) * buffer_size) as usize;

        while processed as usize != to - from {
            let written = match connection.write(&current_row[from + processed as usize..to]) {
                Ok(len) => {
                    if len == 0 {
                        warn!(
                            "Failed to write to miner {uid} connection while there's more to process",
                        );

                        completion_events
                            .send(ProcessingCompletionResult {
                                uid,
                                state: ProcessingCompletionState::Failed(
                                    total_processed,
                                    start + total_processed..end,
                                    connection,
                                ),
                            })
                            .unwrap();

                        return;
                    }

                    len
                }
                Err(error) => {
                    warn!("Error occurred writing to miner {uid}: {error}");

                    completion_events
                        .send(ProcessingCompletionResult {
                            uid,
                            state: ProcessingCompletionState::Failed(
                                total_processed,
                                start + total_processed..end,
                                connection,
                            ),
                        })
                        .unwrap();

                    return;
                }
            };

            let mut read = 0;

            while read < written {
                let range = from + read + processed as usize..from + written + processed as usize;

                let should_validate = if let Some(validation_start_index) = validation_start_index {
                    if range.contains(&(validation_start_index as usize)) {
                        true
                    } else {
                        false
                    }
                } else {
                    false
                };

                let len = match connection.read(&mut buffer[0..range.len()]) {
                    Ok(len) => {
                        if len == 0 {
                            warn!("Failed to read from miner {uid} connection");

                            completion_events
                                .send(ProcessingCompletionResult {
                                    uid,
                                    state: ProcessingCompletionState::Failed(
                                        total_processed,
                                        start + total_processed..end,
                                        connection,
                                    ),
                                })
                                .unwrap();

                            return;
                        }

                        len
                    }
                    Err(error) => {
                        warn!("Error occurred reading from miner {uid}: {error}");

                        completion_events
                            .send(ProcessingCompletionResult {
                                uid,
                                state: ProcessingCompletionState::Failed(
                                    total_processed,
                                    start + total_processed..end,
                                    connection,
                                ),
                            })
                            .unwrap();

                        return;
                    }
                };

                let read_chunk_range = range.start..range.start + len;

                if should_validate {
                    info!("Verifying results of {uid}");
                    if !verify_result(&current_row[range.start..range.start + len], &buffer[..len])
                    {
                        info!("{uid} marked as cheater");
                        metrics.cheater_count.add(1, &[]);

                        completion_events
                            .send(ProcessingCompletionResult {
                                uid,
                                state: ProcessingCompletionState::Cheated(start..end),
                            })
                            .unwrap();

                        return;
                    }
                }

                (&mut current_row[read_chunk_range]).copy_from_slice(&buffer[..len]);
                read += len;
            }

            processed += written as u64;
        }

        total_processed += processed;
    }

    completion_events
        .send(ProcessingCompletionResult {
            uid,
            state: ProcessingCompletionState::Completed(total_processed),
        })
        .unwrap();
}

pub fn do_work(
    current_row: &CurrentRow,
    neurons: &Vec<NeuronData>,
    work_queue_receiver: Receiver<Range<u64>>,
    completion_sender: Sender<ProcessingCompletionResult>,
    metrics: Arc<ValidatorMetrics>,
) {
    loop {
        let Ok(range) = work_queue_receiver.try_recv() else {
            thread::sleep(Duration::from_millis(100));
            continue;
        };

        debug!("Finding suitable miner for {range:?}");

        let (uid, connection) = find_suitable_connection(neurons);

        debug!("Assigned {range:?} to miner {uid}");

        handle_connection(
            unsafe { current_row.get() },
            connection,
            range.start,
            range.end,
            uid,
            completion_sender.clone(),
            &metrics,
        );
    }
}
