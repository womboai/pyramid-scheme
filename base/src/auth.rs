use core::slice;
use rusttensor::AccountId;

#[repr(C)]
pub struct KeyRegistrationInfo {
    pub uid: u16,

    // Account IDs have no alignment requirements, thus after u16 to ensure the best alignment and padding
    pub account_id: AccountId,
}

#[repr(C)]
pub struct VerificationMessage {
    pub nonce: u64,
    pub netuid: u16,

    pub miner: KeyRegistrationInfo,
    pub validator: KeyRegistrationInfo,
}

impl AsRef<[u8]> for VerificationMessage {
    fn as_ref(&self) -> &[u8] {
        // SAFETY: Safe as this is aligned with u8, and is repr(C)
        unsafe { slice::from_raw_parts(self as *const _ as *const u8, size_of::<Self>()) }
    }
}
