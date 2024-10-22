#![feature(portable_simd)]

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::simd::{ToBytes, u64x4, u8x32};
use log::{error, info};
use threadpool::ThreadPool;

#[derive(Default)]
struct Solver {
    last: u64,
}

impl Solver {
    fn new() -> Self {
        Solver::default()
    }

    fn solve(&mut self, data: &mut [u8]) {
        let step = size_of::<u64x4>();

        for i in (0..data.len()).step_by(step) {
            let chunk = u64x4::from_le_bytes(u8x32::from_slice(&data[i..i + step]));
            let mut modified_chunk = u64x4::from([0u64; 4]);

            for j in 0..4 {
                let x = chunk[j];

                modified_chunk[j] = x << 1 | x << 2 | self.last >> 63 | self.last >> 62;

                self.last = x;
            }

            data[i..i + step].copy_from_slice((chunk ^ modified_chunk).to_le_bytes().as_array());
        }
    }
}

fn handle_connection(mut stream: TcpStream, address: SocketAddr) {
    info!("Validator {address} has connected");

    let mut buffer = Vec::with_capacity(16384);
    let mut solver = Solver::new();

    unsafe {
        buffer.set_len(buffer.capacity());
    }

    loop {
        let len = match stream.read(&mut buffer) {
            Ok(len) => len,
            Err(error) => {
                error!("Failed to read from validator {address}, {error}");
                return;
            }
        };

        if len == 0 {
            break;
        }

        solver.solve(&mut buffer);
        match stream.write(&buffer) {
            Ok(len) => {
                if len == 0 {
                    error!("Validator {address}'s connection does not appear to be writable");
                }
            }
            Err(error) => {
                error!("Failed to write to validator {address}, {error}");
                return;
            }
        }
    }
}

fn main() {
    let ip: Ipv4Addr = [0u8, 0, 0, 0].into();
    let listener = TcpListener::bind((ip, 8129u16)).unwrap();
    let pool = ThreadPool::new(32);

    listener.set_nonblocking(true).unwrap();

    loop {
        if let Ok((stream, address)) = listener.accept() {
            pool.execute(move || handle_connection(stream, address));
        }
    }
}
