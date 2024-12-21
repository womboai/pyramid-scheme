use neuron::ProcessingNetworkRequest;
use std::cell::SyncUnsafeCell;
use std::cmp::min;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::ops::Range;
use std::random::random;
use std::sync::mpmc::{Receiver, Sender};
use std::time::{Duration, Instant};
use std::slice;
use tracing::{debug, warn};

const VALIDATION_CHANCE: f32 = 0.05;

#[derive(Debug)]
pub struct ProcessingRequest {
    pub range: Range<u64>,
    pub last_byte: u8,
}

impl ProcessingRequest {
    pub fn new(range: Range<u64>, row: &[u8]) -> Self {
        let last_byte = if range.start == 0 {
            0
        } else {
            row[range.start as usize - 1]
        };

        Self { range, last_byte }
    }
}

pub enum ProcessingCompletionState {
    Completed(u64, &'static mut TcpStream, Duration),
    Failed(u64, ProcessingRequest, Duration),
    Cheated(ProcessingRequest),
}

pub struct ProcessingCompletionResult {
    pub uid: u16,
    pub state: ProcessingCompletionState,
}

fn verify_result(original: &[u8], mut last: u8, result: &[u8]) -> bool {
    for (i, x) in original.iter().enumerate() {
        let expected = x ^ (x << 1 | x << 2 | last >> 7 | last >> 6);

        if result[i] != expected {
            return false;
        }

        last = *x;
    }

    true
}

fn read_len(
    stream: &mut TcpStream,
    buffer: &mut [u8],
    output: &mut [u8],
    length: usize,
    mut last: u8,
    uid: u16,
) -> Option<usize> {
    let mut read = 0;

    while read < length {
        let len = match stream.read(&mut buffer[..length - read]) {
            Ok(len) => {
                if len == 0 {
                    warn!("Failed to read from miner {uid} connection");

                    return Some(read);
                }

                len
            }
            Err(error) => {
                warn!("Error occurred reading from miner {uid}: {error}");

                return Some(read);
            }
        };

        if random::<u16>() as f32 / (u16::MAX as f32) < VALIDATION_CHANCE {
            debug!("Verifying results of {uid}");

            if !verify_result(&output[read..read + len], last, &buffer[..len]) {
                return None;
            }
        }

        last = output[read + len - 1];

        (&mut output[read..read + len]).copy_from_slice(&buffer[..len]);

        read += len;
    }

    Some(read)
}

fn handle_connection(
    current_row: &mut [u8],
    connection: &'static mut TcpStream,
    request: ProcessingRequest,
    uid: u16,
) -> ProcessingCompletionState {
    let buffer_size = 8 * 4 * 1024;

    let mut buffer = Vec::with_capacity(buffer_size);

    unsafe { buffer.set_len(buffer_size) }

    let iterations = current_row.len().div_ceil(buffer_size);

    let mut last = request.last_byte;

    let network_request = ProcessingNetworkRequest {
        length: current_row.len() as u64,
        last_byte: last,
    };

    let network_request = unsafe {
        slice::from_raw_parts(
            &network_request as *const _ as *const u8,
            size_of_val(&network_request),
        )
    };

    if let Err(e) = connection.write(network_request) {
        warn!("Failed to request data processing from miner {uid}, {e}");

        return ProcessingCompletionState::Failed(0, request, Duration::new(0, 0));
    };

    let mut total_processed = 0;

    let time_started = Instant::now();

    for i in 0..iterations {
        let mut processed = 0;
        let from = i * buffer_size;
        let to = min((i + 1) * buffer_size, current_row.len());

        while processed != to - from {
            let write_from = from + processed;

            let written = match connection.write(&current_row[write_from..to]) {
                Ok(len) => {
                    if len == 0 {
                        warn!(
                            "Failed to write to miner {uid} connection while there's more to process",
                        );

                        return ProcessingCompletionState::Failed(
                            total_processed,
                            ProcessingRequest {
                                range: request.range.start + total_processed..request.range.end,
                                last_byte: last,
                            },
                            time_started.elapsed(),
                        );
                    }

                    len
                }
                Err(error) => {
                    warn!("Error occurred writing to miner {uid}: {error}");

                    return ProcessingCompletionState::Failed(
                        total_processed,
                        ProcessingRequest {
                            range: request.range.start + total_processed..request.range.end,
                            last_byte: last,
                        },
                        time_started.elapsed(),
                    );
                }
            };

            let last_byte = current_row[write_from + written - 1];

            let read = read_len(
                connection,
                &mut buffer,
                &mut current_row[write_from..write_from + written],
                written,
                last,
                uid,
            );

            let Some(read) = read else {
                return ProcessingCompletionState::Cheated(request);
            };

            last = last_byte;

            processed += read;
            total_processed += read as u64;

            if read < written {
                return ProcessingCompletionState::Failed(
                    total_processed,
                    ProcessingRequest {
                        range: request.range.start + total_processed..request.range.end,
                        last_byte: last,
                    },
                    time_started.elapsed(),
                );
            }
        }
    }

    ProcessingCompletionState::Completed(total_processed, connection, time_started.elapsed())
}

pub fn do_work(
    current_row: &SyncUnsafeCell<Vec<u8>>,
    available_worker_receiver: Receiver<(u16, &'static mut TcpStream)>,
    work_queue_receiver: Receiver<ProcessingRequest>,
    completion_sender: Sender<ProcessingCompletionResult>,
) {
    loop {
        let request = work_queue_receiver.recv().unwrap();

        debug!("Finding suitable miner for {request:?}");

        let (uid, connection) = available_worker_receiver.recv().unwrap();

        debug!("Assigned {request:?} to miner {uid}");

        let current_row = unsafe {
            slice::from_raw_parts_mut(
                (*current_row.get())
                    .as_mut_ptr()
                    .add(request.range.start as usize),
                (request.range.end - request.range.start) as usize,
            )
        };

        let state = handle_connection(current_row, connection, request, uid);

        completion_sender
            .send(ProcessingCompletionResult { state, uid })
            .expect("Completion event channel should not be closed");
    }
}
