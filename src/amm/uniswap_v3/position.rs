use alloy::primitives::U256;
use alloy::sol;
use serde::{Deserialize, Serialize};
use uniswap_v3_math::full_math;

use super::{liquidity_math, util::require};
use crate::amm::uniswap_v3::util::to_u128;
use crate::errors::AMMError;

sol! {
    #[derive(Debug, Default, Copy, Serialize, Deserialize)]
    struct Position {
        uint128 liquidity;
        uint256 fee_growth_inside_0_last_x128;
        uint256 fee_growth_inside_1_last_x128;
        uint128 tokens_owed0;
        uint128 tokens_owed1;
    }

}

impl Position {
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
