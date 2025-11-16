use alloy::primitives::U256;
use alloy::sol;
use serde::{Deserialize, Serialize};
use uniswap_v3_math::full_math;

use super::{liquidity_math, util::require};
use crate::amm::uniswap_v3::util::to_u128;
use crate::errors::AMMError;

sol! {
    #[derive(Debug, Default, Copy, Serialize, Deserialize, PartialEq, Eq)]
    struct Position {
        uint128 liquidity;
        uint256 fee_growth_inside_0_last_x128;
        uint256 fee_growth_inside_1_last_x128;
        uint128 tokens_owed0;
        uint128 tokens_owed1;
    }

}

impl Position {
    pub fn decode_storage(data: &[U256]) -> Self {
        assert_eq!(
            data.len(),
            4,
            "Expected exactly 128 bytes for a Position (4 storage slots)"
        );

        // Split into 4 32-byte slots (big-endian)
        let s0 = &data[0].to_be_bytes_vec();
        let s1 = &data[1].to_be_bytes_vec();
        let s2 = &data[2].to_be_bytes_vec();
        let s3 = &data[3].to_be_bytes_vec();

        // Slot 0: liquidity (uint128, low half)
        let liquidity = u128::from_be_bytes(s0[16..32].try_into().unwrap());

        // Slot 1: fee_growth_inside_0_last_x128
        let fee_growth_inside_0_last_x128 = U256::from_be_slice(s1);

        // Slot 2: fee_growth_inside_1_last_x128
        let fee_growth_inside_1_last_x128 = U256::from_be_slice(s2);

        // Slot 3: tokens_owed1 (high) + tokens_owed0 (low)
        let tokens_owed1 = u128::from_be_bytes(s3[0..16].try_into().unwrap());
        let tokens_owed0 = u128::from_be_bytes(s3[16..32].try_into().unwrap());

        Position {
            liquidity,
            fee_growth_inside_0_last_x128,
            fee_growth_inside_1_last_x128,
            tokens_owed0,
            tokens_owed1,
        }
    }

    pub fn encode_storage(&self) -> [U256; 4] {
        let mut s0 = [0u8; 32];
        s0[16..32].copy_from_slice(&self.liquidity.to_be_bytes());
        let slot0 = U256::from_be_slice(&s0);

        let slot1 = self.fee_growth_inside_0_last_x128;

        let slot2 = self.fee_growth_inside_1_last_x128;

        let mut s3 = [0u8; 32];
        s3[0..16].copy_from_slice(&self.tokens_owed1.to_be_bytes());
        s3[16..32].copy_from_slice(&self.tokens_owed0.to_be_bytes());
        let slot3 = U256::from_be_slice(&s3);

        [slot0, slot1, slot2, slot3]
    }

    pub(crate) fn update(
        &mut self,
        liquidity_delta: i128,
        fee_growth_inside_0_x128: U256,
        fee_growth_inside_1_x128: U256,
    ) -> Result<(), AMMError> {
        let liquidity_next;
        if liquidity_delta == 0 {
            require(self.liquidity > 0, "NP")?;
            liquidity_next = self.liquidity;
        } else {
            liquidity_next = liquidity_math::add_delta(self.liquidity, liquidity_delta)?;
        }

        // calculate accumulated fees
        let tokens_owed_0 = full_math::mul_div(
            fee_growth_inside_0_x128 - self.fee_growth_inside_0_last_x128,
            U256::from(self.liquidity),
            U256::ONE << 128,
        )?;

        let tokens_owed_1 = full_math::mul_div(
            fee_growth_inside_1_x128 - self.fee_growth_inside_1_last_x128,
            U256::from(self.liquidity),
            U256::ONE << 128,
        )?;

        // update the position
        if liquidity_delta != 0 {
            self.liquidity = liquidity_next
        }
        self.fee_growth_inside_0_last_x128 = fee_growth_inside_0_x128;
        self.fee_growth_inside_1_last_x128 = fee_growth_inside_1_x128;
        if tokens_owed_0 > U256::ZERO || tokens_owed_1 > U256::ZERO {
            self.tokens_owed0 = self.tokens_owed0.wrapping_add(to_u128(tokens_owed_0));
            self.tokens_owed1 = self.tokens_owed1.wrapping_add(to_u128(tokens_owed_1));
        }

        Ok(())
    }
}
