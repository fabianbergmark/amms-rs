use std::collections::hash_map::Entry;

use alloy::{
    primitives::{
        aliases::{I24, I56},
        keccak256,
        map::HashMap,
        U160, U256,
    },
    sol_types::SolValue,
};
use serde::{Deserialize, Serialize};

use crate::errors::AMMError;

use super::{liquidity_math, util::require};
#[derive(Debug, Deserialize, Serialize, Default, Clone)]
pub struct Ticks {
    pub ticks: HashMap<U256, Tick>,
}

impl Ticks {
    pub fn entry(&mut self, key: i32) -> Entry<'_, U256, Tick> {
        let slot = keccak256((I24::try_from(key).unwrap(), U256::from(5)).abi_encode());
        let slot: U256 = slot.into();
        self.ticks.entry(slot)
    }

    pub fn get(&self, key: &i32) -> Option<&Tick> {
        let slot = keccak256((I24::try_from(*key).unwrap(), U256::from(5)).abi_encode());
        let slot: U256 = slot.into();
        self.ticks.get(&slot)
    }

    pub fn remove(&mut self, key: &i32) -> Option<Tick> {
        let slot = keccak256((I24::try_from(*key).unwrap(), U256::from(5)).abi_encode());
        let slot: U256 = slot.into();
        self.ticks.remove(&slot)
    }
    pub fn read_raw(&self, slot: U256) -> Option<U256> {
        for i in 0..4 {
            let offset = U256::from(i);

            if let Some(tick) = self.ticks.get(&(slot - offset)) {
                let bytes = match i {
                    0 => (tick.liquidity_net, tick.liquidity_gross).abi_encode_packed(),
                    1 => tick.fee_growth_outside_0_x128.abi_encode_packed(),
                    2 => tick.fee_growth_outside_1_x128.abi_encode_packed(),
                    3 => (
                        tick.initialized,
                        tick.seconds_outside,
                        U160::from(tick.seconds_per_liquidity_outside_x128),
                        I56::try_from(tick.tick_cumulative_outside).unwrap(),
                    )
                        .abi_encode_packed(),
                    _ => unreachable!("i < 4"),
                };
                return Some(U256::from_be_slice(&bytes));
            }
        }
        None
    }
}

#[derive(Debug, Deserialize, Serialize, Default, Clone, Copy)]
pub struct Tick {
    pub liquidity_gross: u128,
    pub liquidity_net: i128,
    pub fee_growth_outside_0_x128: U256,
    pub fee_growth_outside_1_x128: U256,
    pub tick_cumulative_outside: i64,
    pub seconds_per_liquidity_outside_x128: U256,
    pub seconds_outside: u32,
    pub initialized: bool,
}

impl Tick {
    pub fn get_fee_growth_inside(
        ticks: &Ticks,
        tick_lower: i32,
        tick_upper: i32,
        tick_current: i32,
        fee_growth_global_0_x128: U256,
        fee_growth_global_1_x128: U256,
    ) -> (U256, U256) {
        let lower = ticks.get(&tick_lower).cloned().unwrap_or_default();
        let upper = ticks.get(&tick_upper).cloned().unwrap_or_default();

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

    pub fn update(
        ticks: &mut Ticks,
        tick: i32,
        tick_current: i32,
        liquidity_delta: i128,
        fee_growth_global_0_x128: U256,
        fee_growth_global_1_x128: U256,
        seconds_per_liquidity_cumulative_x128: U256,
        tick_cumulative: i64,
        time: u32,
        upper: bool,
        max_liquidity: u128,
    ) -> Result<bool, AMMError> {
        let info = ticks.entry(tick).or_default();

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

        Ok(flipped)
    }

    pub(crate) fn cross(
        ticks: &mut Ticks,
        tick: i32,
        fee_growth_global_0_x128: U256,
        fee_growth_global_1_x128: U256,
        seconds_per_liquidity_cumulative_x128: U256,
        tick_cumulative: i64,
        time: u32,
    ) -> i128 {
        let info = ticks.entry(tick).or_default();
        info.fee_growth_outside_0_x128 = fee_growth_global_0_x128 - info.fee_growth_outside_0_x128;
        info.fee_growth_outside_1_x128 = fee_growth_global_1_x128 - info.fee_growth_outside_1_x128;
        info.seconds_per_liquidity_outside_x128 =
            seconds_per_liquidity_cumulative_x128 - info.seconds_per_liquidity_outside_x128;
        info.tick_cumulative_outside = tick_cumulative - info.tick_cumulative_outside;
        info.seconds_outside = time - info.seconds_outside;
        return info.liquidity_net;
    }
}
