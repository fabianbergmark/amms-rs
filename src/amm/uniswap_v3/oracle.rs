use alloy::{
    primitives::{aliases::I56, U160},
    sol,
};
use serde::{Deserialize, Serialize};

use super::UniswapV3Pool;

sol! {
    #[derive(Copy, Debug, Default, Serialize, Deserialize)]
    struct Observation {
        uint32 block_timestamp;
        int56 tick_cumulative;
        uint160 seconds_per_liquidity_cumulative_x128;
        bool initialized;
    }
}

/// @title Oracle
/// @notice Provides price and liquidity data useful for a wide variety of system designs
/// @dev Instances of stored oracle data, "observations", are collected in the oracle array
/// Every pool is initialized with an oracle array length of 1. Anyone can pay the SSTOREs to increase the
/// maximum length of the oracle array. New slots will be added when the array is fully populated.
/// Observations are overwritten when the full length of the oracle array is populated.
/// The most recent observation is available, independent of the length of the oracle array, by passing 0 to observe()
impl UniswapV3Pool {
    /// @notice Transforms a previous observation into a new observation, given the passage of time and the current tick and liquidity values
    /// @dev blockTimestamp _must_ be chronologically equal to or greater than last.blockTimestamp, safe for 0 or 1 overflows
    /// @param last The specified observation to be transformed
    /// @param blockTimestamp The timestamp of the new observation
    /// @param tick The active tick at the time of the new observation
    /// @param liquidity The total in-range liquidity at the time of the new observation
    /// @return Observation The newly populated observation
    pub fn observation_transform(
        last: &Observation,
        block_timestamp: u32,
        tick: i32,
        liquidity: u128,
    ) -> Observation {
        let delta = block_timestamp - last.block_timestamp;

        let tick_cumulative = last.tick_cumulative + I56::try_from(tick * delta as i32).unwrap();

        let liquidity_value = if liquidity > 0 { liquidity } else { 1 };
        let seconds_per_liquidity_cumulative_x128 = last.seconds_per_liquidity_cumulative_x128
            + ((U160::from(delta) << 128) / U160::from(liquidity_value));

        Observation {
            block_timestamp,
            tick_cumulative,
            seconds_per_liquidity_cumulative_x128,
            initialized: true,
        }
    }
    /// @notice Initialize the oracle array by writing the first slot. Called once for the lifecycle of the observations array
    /// @param self The stored oracle array
    /// @param time The time of the oracle initialization, via block.timestamp truncated to uint32
    /// @return cardinality The number of populated elements in the oracle array
    /// @return cardinalityNext The new length of the oracle array, independent of population
    pub fn observation_initialize(&mut self, time: u32) -> (u16, u16) {
        self.save_observation(
            0,
            Observation {
                block_timestamp: time,
                tick_cumulative: I56::ONE,
                seconds_per_liquidity_cumulative_x128: U160::ZERO,
                initialized: true,
            },
        );

        (1, 1)
    }

    /// @notice Writes an oracle observation to the array
    /// @dev Writable at most once per block. Index represents the most recently written element. cardinality and index must be tracked externally.
    /// If the index is at the end of the allowable array length (according to cardinality), and the next cardinality
    /// is greater than the current one, cardinality may be increased. This restriction is created to preserve ordering.
    /// @param self The stored oracle array
    /// @param index The index of the observation that was most recently written to the observations array
    /// @param blockTimestamp The timestamp of the new observation
    /// @param tick The active tick at the time of the new observation
    /// @param liquidity The total in-range liquidity at the time of the new observation
    /// @param cardinality The number of populated elements in the oracle array
    /// @param cardinalityNext The new length of the oracle array, independent of population
    /// @return indexUpdated The new index of the most recently written element in the oracle array
    /// @return cardinalityUpdated The new cardinality of the oracle array
    pub fn observation_write(
        &mut self,
        index: u16,
        block_timestamp: u32,
        tick: i32,
        liquidity: u128,
        cardinality: u16,
        cardinality_next: u16,
    ) -> (u16, u16) {
        let last = &self.get_observation(index);

        // early return if we've already written an observation this block
        if last.block_timestamp == block_timestamp {
            return (index, cardinality);
        }

        // update cardinality if appropriate
        let cardinality_updated = if cardinality_next > cardinality && index == cardinality - 1 {
            cardinality_next
        } else {
            cardinality
        };

        // increment index (circular)
        let index_updated = (index as u32 + 1) % (cardinality_updated as u32);
        let index_updated_u16 = index_updated as u16;

        // write transformed observation
        self.save_observation(
            index_updated_u16,
            Self::observation_transform(last, block_timestamp, tick, liquidity),
        );

        (index_updated_u16, cardinality_updated)
    }

    /// @notice Prepares the oracle array to store up to `next` observations
    /// @param self The stored oracle array
    /// @param current The current next cardinality of the oracle array
    /// @param next The proposed next cardinality which will be populated in the oracle array
    /// @return next The next cardinality which will be populated in the oracle array
    pub fn observation_grow(&mut self, current: u16, next: u16) -> u16 {
        assert!(current > 0, "I");

        if next <= current {
            return current;
        }

        for i in current..next {
            self.save_observation(
                i,
                Observation {
                    block_timestamp: 1,
                    ..Default::default()
                },
            );
        }

        next
    }

