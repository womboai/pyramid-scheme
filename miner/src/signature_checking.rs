use neuron::auth::VerificationMessage;
use neuron::config;
use rusttensor::rpc::types::NeuronInfoLite;
use rusttensor::AccountId;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum IncorrectVerificationMessageInformation {
    #[error("Expected netuid {} but got {0}", *config::NETUID)]
    IncorrectNetuid(u16),

    #[error("Verification message reported miner hotkey as {reported} but it is {current}")]
    MismatchedHotkey {
        reported: AccountId,
        current: AccountId,
    },

    #[error("Verification message reported miner UID as {reported} but it is {current}")]
    MismatchedUid { reported: u16, current: u16 },

    #[error("Verification message reported validator UID as {uid} with hotkey {reported_hotkey}, but UID {uid} is associated with hotkey {expected_hotkey}")]
    MismatchedValidatorInfo {
        uid: u16,
        reported_hotkey: AccountId,
        expected_hotkey: AccountId,
    },
}

pub fn info_matches(
    message: &VerificationMessage,
    neurons: &[NeuronInfoLite],
    account_id: &AccountId,
    miner_uid: u16,
) -> Result<(), IncorrectVerificationMessageInformation> {
    if message.netuid != *config::NETUID {
        return Err(IncorrectVerificationMessageInformation::IncorrectNetuid(
            message.netuid,
        ));
    }

    if &message.miner.account_id != account_id {
        return Err(IncorrectVerificationMessageInformation::MismatchedHotkey {
            reported: message.miner.account_id.clone(),
            current: account_id.clone(),
        });
    }

    if message.miner.uid != miner_uid {
        return Err(IncorrectVerificationMessageInformation::MismatchedUid {
            reported: message.miner.uid,
            current: miner_uid,
        });
    }

    let expected_validator_hotkey = &neurons[message.validator.uid as usize].hotkey;
    if expected_validator_hotkey != &message.validator.account_id {
        return Err(
            IncorrectVerificationMessageInformation::MismatchedValidatorInfo {
                uid: message.validator.uid,
                reported_hotkey: message.validator.account_id.clone(),
                expected_hotkey: expected_validator_hotkey.clone(),
            },
        );
    }

    Ok(())
}
