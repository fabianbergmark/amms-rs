use alloy::{
    primitives::{aliases::I56, U160, U256},
    sol,
    sol_types::SolValue,
};
use serde::{Deserialize, Serialize};

use super::{liquidity_math, util::require, UniswapV3Pool};
use crate::errors::AMMError;

sol! {
    #[derive(Debug, Deserialize, Serialize, Default, Copy, PartialEq, Eq)]
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

impl Tick {
    /// Decode from 128 bytes (4 storage slots) into Tick.
    pub fn decode_storage(data: &[U256]) -> Self {
        assert_eq!(data.len(), 4, "Expected exactly 128 bytes for Tick");

        let s0 = &data[0].to_be_bytes_vec();
        let s1 = &data[1].to_be_bytes_vec();
        let s2 = &data[2].to_be_bytes_vec();
        let s3 = &data[3].to_be_bytes_vec();

        // Slot 0: liquidity_gross (low 16B), liquidity_net (high 16B)
        let liquidity_gross = u128::from_be_bytes(s0[16..32].try_into().unwrap());
        let liquidity_net = i128::from_be_bytes(s0[0..16].try_into().unwrap());

        // Slot 1: fee_growth_outside_0_x128
        let fee_growth_outside_0_x128 = U256::from_be_slice(s1);

        // Slot 2: fee_growth_outside_1_x128
        let fee_growth_outside_1_x128 = U256::from_be_slice(s2);

        // Slot 3: packed tail
        let initialized = s3[0] != 0;
        let seconds_outside = u32::from_be_bytes(s3[1..5].try_into().unwrap());
        let seconds_per_liquidity_outside_x128 = U160::from_be_slice(&s3[5..25]);
        let tick_cumulative_outside = I56::try_from_be_slice(&s3[25..32]).unwrap();

        Tick {
            liquidity_gross,
            liquidity_net,
            fee_growth_outside_0_x128,
            fee_growth_outside_1_x128,
            tick_cumulative_outside,
            seconds_per_liquidity_outside_x128,
            seconds_outside,
            initialized,
        }
    }

    /// Encode Tick into 4 storage slots (big-endian, packed) using abi::encode_packed where possible.
    pub fn encode_storage(&self) -> [U256; 4] {
        // Slot 0: liquidity_net (high 16B) + liquidity_gross (low 16B)
        let s0_bytes = (self.liquidity_net, self.liquidity_gross).abi_encode_packed();
        let slot0 = U256::from_be_slice(&s0_bytes);

        // Slot 1
        let slot1 = self.fee_growth_outside_0_x128;

        // Slot 2
        let slot2 = self.fee_growth_outside_1_x128;

        // Slot 3: tick_cumulative_outside (7B) + seconds_per_liquidity_outside_x128 (20B) + seconds_outside (4B) + initialized (1B)
        let s3_bytes = (
            self.initialized,
            self.seconds_outside,
            self.seconds_per_liquidity_outside_x128,
            self.tick_cumulative_outside,
        )
            .abi_encode_packed();
        let slot3 = U256::from_be_slice(&s3_bytes);

        [slot0, slot1, slot2, slot3]
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
                info.seconds_per_liquidity_outside_x128 = seconds_per_liquidity_cumulative_x128;
                info.tick_cumulative_outside = tick_cumulative;
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
        info.seconds_per_liquidity_outside_x128 =
            seconds_per_liquidity_cumulative_x128 - info.seconds_per_liquidity_outside_x128;
        info.tick_cumulative_outside = tick_cumulative - info.tick_cumulative_outside;
        info.seconds_outside = time - info.seconds_outside;
        self.save_tick(tick, info);
        return info.liquidity_net;
    }
}
