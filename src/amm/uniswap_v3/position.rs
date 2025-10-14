use std::collections::hash_map::Entry;

use alloy::{
    primitives::{aliases::I24, keccak256, map::HashMap, Address, U256},
    sol_types::SolValue,
};
use uniswap_v3_math::full_math;

use crate::errors::AMMError;

use super::{liquidity_math, util::require};

#[derive(Debug, Clone, Default)]
pub struct Positions {
    pub positions: HashMap<U256, Position>,
}

impl Positions {
    pub fn entry(&mut self, key: (Address, i32, i32)) -> Entry<'_, U256, Position> {
        let key = (
            key.0,
            I24::try_from(key.1).unwrap(),
            I24::try_from(key.2).unwrap(),
        );
        let encoded = key.abi_encode_packed();
        let slot = keccak256(encoded);
        let slot = keccak256((slot, U256::from(7)).abi_encode());
        let slot: U256 = slot.into();
        self.positions.entry(slot)
    }

    pub fn read_raw(&self, slot: U256) -> Option<U256> {
        for i in 0..4 {
            let offset = U256::from(i);

            if let Some(position) = self.positions.get(&(slot - offset)) {
                let bytes = match i {
                    0 => position.liquidity.abi_encode_packed(),
                    1 => position.fee_growth_inside_0_last_x128.abi_encode_packed(),
                    2 => position.fee_growth_inside_1_last_x128.abi_encode_packed(),
                    3 => (position.tokens_owed1, position.tokens_owed0).abi_encode_packed(),
                    _ => unreachable!("i < 4"),
                };
                return Some(U256::from_be_slice(&bytes));
            }
        }
        None
    }
}

#[derive(Debug, Clone, Default, Copy)]
pub struct Position {
    pub liquidity: u128,
    pub fee_growth_inside_0_last_x128: U256,
    pub fee_growth_inside_1_last_x128: U256,
    pub tokens_owed0: u128,
    pub tokens_owed1: u128,
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
            self.tokens_owed0 += u128::try_from(tokens_owed_0).unwrap();
            self.tokens_owed1 += u128::try_from(tokens_owed_1).unwrap();
        }

        Ok(())
    }
}
