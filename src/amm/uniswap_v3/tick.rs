use alloy::{
    primitives::{aliases::I56, U160, U256},
    sol,
};
use serde::{Deserialize, Serialize};

use super::{liquidity_math, util::require, UniswapV3Pool};
use crate::errors::AMMError;

sol! {
    #[derive(Debug, Deserialize, Serialize, Default, Copy)]
    struct Tick {
        uint128 liquidity_gross;
        int128 liquidity_net;
        uint256 fee_growth_outside_0_x128;
        uint256 fee_growth_outside_1_x128;
        int56 tick_cumulative_outside;
        uint160 seconds_per_liquidity_outside_x128;
        uint32 seconds_outside;
        bool initialized;
    }
}

impl UniswapV3Pool {
    pub fn get_fee_growth_inside(
        &self,
        tick_lower: i32,
        tick_upper: i32,
        tick_current: i32,
        fee_growth_global_0_x128: U256,
        fee_growth_global_1_x128: U256,
    ) -> (U256, U256) {
        let lower = self.get_tick(tick_lower);
        let upper = self.get_tick(tick_upper);

        // calculate fee growth below
        let fee_growth_below_0_x128;
        let fee_growth_below_1_x128;

        if tick_current >= tick_lower {
            fee_growth_below_0_x128 = lower.fee_growth_outside_0_x128;
            fee_growth_below_1_x128 = lower.fee_growth_outside_1_x128;
        } else {
            fee_growth_below_0_x128 = fee_growth_global_0_x128 - lower.fee_growth_outside_0_x128;
            fee_growth_below_1_x128 = fee_growth_global_1_x128 - lower.fee_growth_outside_1_x128;
        }

        // calculate fee growth above
        let fee_growth_above_0_x128;
        let fee_growth_above_1_x128;

        if tick_current < tick_upper {
            fee_growth_above_0_x128 = upper.fee_growth_outside_0_x128;
            fee_growth_above_1_x128 = upper.fee_growth_outside_1_x128;
        } else {
            fee_growth_above_0_x128 = fee_growth_global_0_x128 - upper.fee_growth_outside_0_x128;
            fee_growth_above_1_x128 = fee_growth_global_1_x128 - upper.fee_growth_outside_1_x128;
        }

        let fee_growth_inside_0_x128 =
            fee_growth_global_0_x128 - fee_growth_below_0_x128 - fee_growth_above_0_x128;
        let fee_growth_inside_1_x128 =
            fee_growth_global_1_x128 - fee_growth_below_1_x128 - fee_growth_above_1_x128;

        return (fee_growth_inside_0_x128, fee_growth_inside_1_x128);
    }

    pub fn update_tick(
        &mut self,
        tick: i32,
        tick_current: i32,
        liquidity_delta: i128,
        fee_growth_global_0_x128: U256,
        fee_growth_global_1_x128: U256,
        seconds_per_liquidity_cumulative_x128: U160,
        tick_cumulative: I56,
        time: u32,
        upper: bool,
        max_liquidity: u128,
    ) -> Result<bool, AMMError> {
        let mut info = self.get_tick(tick);

        let liquidity_gross_before = info.liquidity_gross;
        let liquidity_gross_after =
            liquidity_math::add_delta(liquidity_gross_before, liquidity_delta)?;

        require(liquidity_gross_after <= max_liquidity, "LO")?;

        let flipped = (liquidity_gross_after == 0) != (liquidity_gross_before == 0);

        if liquidity_gross_before == 0 {
            // by convention, we assume that all growth before a tick was initialized happened _below_ the tick
            if tick <= tick_current {
                info.fee_growth_outside_0_x128 = fee_growth_global_0_x128;
                info.fee_growth_outside_1_x128 = fee_growth_global_1_x128;
                info.seconds_per_liquidity_outside_x128 =
                    seconds_per_liquidity_cumulative_x128.to();
                info.tick_cumulative_outside = tick_cumulative.try_into().unwrap();
                info.seconds_outside = time;
            }
            info.initialized = true;
        }

        info.liquidity_gross = liquidity_gross_after;

        info.liquidity_net = if upper {
            info.liquidity_net - liquidity_delta
        } else {
            info.liquidity_net + liquidity_delta
        };
        self.save_tick(tick, info);

        Ok(flipped)
    }

    pub(crate) fn cross_tick(
        &mut self,
        tick: i32,
        fee_growth_global_0_x128: U256,
        fee_growth_global_1_x128: U256,
        seconds_per_liquidity_cumulative_x128: U160,
        tick_cumulative: I56,
        time: u32,
    ) -> i128 {
        let mut info = self.get_tick(tick);
        info.fee_growth_outside_0_x128 = fee_growth_global_0_x128 - info.fee_growth_outside_0_x128;
        info.fee_growth_outside_1_x128 = fee_growth_global_1_x128 - info.fee_growth_outside_1_x128;
        info.seconds_per_liquidity_outside_x128 = seconds_per_liquidity_cumulative_x128
            .to::<U160>()
            - info.seconds_per_liquidity_outside_x128;
        info.tick_cumulative_outside =
            I56::unchecked_from(tick_cumulative) - info.tick_cumulative_outside;
        info.seconds_outside = time - info.seconds_outside;
        return info.liquidity_net;
    }
}
