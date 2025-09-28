use alloy::primitives::U256;
use uniswap_v3_math::{full_math, sqrt_price_math::FIXED_POINT_96_RESOLUTION};

use crate::errors::AMMError;

use super::{liquidity_math, util::require};

#[derive(Debug, Clone, Default, Copy)]
pub struct Position {
    liquidity: u128,
    fee_growth_inside_0_last_x128: U256,
    fee_growth_inside_1_last_x128: U256,
    tokens_owed0: u128,
    tokens_owed1: u128,
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
            require(self.liquidity > 0, "NP");
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
            self.tokens_owed0 += u128::try_from(tokens_owed_0).unwrap();
            self.tokens_owed1 += u128::try_from(tokens_owed_1).unwrap();
        }

        Ok(())
    }
}