    pub fn lte(time: u32, a: u32, b: u32) -> bool {
        if a <= time && b <= time {
            return a <= b;
        }

        let a_adjusted = if a > time {
            a as u64
        } else {
            (a as u64) + (1u64 << 32)
        };
        let b_adjusted = if b > time {
            b as u64
        } else {
            (b as u64) + (1u64 << 32)
        };

        a_adjusted <= b_adjusted
    }

    pub fn binary_search(
        &self,
        time: u32,
        target: u32,
        index: u16,
        cardinality: u16,
    ) -> (Observation, Observation) {
        let mut l = ((index as u32 + 1) % cardinality as u32) as usize;
        let mut r = l + cardinality as usize - 1;
        let mut before_or_at: Observation;
        let mut at_or_after: Observation;

        loop {
            let i = (l + r) / 2;
            before_or_at = self.get_observation((i % cardinality as usize) as u16);

            if !before_or_at.initialized {
                l = i + 1;
                continue;
            }

            at_or_after = self.get_observation(((i + 1) % cardinality as usize) as u16);

            let target_at_or_after = Self::lte(time, before_or_at.block_timestamp, target);

            if target_at_or_after && Self::lte(time, target, at_or_after.block_timestamp) {
                break (before_or_at, at_or_after);
            }

            if !target_at_or_after {
                r = i - 1;
            } else {
                l = i + 1;
            }
        }
    }

    pub fn get_surrounding_observations(
        &self,
        time: u32,
        target: u32,
        tick: i32,
        index: u16,
        liquidity: u128,
        cardinality: u16,
    ) -> (Observation, Observation) {
        let mut before_or_at = self.get_observation(index);

        if Self::lte(time, before_or_at.block_timestamp, target) {
            if before_or_at.block_timestamp == target {
                return (before_or_at, Observation::default());
            } else {
                return (
                    before_or_at.clone(),
                    Self::observation_transform(&before_or_at, target, tick, liquidity),
                );
            }
        }

        before_or_at = self.get_observation((index + 1) % cardinality);
        if !before_or_at.initialized {
            before_or_at = self.get_observation(0);
        }

        assert!(Self::lte(time, before_or_at.block_timestamp, target), "OLD");

        self.binary_search(time, target, index, cardinality)
    }

    pub fn observe_single(
        &self,
        time: u32,
        seconds_ago: u32,
        tick: i32,
        index: u16,
        liquidity: u128,
        cardinality: u16,
    ) -> (I56, U160) {
        if seconds_ago == 0 {
            let mut last = self.get_observation(index);
            if last.block_timestamp != time {
                last = Self::observation_transform(&last, time, tick, liquidity);
            }
            return (
                last.tick_cumulative,
                last.seconds_per_liquidity_cumulative_x128,
            );
        }

        let target = time - seconds_ago;

        let (before_or_at, at_or_after) =
            self.get_surrounding_observations(time, target, tick, index, liquidity, cardinality);

        if target == before_or_at.block_timestamp {
            return (
                before_or_at.tick_cumulative,
                before_or_at.seconds_per_liquidity_cumulative_x128,
            );
        } else if target == at_or_after.block_timestamp {
            return (
                at_or_after.tick_cumulative,
                at_or_after.seconds_per_liquidity_cumulative_x128,
            );
        } else {
            let observation_time_delta = at_or_after.block_timestamp - before_or_at.block_timestamp;
            let target_delta = target - before_or_at.block_timestamp;

            let tick_cumulative = before_or_at.tick_cumulative
                + ((at_or_after.tick_cumulative - before_or_at.tick_cumulative)
                    / I56::unchecked_from(observation_time_delta))
                    * I56::unchecked_from(target_delta);

            let seconds_per_liquidity_cumulative_x128 = before_or_at
                .seconds_per_liquidity_cumulative_x128
                + ((at_or_after.seconds_per_liquidity_cumulative_x128
                    - before_or_at.seconds_per_liquidity_cumulative_x128)
                    * U160::from(target_delta)
                    / U160::from(observation_time_delta));

            (tick_cumulative, seconds_per_liquidity_cumulative_x128)
        }
    }

    pub fn observe(
        &self,
        time: u32,
        seconds_agos: &[u32],
        tick: i32,
        index: u16,
        liquidity: u128,
        cardinality: u16,
    ) -> (Vec<I56>, Vec<U160>) {
        assert!(cardinality > 0, "I");

        let mut tick_cumulatives = Vec::with_capacity(seconds_agos.len());
        let mut seconds_per_liquidity_cumulative_x128s = Vec::with_capacity(seconds_agos.len());

        for &seconds_ago in seconds_agos {
            let (tick_cum, sec_liq) =
                self.observe_single(time, seconds_ago, tick, index, liquidity, cardinality);
            tick_cumulatives.push(tick_cum);
            seconds_per_liquidity_cumulative_x128s.push(sec_liq);
        }

        (tick_cumulatives, seconds_per_liquidity_cumulative_x128s)
    }
}
