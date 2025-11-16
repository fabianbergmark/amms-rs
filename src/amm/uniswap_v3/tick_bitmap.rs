use alloy::primitives::{keccak256, U256};
use alloy::sol;
use alloy::sol_types::SolValue;
use uniswap_v3_math::bit_math;
use uniswap_v3_math::error::UniswapV3MathError;

use super::UniswapV3Pool;
use crate::amm::consts::U256_1;

sol! {
    #[sol(rpc)]
    interface IUniswapV3Pool {
        function tick_bitmap(int16) external returns (int16);
    }
}
impl UniswapV3Pool {
    //Flips the initialized state for a given tick from false to true, or vice versa
    pub fn flip_tick(&mut self, tick: i32, tick_spacing: i32) -> Result<(), UniswapV3MathError> {
        if (tick % tick_spacing) != 0 {
            return Err(UniswapV3MathError::TickSpacingError);
        }
        let (word_pos, bit_pos) = position(tick / tick_spacing);
        let mask = U256_1 << bit_pos;
        let slot = keccak256((word_pos, U256::from(6)).abi_encode());
        let slot: U256 = slot.into();
        *self.data.entry(slot).or_default() ^= mask;
        Ok(())
    }

    //Returns the next initialized tick contained in the same word (or adjacent word) as the tick that is either
    //to the left (less than or equal to) or right (greater than) of the given tick
    pub fn next_initialized_tick_within_one_word(
        &self,
        tick: i32,
        tick_spacing: i32,
        lte: bool,
    ) -> Result<(i32, bool), UniswapV3MathError> {
        let compressed = if tick < 0 && tick % tick_spacing != 0 {
            (tick / tick_spacing) - 1
        } else {
            tick / tick_spacing
        };

        if lte {
            let (word_pos, bit_pos) = position(compressed);
            let slot = keccak256((word_pos, U256::from(6)).abi_encode());
            let slot: U256 = slot.into();
            let mask = (U256_1 << bit_pos) - U256_1 + (U256_1 << bit_pos);

            let masked = self.data.get(slot) & mask;

            let initialized = !masked.is_zero();

            let next = if initialized {
                (compressed
                    - (bit_pos
                        .overflowing_sub(bit_math::most_significant_bit(masked)?)
                        .0) as i32)
                    * tick_spacing
            } else {
                (compressed - bit_pos as i32) * tick_spacing
            };

            Ok((next, initialized))
        } else {
            let (word_pos, bit_pos) = position(compressed + 1);
            let slot = keccak256((word_pos, U256::from(6)).abi_encode());
            let slot: U256 = slot.into();
            let mask = !((U256_1 << bit_pos) - U256_1);

            let masked = self.data.get(slot) & mask;

            let initialized = !masked.is_zero();

            let next = if initialized {
                (compressed
                    + 1
                    + (bit_math::least_significant_bit(masked)?
                        .overflowing_sub(bit_pos)
                        .0) as i32)
                    * tick_spacing
            } else {
                (compressed + 1 + ((0xFF - bit_pos) as i32)) * tick_spacing
            };

            Ok((next, initialized))
        }
    }
}

//Computes the position in the mapping where the initialized bit for a tick lives
pub fn position(tick: i32) -> (i16, u8) {
    ((tick >> 8) as i16, (tick % 256) as u8)
}
