use alloy::primitives::U256;

use crate::errors::AMMError;

pub(crate) fn require(assertion: bool, message: &'static str) -> Result<(), AMMError> {
    if assertion {
        Ok(())
    } else {
        Err(AMMError::LogicError(message))
    }
}

pub(crate) fn to_u128(v: U256) -> u128 {
    let limbs = v.as_limbs();

    // limbs[0] = 0x0000000000000005 (the lowest 64 bits)
    // limbs[1] = 0x0000000000000000 (the next 64 bits)
    // limbs[2] = 0x0000000000000001 (the next 64 bits)
    // limbs[3] = 0x0000000000000000 (the highest 64 bits)

    // Combine the two lowest limbs to get the lower 128 bits
    (limbs[1] as u128) << 64 | (limbs[0] as u128)
}
