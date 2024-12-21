use crate::validator::metrics::ValidatorMetrics;
use crate::validator::neuron_data::{ConnectionState, NeuronData};
use neuron::auth::{KeyRegistrationInfo, VerificationMessage};
use neuron::{config, SPEC_VERSION};
use rusttensor::rpc::types::NeuronInfoLite;
use rusttensor::sign::sign_message;
use rusttensor::wallet::Signer;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::time::Duration;
use tracing::{info, warn};

pub struct AvailableWorkerConnection {
    pub uid: u16,
    pub stream: &'static mut TcpStream,
    pub weight: u8,
}

pub fn connect_to_miner(
    signer: &Signer,
    uid: u16,
    neuron: &NeuronInfoLite,
    cheater: bool,
    metrics: &ValidatorMetrics,
) -> ConnectionState {
    if cheater {
        return ConnectionState::Unusable;
    }

    let ip: IpAddr = if neuron.axon_info.ip_type == 4 {
        Ipv4Addr::from(neuron.axon_info.ip as u32).into()
    } else {
        Ipv6Addr::from(neuron.axon_info.ip).into()
    };

    let address = SocketAddr::new(ip, neuron.axon_info.port);

    info!("Attempting to connect to {address}");

    match TcpStream::connect_timeout(&address, Duration::from_secs(5)) {
        Ok(mut stream) => {
            if let Err(e) = stream.set_read_timeout(Some(Duration::from_secs(5))) {
                warn!("Not continuing connection to uid {uid} at {address} because could not configure read timeout, {e}", uid = neuron.uid.0);

                return ConnectionState::Unusable;
            }

            if let Err(e) = stream.set_write_timeout(Some(Duration::from_secs(5))) {
                warn!("Not continuing connection to uid {uid} at {address} because could not configure write timeout, {e}", uid = neuron.uid.0);

                return ConnectionState::Unusable;
            }

            let message = VerificationMessage {
                nonce: 0,
                netuid: *config::NETUID,

                miner: KeyRegistrationInfo {
                    uid: neuron.uid.0,
                    account_id: neuron.hotkey.clone(),
                },

                validator: KeyRegistrationInfo {
                    uid,
                    account_id: signer.account_id().clone(),
                },
            };

            let signature = sign_message(signer, &message);

            if let Err(e) = stream.write((&message).as_ref()) {
                warn!("Failed to write to miner {uid}, {e}", uid = neuron.uid.0);

                return ConnectionState::Unusable;
            };

            if let Err(e) = stream.write(&signature) {
                warn!("Failed to write to miner {uid}, {e}", uid = neuron.uid.0);

                return ConnectionState::Unusable;
            }

            let mut version_buffer = [0u8; size_of::<u32>()];

            if let Err(e) = stream.read(&mut version_buffer) {
                warn!(
                    "Miner {uid} failed to report their version, {e}",
                    uid = neuron.uid.0
                );

                return ConnectionState::Unusable;
            }

            let version = u32::from_le_bytes(version_buffer);

            if version != SPEC_VERSION {
                warn!("Miner {uid} is using incorrect spec, expected {SPEC_VERSION} but got {version}", uid = neuron.uid.0);

                return ConnectionState::Unusable;
            }

            metrics.connected_miners.add(1, &[]);
            ConnectionState::connected(stream)
        }
        Err(error) => {
            warn!(
                "Couldn't connect to neuron {uid}, {error}",
                uid = neuron.uid.0
            );

            ConnectionState::Unusable
        }
    }
}

pub fn worker_count_hint(neurons: &mut [NeuronData]) -> usize {
    let mut count = 0;

    for x in neurons.iter_mut() {
        if matches!(x.connection.get_mut(), ConnectionState::Connected(_),) {
            count += 1;
        }
    }

    count
}

pub fn worker_connections(neurons: &mut [NeuronData]) -> Vec<AvailableWorkerConnection> {
    let mut weights = Vec::with_capacity(neurons.len());

    for neuron in neurons.iter_mut() {
        let connection = unsafe {
            // SAFETY: Safe as each connection is only used in one thread, according to the channel logic
            &mut *neuron.connection.get()
        };

        if let ConnectionState::Connected(stream) = connection {
            weights.push(AvailableWorkerConnection {
                uid: neuron.info.uid.0,
                stream,
                weight: neuron.weight.get(),
            });
        }
    }

    weights.sort_by(|x, y| x.weight.cmp(&y.weight).reverse());

    weights
}
