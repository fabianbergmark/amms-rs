pub mod batch_request;
pub mod factory;
pub mod liquidity_math;
pub mod oracle;
pub mod position;
pub mod tick;
pub mod util;

use std::fmt::{Display, Formatter};
use std::u128;
use std::{cmp::Ordering, collections::BTreeMap, sync::Arc};

use alloy::primitives::aliases::I24;
use alloy::primitives::ruint::UintTryFrom;
use alloy::primitives::{U128, U512};
use alloy::{
    network::Network,
    primitives::{Address, Bytes, B256, I256, U256},
    providers::Provider,
    rpc::types::eth::{Filter, Log},
    sol,
    sol_types::{SolCall, SolEvent},
    uint,
};
use async_trait::async_trait;
use futures::{stream::FuturesOrdered, StreamExt};
use liquidity_math::add_delta;
use num_bigfloat::BigFloat;
use oracle::Observations;
use position::Positions;
use serde::{Deserialize, Serialize};
use tick::{Tick, Ticks};
use tracing::instrument;
use uniswap_v3_math::full_math::mul_div;
use uniswap_v3_math::tick_bitmap::TickBitmap;
use uniswap_v3_math::tick_math::{MAX_SQRT_RATIO, MAX_TICK, MIN_SQRT_RATIO, MIN_TICK};
use util::require;

use self::factory::IUniswapV3Factory;
use crate::{
    amm::{consts::*, AutomatedMarketMaker, IErc20},
    errors::{AMMError, ArithmeticError, EventLogError, SwapSimulationError},
};

sol! {
    /// Interface of the IUniswapV3Pool
    #[derive(Debug, PartialEq, Eq)]
    #[sol(rpc)]
    contract IUniswapV3Pool {
        event Initialize(uint160 sqrtPriceX96, int24 tick);
        event Swap(address indexed sender, address indexed recipient, int256 amount0, int256 amount1, uint160 sqrtPriceX96, uint128 liquidity, int24 tick);
        event Burn(address indexed owner, int24 indexed tickLower, int24 indexed tickUpper, uint128 amount, uint256 amount0, uint256 amount1);
        event Mint(address sender, address indexed owner, int24 indexed tickLower, int24 indexed tickUpper, uint128 amount, uint256 amount0, uint256 amount1);
        event Collect(address indexed owner, address recipient, int24 indexed tickLower, int24 indexed tickUpper, uint128 amount0, uint128 amount1);
        event Flash( address indexed sender, address indexed recipient, uint256 amount0, uint256 amount1, uint256 paid0, uint256 paid1 );
        event IncreaseObservationCardinalityNext( uint16 observationCardinalityNextOld, uint16 observationCardinalityNextNew );
        event SetFeeProtocol(uint8 feeProtocol0Old, uint8 feeProtocol1Old, uint8 feeProtocol0New, uint8 feeProtocol1New);
        event CollectProtocol(address indexed sender, address indexed recipient, uint128 amount0, uint128 amount1);
        function token0() external view returns (address);
        function token1() external view returns (address);
        function liquidity() external view returns (uint128);
        function slot0() external view returns (uint160, int24, uint16, uint16, uint16, uint8, bool);
        function fee() external view returns (uint24);
        function tickSpacing() external view returns (int24);
        function ticks(int24 tick) external view returns (uint128, int128, uint256, uint256, int56, uint160, uint32, bool);
        function tickBitmap(int16 wordPosition) external view returns (uint256);
        function swap(address recipient, bool zeroForOne, int256 amountSpecified, uint160 sqrtPriceLimitX96, bytes calldata data) external returns (int256, int256);
    }
}

pub const ONE: U256 = uint!(1_U256);

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UniswapV3Pool {
    pub address: Address,
    pub token_a: Address,
    pub token_a_decimals: u8,
    pub token_b: Address,
    pub token_b_decimals: u8,
    pub slot0: Slot0,
    pub liquidity: u128,
    pub fee: u32,
    pub tick_spacing: i32,
    pub tick_bitmap: TickBitmap,
    pub ticks: Ticks,
    pub positions: Positions,
    pub fee_growth_global_0_x128: U256,
    pub fee_growth_global_1_x128: U256,
    pub max_liquidity_per_tick: u128,
    pub protocol_fees: ProtocolFees,
    pub observations: Observations,
}

#[derive(Debug, Clone, Default, Copy)]
pub struct ModifyPositionParams {
    owner: Address,
    tick_lower: i32,
    tick_upper: i32,
    liquidity_delta: i128,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Info {
    pub liquidity_gross: u128,
    pub liquidity_net: i128,
    pub initialized: bool,
}

#[derive(Debug)]
pub struct OverflowError;

impl Display for OverflowError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str("Overflow")
    }
}

impl Info {
    pub fn new(liquidity_gross: u128, liquidity_net: i128, initialized: bool) -> Self {
        Info {
            liquidity_gross,
            liquidity_net,
            initialized,
        }
    }
}

#[async_trait]
impl AutomatedMarketMaker for UniswapV3Pool {
    fn address(&self) -> Address {
        self.address
    }

    #[instrument(skip(self, provider), level = "debug")]
    async fn sync<N, P>(&mut self, provider: Arc<P>) -> Result<(), AMMError>
    where
        N: Network,
        P: Provider<N>,
    {
        batch_request::sync_v3_pool_batch_request(self, provider.clone()).await?;
        Ok(())
    }

    // This defines the event signatures to listen to that will produce events to be passed into AMM::sync_from_log()
    fn sync_on_event_signatures(&self) -> Vec<B256> {
        vec![
            IUniswapV3Pool::Initialize::SIGNATURE_HASH,
            IUniswapV3Pool::Swap::SIGNATURE_HASH,
            IUniswapV3Pool::Mint::SIGNATURE_HASH,
            IUniswapV3Pool::Burn::SIGNATURE_HASH,
            IUniswapV3Pool::Collect::SIGNATURE_HASH,
            IUniswapV3Pool::CollectProtocol::SIGNATURE_HASH,
            IUniswapV3Pool::Flash::SIGNATURE_HASH,
            IUniswapV3Pool::IncreaseObservationCardinalityNext::SIGNATURE_HASH,
            IUniswapV3Pool::SetFeeProtocol::SIGNATURE_HASH,
        ]
    }

    #[instrument(skip(self), level = "debug")]
    fn sync_from_log(&mut self, log: Log) -> Result<(), AMMError> {
        let event_signature = log.topics()[0];

        if event_signature == *IUniswapV3Pool::Initialize::SIGNATURE_HASH {
            self.sync_from_initialize_log(log)?;
        } else if event_signature == IUniswapV3Pool::Burn::SIGNATURE_HASH {
            self.sync_from_burn_log(log)?;
        } else if event_signature == IUniswapV3Pool::Mint::SIGNATURE_HASH {
            self.sync_from_mint_log(log)?;
        } else if event_signature == IUniswapV3Pool::Swap::SIGNATURE_HASH {
            self.sync_from_swap_log(log)?;
        } else if event_signature == IUniswapV3Pool::Flash::SIGNATURE_HASH {
            self.sync_from_flash_log(log)?;
        } else if event_signature == IUniswapV3Pool::Collect::SIGNATURE_HASH {
            self.sync_from_collect_log(log)?;
        } else if event_signature == IUniswapV3Pool::CollectProtocol::SIGNATURE_HASH {
            self.sync_from_collect_protocol_log(log)?;
        } else if event_signature == IUniswapV3Pool::SetFeeProtocol::SIGNATURE_HASH {
            self.sync_from_set_fee_protocol_log(log)?;
        } else if event_signature
            == IUniswapV3Pool::IncreaseObservationCardinalityNext::SIGNATURE_HASH
        {
            self.sync_from_increase_observation_cardinality_next_log(log)?;
        } else {
            Err(EventLogError::InvalidEventSignature)?
        }

        Ok(())
    }

    fn tokens(&self) -> Vec<Address> {
        vec![self.token_a, self.token_b]
    }

    fn calculate_price(&self, base_token: Address) -> Result<f64, ArithmeticError> {
        let tick = uniswap_v3_math::tick_math::get_tick_at_sqrt_ratio(self.slot0.sqrt_price_x96)?;
        let shift = self.token_a_decimals as i8 - self.token_b_decimals as i8;

        let price = match shift.cmp(&0) {
            Ordering::Less => 1.0001_f64.powi(tick) / 10_f64.powi(-shift as i32),
            Ordering::Greater => 1.0001_f64.powi(tick) * 10_f64.powi(shift as i32),
            Ordering::Equal => 1.0001_f64.powi(tick),
        };

        if base_token == self.token_a {
            Ok(price)
        } else {
            Ok(1.0 / price)
        }
    }
    // NOTE: This function will not populate the tick_bitmap and ticks, if you want to populate those, you must call populate_tick_data on an initialized pool
    async fn populate_data<N, P>(
        &mut self,
        block_number: Option<u64>,
        provider: Arc<P>,
    ) -> Result<(), AMMError>
    where
        N: Network,
        P: Provider<N>,
    {
        batch_request::get_v3_pool_data_batch_request(self, block_number, provider.clone()).await?;
        Ok(())
    }

    fn simulate_swap(
        &self,
        token_in: Address,
        amount_in: U256,
    ) -> Result<U256, SwapSimulationError> {
        self.clone().simulate_swap_mut(token_in, amount_in)
    }

    fn simulate_swap_mut(
        &mut self,
        token_in: Address,
        amount_in: U256,
    ) -> Result<U256, SwapSimulationError> {
        let zero_for_one = token_in == self.token_a;
        let limit = if zero_for_one {
            MIN_SQRT_RATIO + U256::ONE
        } else {
            MAX_SQRT_RATIO - U256::ONE
        };
        self.swap(
            Address::default(),
            zero_for_one,
            I256::from_raw(amount_in),
            limit,
            Default::default(),
            None,
        )
        .map_err(|_| SwapSimulationError::MixedTypes)
        .map(|(amount0, amount1)| {
            if zero_for_one {
                (-amount1).into_raw()
            } else {
                (-amount0).into_raw()
            }
        })
    }

    fn get_token_out(&self, token_in: Address) -> Address {
        if self.token_a == token_in {
            self.token_b
        } else {
            self.token_a
        }
    }
}

impl UniswapV3Pool {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        address: Address,
        token_a: Address,
        token_a_decimals: u8,
        token_b: Address,
        token_b_decimals: u8,
        fee: u32,
        liquidity: u128,
        sqrt_price_x96: U256,
        tick: i32,
        tick_spacing: i32,
    ) -> UniswapV3Pool {
        let min_tick = (MIN_TICK / tick_spacing) * tick_spacing;
        let max_tick = (MAX_TICK / tick_spacing) * tick_spacing;
        let num_ticks = ((max_tick - min_tick) / tick_spacing) + 1;
        let max_liquidity_per_tick = u128::MAX / num_ticks as u128;
        UniswapV3Pool {
            address,
            token_a,
            token_a_decimals,
            token_b,
            token_b_decimals,
            fee,
            liquidity,
            tick_spacing,
            max_liquidity_per_tick,
            slot0: Slot0 {
                sqrt_price_x96,
                tick,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// Creates a new instance of the pool from the pair address.
    ///
    /// This function will populate all pool data.
    pub async fn new_from_address<N, P>(
        pair_address: Address,
        creation_block: u64,
        provider: Arc<P>,
    ) -> Result<Self, AMMError>
    where
        N: Network,
        P: Provider<N>,
    {
        let mut pool = UniswapV3Pool {
            address: pair_address,
            ..Default::default()
        };

        // We need to get tick spacing before populating tick data because tick spacing can not be uninitialized when syncing burn and mint logs
        pool.tick_spacing = pool.get_tick_spacing(provider.clone()).await?;
        pool.fee = pool.get_fee(provider.clone()).await?;

        let min_tick = (MIN_TICK / pool.tick_spacing) * pool.tick_spacing;
        let max_tick = (MAX_TICK / pool.tick_spacing) * pool.tick_spacing;
        let num_ticks = ((max_tick - min_tick) / pool.tick_spacing) + 1;
        let max_liquidity_per_tick = u128::MAX / num_ticks as u128;
        pool.max_liquidity_per_tick = max_liquidity_per_tick;

        let synced_block = pool
            .populate_tick_data(creation_block, provider.clone())
            .await?;

        // TODO: break this into two threads so it can happen concurrently
        pool.populate_data(Some(synced_block), provider).await?;

        if !pool.data_is_populated() {
            return Err(AMMError::PoolDataError);
        }

        Ok(pool)
    }

    /// Creates a new instance of the pool from a log.
    ///
    /// This function will populate all pool data.
    pub async fn new_from_log<N, P>(log: Log, provider: Arc<P>) -> Result<Self, AMMError>
    where
        N: Network,
        P: Provider<N>,
    {
        let event_signature = log.topics()[0];

        if event_signature == IUniswapV3Factory::PoolCreated::SIGNATURE_HASH {
            if let Some(block_number) = log.block_number {
                let event = IUniswapV3Factory::PoolCreated::decode_log(&log.inner)?;

                let mut pool = UniswapV3Pool {
                    address: event.address,
                    tick_spacing: event.tickSpacing.as_i32(),
                    fee: event.fee.to(),
                    ..Default::default()
                };

                let min_tick = (MIN_TICK / pool.tick_spacing) * pool.tick_spacing;
                let max_tick = (MAX_TICK / pool.tick_spacing) * pool.tick_spacing;
                let num_ticks = ((max_tick - min_tick) / pool.tick_spacing) + 1;
                let max_liquidity_per_tick = u128::MAX / num_ticks as u128;
                pool.max_liquidity_per_tick = max_liquidity_per_tick;

                let synced_block = pool
                    .populate_tick_data(block_number, provider.clone())
                    .await?;

                // TODO: break this into two threads so it can happen concurrently
                pool.populate_data(Some(synced_block), provider).await?;

                if !pool.data_is_populated() {
                    return Err(AMMError::PoolDataError);
                }

                Ok(pool)
            } else {
                Err(EventLogError::LogBlockNumberNotFound)?
            }
        } else {
            Err(EventLogError::InvalidEventSignature)?
        }
    }
    /// Creates a new instance of the pool from a log.
    ///
    /// This function will not populate all pool data.
    pub fn new_empty_pool_from_log(log: Log) -> Result<Self, EventLogError> {
        let event_signature = log.topics()[0];

        if event_signature == IUniswapV3Factory::PoolCreated::SIGNATURE_HASH {
            let event = IUniswapV3Factory::PoolCreated::decode_log(log.as_ref())?;

            let min_tick = (MIN_TICK / event.tickSpacing.as_i32()) * event.tickSpacing.as_i32();
            let max_tick = (MAX_TICK / event.tickSpacing.as_i32()) * event.tickSpacing.as_i32();
            let num_ticks = ((max_tick - min_tick) / event.tickSpacing.as_i32()) + 1;
            let max_liquidity_per_tick = u128::MAX / num_ticks as u128;

            Ok(UniswapV3Pool {
                address: event.pool,
                token_a: event.token0,
                token_b: event.token1,
                fee: event.fee.to(),
                max_liquidity_per_tick,
                tick_spacing: event.tickSpacing.as_i32(),
                ..Default::default()
            })
        } else {
            Err(EventLogError::InvalidEventSignature)
        }
    }

    /// Populates the `tick_bitmap` and `ticks` fields of the pool to the current block.
    ///
    /// Returns the last synced block number.
    pub async fn populate_tick_data<N, P>(
        &mut self,
        mut from_block: u64,
        provider: Arc<P>,
    ) -> Result<u64, AMMError>
    where
        N: Network,
        P: Provider<N>,
    {
        let current_block = provider
            .get_block_number()
            .await
            .map_err(AMMError::TransportError)?;

        let mut futures = FuturesOrdered::new();

        let mut ordered_logs: BTreeMap<u64, Vec<Log>> = BTreeMap::new();

        let pool_address: Address = self.address;

        while from_block <= current_block {
            let middleware = provider.clone();

            let mut target_block = from_block + POPULATE_TICK_DATA_STEP - 1;
            if target_block > current_block {
                target_block = current_block;
            }

            futures.push_back(async move {
                middleware
                    .get_logs(
                        &Filter::new()
                            .event_signature(vec![
                                IUniswapV3Pool::Initialize::SIGNATURE_HASH,
                                IUniswapV3Pool::Burn::SIGNATURE_HASH,
                                IUniswapV3Pool::Mint::SIGNATURE_HASH,
                                IUniswapV3Pool::Flash::SIGNATURE_HASH,
                                IUniswapV3Pool::Collect::SIGNATURE_HASH,
                                IUniswapV3Pool::CollectProtocol::SIGNATURE_HASH,
                                IUniswapV3Pool::IncreaseObservationCardinalityNext::SIGNATURE_HASH,
                                IUniswapV3Pool::Swap::SIGNATURE_HASH,
                                IUniswapV3Pool::SetFeeProtocol::SIGNATURE_HASH,
                            ])
                            .address(pool_address)
                            .from_block(from_block)
                            .to_block(target_block),
                    )
                    .await
            });

            from_block += POPULATE_TICK_DATA_STEP;
        }

        // TODO: this could be more dry since we use this in another place
        while let Some(result) = futures.next().await {
            let logs = result.map_err(AMMError::TransportError)?;

            for log in logs {
                if let Some(log_block_number) = log.block_number {
                    if let Some(log_group) = ordered_logs.get_mut(&log_block_number) {
                        log_group.push(log);
                    } else {
                        ordered_logs.insert(log_block_number, vec![log]);
                    }
                } else {
                    return Err(EventLogError::LogBlockNumberNotFound)?;
                }
            }
        }

        for (_, log_group) in ordered_logs {
            for log in log_group {
                self.sync_from_log(log)?;
            }
        }

        Ok(current_block)
    }

    pub fn swap(
        &mut self,
        _recipient: Address,
        zero_for_one: bool,
        amount_specified: I256,
        sqrt_price_limit_x_96: U256,
        block_timestamp: u64,
        amount_in_from_log: Option<I256>,
    ) -> Result<(I256, I256), AMMError> {
        require(amount_specified != I256::ZERO, "AS")?;

        let slot0_start = self.slot0;

        require(
            if zero_for_one {
                sqrt_price_limit_x_96 < slot0_start.sqrt_price_x96
                    && sqrt_price_limit_x_96 > MIN_SQRT_RATIO
            } else {
                sqrt_price_limit_x_96 > slot0_start.sqrt_price_x96
                    && sqrt_price_limit_x_96 < MAX_SQRT_RATIO
            },
            "SPL",
        )?;

        let mut cache = SwapCache {
            fee_protocol: if zero_for_one {
                slot0_start.fee_protocol % 16
            } else {
                slot0_start.fee_protocol >> 4
            },
            liquidity_start: self.liquidity,
            block_timestamp: block_timestamp as u32,
            tick_cumulative: 0,
            seconds_per_liquidity_cumulative_x128: U256::ZERO,
            computed_last_observations: false,
        };

        let exact_input = amount_specified > I256::ZERO;

        let mut state = SwapState {
            amount_specified_remaining: amount_specified, //Amount of token_in that has not been swapped
            amount_calculated: I256::ZERO, //Amount of token_out that has been calculated
            sqrt_price_x_96: slot0_start.sqrt_price_x96, //Active price on the pool
            tick: slot0_start.tick,        //Current i24 tick of the pool
            liquidity: cache.liquidity_start,
            fee_growth_global_x128: if zero_for_one {
                self.fee_growth_global_0_x128
            } else {
                self.fee_growth_global_1_x128
            },
            protocol_fee: 0,
        };

        // continue swapping as long as we haven't used the entire input/output and haven't reached the price limit
        while state.amount_specified_remaining != I256::ZERO
            && state.sqrt_price_x_96 != sqrt_price_limit_x_96
        {
            let mut step = StepComputations {
                sqrt_price_start_x_96: state.sqrt_price_x_96, //Set the sqrt_price_start_x_96 to the current sqrt_price_x_96
                ..Default::default()
            };

            //Get the next tick from the current tick
            (step.tick_next, step.initialized) =
                self.tick_bitmap.next_initialized_tick_within_one_word(
                    state.tick,
                    self.tick_spacing,
                    zero_for_one,
                )?;

            // ensure that we do not overshoot the min/max tick, as the tick bitmap is not aware of these bounds
            step.tick_next = step.tick_next.clamp(MIN_TICK, MAX_TICK);

            // get the price for the next tick
            step.sqrt_price_next_x96 =
                uniswap_v3_math::tick_math::get_sqrt_ratio_at_tick(step.tick_next)?;

            // compute values to swap to the target tick, price limit, or point where input/output amount is exhausted
            (
                state.sqrt_price_x_96,
                step.amount_in,
                step.amount_out,
                step.fee_amount,
            ) = uniswap_v3_math::swap_math::compute_swap_step(
                state.sqrt_price_x_96,
                if zero_for_one {
                    if step.sqrt_price_next_x96 < sqrt_price_limit_x_96 {
                        sqrt_price_limit_x_96
                    } else {
                        step.sqrt_price_next_x96
                    }
                } else if step.sqrt_price_next_x96 > sqrt_price_limit_x_96 {
                    sqrt_price_limit_x_96
                } else {
                    step.sqrt_price_next_x96
                },
                state.liquidity,
                state.amount_specified_remaining,
                self.fee,
            )?;

            if exact_input {
                state.amount_specified_remaining -=
                    I256::try_from(step.amount_in + step.fee_amount).unwrap();
                state.amount_calculated -= I256::try_from(step.amount_out).unwrap();
            } else {
                state.amount_specified_remaining += I256::try_from(step.amount_out).unwrap();
                state.amount_calculated +=
                    I256::try_from(step.amount_in + step.fee_amount).unwrap();
            }

            // Patch for determining edge case where more fee is paid
            if let Some(amount_in) = &amount_in_from_log {
                assert!(!exact_input);
                if state.amount_specified_remaining.is_zero()
                    && state.sqrt_price_x_96 == sqrt_price_limit_x_96
                {
                    let extra_fee = *amount_in - state.amount_calculated;
                    state.amount_calculated += extra_fee;
                    assert!(extra_fee >= I256::ZERO);
                    step.fee_amount += extra_fee.into_raw();
                }
            }

            // if the protocol fee is on, calculate how much is owed, decrement feeAmount, and increment protocolFee
            if cache.fee_protocol > 0 {
                let delta = step.fee_amount / U256::from(cache.fee_protocol);
                step.fee_amount -= delta;
                state.protocol_fee += u128::try_from(delta).unwrap();
            }

            // update global fee tracker
            if state.liquidity > 0 {
                state.fee_growth_global_x128 += mul_div(
                    step.fee_amount,
                    U256::ONE << 128,
                    U256::from(state.liquidity),
                )?;
            }

            // shift tick if we reached the next price
            if state.sqrt_price_x_96 == step.sqrt_price_next_x96 {
                // if the tick is initialized, run the tick transition
                if step.initialized {
                    // check for the placeholder value, which we replace with the actual value the first time the swap
                    // crosses an initialized tick
                    if !cache.computed_last_observations {
                        (
                            cache.tick_cumulative,
                            cache.seconds_per_liquidity_cumulative_x128,
                        ) = self.observations.observe_single(
                            cache.block_timestamp,
                            0,
                            slot0_start.tick,
                            slot0_start.observation_index,
                            cache.liquidity_start,
                            slot0_start.observation_cardinality,
                        );
                        cache.computed_last_observations = true;
                    }

                    let mut liquidity_net = Tick::cross(
                        &mut self.ticks,
                        step.tick_next,
                        if zero_for_one {
                            state.fee_growth_global_x128
                        } else {
                            self.fee_growth_global_0_x128
                        },
                        if zero_for_one {
                            self.fee_growth_global_1_x128
                        } else {
                            state.fee_growth_global_x128
                        },
                        cache.seconds_per_liquidity_cumulative_x128,
                        cache.tick_cumulative,
                        cache.block_timestamp,
                    );

                    if zero_for_one {
                        liquidity_net = -liquidity_net;
                    }

                    state.liquidity = add_delta(state.liquidity, liquidity_net)?;
                }

                state.tick = if zero_for_one {
                    step.tick_next - 1
                } else {
                    step.tick_next
                }
            } else if state.sqrt_price_x_96 != step.sqrt_price_start_x_96 {
                // recompute unless we're on a lower tick boundary (i.e. already transitioned ticks), and haven't moved
                state.tick =
                    uniswap_v3_math::tick_math::get_tick_at_sqrt_ratio(state.sqrt_price_x_96)?;
            }
        }

        if state.tick != slot0_start.tick {
            let (observation_index, observation_cardinality) = self.observations.write(
                slot0_start.observation_index,
                cache.block_timestamp,
                slot0_start.tick,
                cache.liquidity_start,
                slot0_start.observation_cardinality,
                slot0_start.observation_cardinality_next,
            );
            self.slot0 = Slot0 {
                sqrt_price_x96: state.sqrt_price_x_96,
                tick: state.tick,
                observation_index,
                observation_cardinality,
                ..self.slot0
            };
            self.slot0.sqrt_price_x96 = state.sqrt_price_x_96;
            self.slot0.tick = state.tick;
        } else {
            // otherwise just update the price
            self.slot0.sqrt_price_x96 = state.sqrt_price_x_96;
        }

        // update liquidity if it changed
        if cache.liquidity_start != state.liquidity {
            self.liquidity = state.liquidity
        }

        // update fee growth global and, if necessary, protocol fees
        // overflow is acceptable, protocol has to withdraw before it hits type(uint128).max fees
        if zero_for_one {
            self.fee_growth_global_0_x128 = state.fee_growth_global_x128;
            if state.protocol_fee > 0 {
                self.protocol_fees.token0 += state.protocol_fee;
            }
        } else {
            self.fee_growth_global_1_x128 = state.fee_growth_global_x128;
            if state.protocol_fee > 0 {
                self.protocol_fees.token1 += state.protocol_fee;
            }
        }

        let (amount0, amount1) = if zero_for_one == exact_input {
            (
                amount_specified - state.amount_specified_remaining,
                state.amount_calculated,
            )
        } else {
            (
                state.amount_calculated,
                amount_specified - state.amount_specified_remaining,
            )
        };

        Ok((amount0, amount1))
    }

    // Only for tracking state with logs
    pub fn flash(
        &mut self,
        _recipient: Address,
        _amount0: U256,
        _amount1: U256,
        paid0: U256,
        paid1: U256,
    ) -> Result<(), AMMError> {
        let liquidity = self.liquidity;
        require(liquidity > 0, "L")?;

        if paid0 > U256::ZERO {
            let fee_protocol0 = self.slot0.fee_protocol % 16;
            let fees0 = if fee_protocol0 == 0 {
                U256::ZERO
            } else {
                paid0 / U256::from(fee_protocol0)
            };

            if fees0 > U256::ZERO {
                self.protocol_fees.token0 += u128::try_from(fees0).unwrap();
            }

            self.fee_growth_global_0_x128 +=
                mul_div(paid0 - fees0, U256::ONE << 128, U256::from(liquidity))?;
        }

        if paid1 > U256::ZERO {
            let fee_protocol1 = self.slot0.fee_protocol >> 4;
            let fees1 = if fee_protocol1 == 0 {
                U256::ZERO
            } else {
                paid1 / U256::from(fee_protocol1)
            };

            if fees1 > U256::ZERO {
                self.protocol_fees.token1 += u128::try_from(fees1).unwrap();
            }

            self.fee_growth_global_1_x128 +=
                mul_div(paid1 - fees1, U256::ONE << 128, U256::from(liquidity))?;
        }

        Ok(())
    }

    /// Returns the swap fee of the pool.
    pub fn fee(&self) -> u32 {
        self.fee
    }

    /// Returns whether the pool data is populated.
    pub fn data_is_populated(&self) -> bool {
        !(self.token_a.is_zero() || self.token_b.is_zero())
    }

    /// Returns the word position of a tick in the `tick_bitmap`.
    pub async fn get_tick_word<N, P>(&self, tick: i32, provider: Arc<P>) -> Result<U256, AMMError>
    where
        N: Network,
        P: Provider<N>,
    {
        let v3_pool = IUniswapV3Pool::new(self.address, provider);
        let (word_position, _) = uniswap_v3_math::tick_bitmap::position(tick);
        let bm = v3_pool.tickBitmap(word_position).call().await?;
        Ok(bm)
    }

    /// Returns the next word in the `tick_bitmap` after a given word position.
    pub async fn get_next_word<N, P>(
        &self,
        word_position: i16,
        provider: Arc<P>,
    ) -> Result<U256, AMMError>
    where
        N: Network,
        P: Provider<N>,
    {
        let v3_pool = IUniswapV3Pool::new(self.address, provider);
        let bm = v3_pool.tickBitmap(word_position).call().await?;
        Ok(bm)
    }

    /// Returns the tick spacing of the pool.
    pub async fn get_tick_spacing<N, P>(&self, provider: Arc<P>) -> Result<i32, AMMError>
    where
        N: Network,
        P: Provider<N>,
    {
        let v3_pool = IUniswapV3Pool::new(self.address, provider);
        let ts = v3_pool.tickSpacing().call().await?;
        Ok(ts.as_i32())
    }

    /// Fetches the current tick of the pool via static call.
    pub async fn get_tick<N, P>(&self, provider: Arc<P>) -> Result<i32, AMMError>
    where
        N: Network,
        P: Provider<N>,
    {
        Ok(self.get_slot_0(provider).await?.1)
    }

    /// Fetches the tick info of a given tick via static call.
    pub async fn get_tick_info<N, P>(
        &self,
        tick: i32,
        provider: Arc<P>,
    ) -> Result<(u128, i128, U256, U256, i64, U256, u32, bool), AMMError>
    where
        N: Network,
        P: Provider<N>,
    {
        let v3_pool = IUniswapV3Pool::new(self.address, provider.clone());

        let tick_info = v3_pool.ticks(I24::try_from(tick).unwrap()).call().await?;

        Ok((
            tick_info._0,
            tick_info._1,
            tick_info._2,
            tick_info._3,
            tick_info._4.as_i64(),
            tick_info._5.to(),
            tick_info._6,
            tick_info._7,
        ))
    }

    /// Fetches `liquidity_net` at a given tick via static call.
    pub async fn get_liquidity_net<N, P>(
        &self,
        tick: i32,
        provider: Arc<P>,
    ) -> Result<i128, AMMError>
    where
        N: Network,
        P: Provider<N>,
    {
        let tick_info = self.get_tick_info(tick, provider).await?;
        Ok(tick_info.1)
    }

    /// Fetches whether a specified tick is initialized via static call.
    pub async fn get_initialized<N, P>(&self, tick: i32, provider: Arc<P>) -> Result<bool, AMMError>
    where
        N: Network,
        P: Provider<N>,
    {
        let tick_info = self.get_tick_info(tick, provider).await?;
        Ok(tick_info.7)
    }

    /// Fetches the current slot 0 of the pool via static call.
    pub async fn get_slot_0<N, P>(
        &self,
        provider: Arc<P>,
    ) -> Result<(U256, i32, u16, u16, u16, u8, bool), AMMError>
    where
        N: Network,
        P: Provider<N>,
    {
        let v3_pool = IUniswapV3Pool::new(self.address, provider);
        let slot = v3_pool.slot0().call().await?;
        Ok((
            slot._0.to(),
            slot._1.as_i32(),
            slot._2,
            slot._3,
            slot._4,
            slot._5,
            slot._6,
        ))
    }

    /// Fetches the current liquidity of the pool via static call.
    pub async fn get_liquidity<N, P>(&self, provider: Arc<P>) -> Result<u128, AMMError>
    where
        N: Network,
        P: Provider<N>,
    {
        let v3_pool = IUniswapV3Pool::new(self.address, provider);
        let liquidity = v3_pool.liquidity().call().await?;
        Ok(liquidity)
    }

    /// Fetches the current sqrt price of the pool via static call.
    pub async fn get_sqrt_price<N, P>(&self, provider: Arc<P>) -> Result<U256, AMMError>
    where
        N: Network,
        P: Provider<N>,
    {
        Ok(self.get_slot_0(provider).await?.0)
    }

    pub fn sync_from_initialize_log(&mut self, log: Log) -> Result<(), alloy::dyn_abi::Error> {
        let event = IUniswapV3Pool::Initialize::decode_log(log.as_ref())?;

        self.slot0.sqrt_price_x96 = event.sqrtPriceX96.to();
        self.slot0.tick = event.tick.as_i32();
        let (cardinality, cardinality_next) = self
            .observations
            .initialize(log.block_timestamp.unwrap() as u32);
        self.slot0.observation_cardinality = cardinality;
        self.slot0.observation_cardinality_next = cardinality_next;
        self.slot0.unlocked = true;

        Ok(())
    }

    /// Updates the pool state from a burn event log.
    pub fn sync_from_burn_log(&mut self, log: Log) -> Result<(), AMMError> {
        let event = IUniswapV3Pool::Burn::decode_log(log.as_ref())?;

        match self.burn(
            event.owner,
            i32::try_from(event.tickLower).unwrap(),
            i32::try_from(event.tickUpper).unwrap(),
            event.amount,
            log.block_timestamp.unwrap(),
        ) {
            Ok((amount_0, amount_1)) => {
                assert_eq!(amount_0, event.amount0);
                assert_eq!(amount_1, event.amount1);
            }
            Err(e) => return Err(e),
        }
        tracing::debug!(?event, address = ?self.address, sqrt_price = ?self.slot0.sqrt_price_x96, liquidity = ?self.liquidity, tick = ?self.slot0.tick, "UniswapV3 burn event");
        Ok(())
    }

    /// Updates the pool state from a mint event log.
    pub fn sync_from_mint_log(&mut self, log: Log) -> Result<(), AMMError> {
        let event = IUniswapV3Pool::Mint::decode_log(log.as_ref())?;

        match self.mint(
            event.owner,
            i32::try_from(event.tickLower).unwrap(),
            i32::try_from(event.tickUpper).unwrap(),
            event.amount,
            log.block_timestamp.unwrap(),
        ) {
            Ok((amount_0, amount_1)) => {
                assert_eq!(amount_0, event.amount0);
                assert_eq!(amount_1, event.amount1);
            }
            Err(e) => return Err(e),
        }

        tracing::debug!(?event, address = ?self.address, sqrt_price = ?self.slot0.sqrt_price_x96, liquidity = ?self.liquidity, tick = ?self.slot0.tick, "UniswapV3 mint event");

        Ok(())
    }

    pub fn mint_helper(
        &self,
        amount0: U256,
        amount1: U256,
        tick_lower: i32,
        tick_upper: i32,
    ) -> Result<u128, OverflowError> {
        let liquidity;

        if self.slot0.tick < tick_lower {
            liquidity = Self::get_amount_0_delta_inverted(
                Self::get_sqrt_ratio_at_tick(tick_lower),
                Self::get_sqrt_ratio_at_tick(tick_upper),
                amount0,
            )?
            .to();
        }
        //if the tick is between the tick lower and tick upper, update the liquidity between the ticks
        else if self.slot0.tick < tick_upper {
            let liquidity_lower: u128 = Self::get_amount_0_delta_inverted(
                self.slot0.sqrt_price_x96,
                Self::get_sqrt_ratio_at_tick(tick_upper),
                amount0,
            )?
            .to();

            let liquidity_upper: u128 = Self::get_amount_1_delta_inverted(
                Self::get_sqrt_ratio_at_tick(tick_lower),
                self.slot0.sqrt_price_x96,
                amount1,
            )?
            .to();
            // take the minimum value
            liquidity = liquidity_lower.min(liquidity_upper);
        } else {
            liquidity = Self::get_amount_1_delta_inverted(
                Self::get_sqrt_ratio_at_tick(tick_lower),
                Self::get_sqrt_ratio_at_tick(tick_upper),
                amount1,
            )?
            .to();
        }

        // Temporary check for correctness
        let mut a0 = I256::ZERO;
        let mut a1 = I256::ZERO;
        let liquidity_delta = liquidity as i128;
        if liquidity_delta != 0 {
            if self.slot0.tick < tick_lower {
                a0 = Self::get_amount_0_delta(
                    Self::get_sqrt_ratio_at_tick(tick_lower),
                    Self::get_sqrt_ratio_at_tick(tick_upper),
                    liquidity_delta,
                )
            }
            //if the tick is between the tick lower and tick upper, update the liquidity between the ticks
            else if self.slot0.tick < tick_upper {
                a0 = Self::get_amount_0_delta(
                    self.slot0.sqrt_price_x96,
                    Self::get_sqrt_ratio_at_tick(tick_upper),
                    liquidity_delta,
                );
                a1 = Self::get_amount_1_delta(
                    Self::get_sqrt_ratio_at_tick(tick_lower),
                    self.slot0.sqrt_price_x96,
                    liquidity_delta,
                );
            } else {
                a1 = Self::get_amount_1_delta(
                    Self::get_sqrt_ratio_at_tick(tick_lower),
                    Self::get_sqrt_ratio_at_tick(tick_upper),
                    liquidity_delta,
                )
            }
        }
        assert!(a0.into_raw() <= amount0);
        assert!(a1.into_raw() <= amount1);

        Ok(liquidity)
    }

    pub fn get_next_tick(&self, dir: i32) -> i32 {
        let mut compressed = self.slot0.tick / self.tick_spacing;
        if self.slot0.tick < 0 && self.slot0.tick % self.tick_spacing != 0 {
            compressed -= 1;
        }
        compressed += dir;
        compressed * self.tick_spacing
    }

    pub fn mint(
        &mut self,
        recipient: Address,
        tick_lower: i32,
        tick_upper: i32,
        amount: u128,
        block_timestamp: u64,
    ) -> Result<(U256, U256), AMMError> {
        require(amount > 0, "mint: amount must be larger than zero")?;
        let (amount_0_int, amount_1_int) = self.modify_position(
            ModifyPositionParams {
                owner: recipient,
                tick_lower,
                tick_upper,
                liquidity_delta: amount as i128,
            },
            block_timestamp,
        )?;

        let amount0 = amount_0_int.into_raw();
        let amount1 = amount_1_int.into_raw();

        Ok((amount0, amount1))
    }

    pub fn collect(
        &mut self,
        owner: Address,
        _recipient: Address,
        tick_lower: i32,
        tick_upper: i32,
        amount_0_requested: u128,
        amount_1_requested: u128,
    ) -> (u128, u128) {
        let position = self
            .positions
            .entry((owner, tick_lower, tick_upper))
            .or_default();

        let amount0 = if amount_0_requested > position.tokens_owed0 {
            position.tokens_owed0
        } else {
            amount_0_requested
        };
        let amount1 = if amount_1_requested > position.tokens_owed1 {
            position.tokens_owed1
        } else {
            amount_1_requested
        };

        if amount0 > 0 {
            position.tokens_owed0 -= amount0
        }
        if amount1 > 0 {
            position.tokens_owed1 -= amount1
        }
        (amount0, amount1)
    }

    pub fn burn(
        &mut self,
        owner: Address,
        tick_lower: i32,
        tick_upper: i32,
        amount: u128,
        block_timestamp: u64,
    ) -> Result<(U256, U256), AMMError> {
        let (amount_0_int, amount_1_int) = self.modify_position(
            ModifyPositionParams {
                owner,
                tick_lower,
                tick_upper,
                liquidity_delta: -(amount as i128),
            },
            block_timestamp,
        )?;
        let position = self
            .positions
            .entry((owner, tick_lower, tick_upper))
            .or_default();

        let amount0 = (-amount_0_int).into_raw();
        let amount1 = (-amount_1_int).into_raw();

        if amount0 > U256::ZERO || amount1 > U256::ZERO {
            position.tokens_owed0 += u128::try_from(amount0).unwrap();
            position.tokens_owed1 += u128::try_from(amount1).unwrap();
        }

        Ok((amount0, amount1))
    }

    fn get_sqrt_ratio_at_tick(tick: i32) -> U256 {
        let abs_tick = tick.abs();

        let mut ratio: U256 = if abs_tick & 0x1 != 0 {
            U256::from(0xfffcb933bd6fad37aa2d162d1a594001_u128)
        } else {
            ONE << 128
        };
        if abs_tick & 0x2 != 0 {
            ratio = (ratio * U256::from(0xfff97272373d413259a46990580e213a_u128)) >> 128
        };
        if abs_tick & 0x4 != 0 {
            ratio = (ratio * U256::from(0xfff2e50f5f656932ef12357cf3c7fdcc_u128)) >> 128
        };
        if abs_tick & 0x8 != 0 {
            ratio = (ratio * U256::from(0xffe5caca7e10e4e61c3624eaa0941cd0_u128)) >> 128
        };
        if abs_tick & 0x10 != 0 {
            ratio = (ratio * U256::from(0xffcb9843d60f6159c9db58835c926644_u128)) >> 128
        };
        if abs_tick & 0x20 != 0 {
            ratio = (ratio * U256::from(0xff973b41fa98c081472e6896dfb254c0_u128)) >> 128
        };
        if abs_tick & 0x40 != 0 {
            ratio = (ratio * U256::from(0xff2ea16466c96a3843ec78b326b52861_u128)) >> 128
        };
        if abs_tick & 0x80 != 0 {
            ratio = (ratio * U256::from(0xfe5dee046a99a2a811c461f1969c3053_u128)) >> 128
        };
        if abs_tick & 0x100 != 0 {
            ratio = (ratio * U256::from(0xfcbe86c7900a88aedcffc83b479aa3a4_u128)) >> 128
        };
        if abs_tick & 0x200 != 0 {
            ratio = (ratio * U256::from(0xf987a7253ac413176f2b074cf7815e54_u128)) >> 128
        };
        if abs_tick & 0x400 != 0 {
            ratio = (ratio * U256::from(0xf3392b0822b70005940c7a398e4b70f3_u128)) >> 128
        };
        if abs_tick & 0x800 != 0 {
            ratio = (ratio * U256::from(0xe7159475a2c29b7443b29c7fa6e889d9_u128)) >> 128
        };
        if abs_tick & 0x1000 != 0 {
            ratio = (ratio * U256::from(0xd097f3bdfd2022b8845ad8f792aa5825_u128)) >> 128
        };
        if abs_tick & 0x2000 != 0 {
            ratio = (ratio * U256::from(0xa9f746462d870fdf8a65dc1f90e061e5_u128)) >> 128
        };
        if abs_tick & 0x4000 != 0 {
            ratio = (ratio * U256::from(0x70d869a156d2a1b890bb3df62baf32f7_u128)) >> 128
        };
        if abs_tick & 0x8000 != 0 {
            ratio = (ratio * U256::from(0x31be135f97d08fd981231505542fcfa6_u128)) >> 128
        };
        if abs_tick & 0x10000 != 0 {
            ratio = (ratio * U256::from(0x9aa508b5b7a84e1c677de54f3e99bc9_u128)) >> 128
        };
        if abs_tick & 0x20000 != 0 {
            ratio = (ratio * U256::from(0x5d6af8dedb81196699c329225ee604_u128)) >> 128
        };
        if abs_tick & 0x40000 != 0 {
            ratio = (ratio * U256::from(0x2216e584f5fa1ea926041bedfe98_u128)) >> 128
        };
        if abs_tick & 0x80000 != 0 {
            ratio = (ratio * U256::from(0x48a170391f7dc42444e8fa2_u128)) >> 128
        };

        if tick > 0 {
            ratio = U256::MAX / ratio
        };

        // this divides by 1<<32 rounding up to go from a Q128.128 to a Q128.96.
        // we then downcast because we know the result always fits within 160 bits due to our tick input constraint
        // we round up in the division so getTickAtSqrtRatio of the output price is always consistent
        (ratio >> 32)
            + if ratio % U256::from(1_u64 << 32) == U256::ZERO {
                U256::ZERO
            } else {
                ONE
            }
    }

    fn get_amount_0_delta(mut a: U256, mut b: U256, liq: i128) -> I256 {
        let (liquidity, roundup) = if liq < 0 {
            (-liq as u128, false)
        } else {
            (liq as u128, true)
        };
        if a > b {
            (a, b) = (b, a);
        }
        let numerator1 = U512::from(liquidity) << 96;
        let numerator2 = U512::from(b - a);
        let amount0 = if roundup {
            let mut result = U256::uint_try_from((numerator1 * numerator2) / U512::from(b))
                .expect("Failed to convert U512 to U256");
            if (numerator1 * numerator2) % U512::from(b) > U512::ZERO {
                result += ONE;
            }
            let result = result / a
                + if result % a > U256::ZERO {
                    ONE
                } else {
                    U256::ZERO
                };
            I256::try_from(result).expect("Failed to convert U256 to I256")
        } else {
            -I256::try_from(
                U256::uint_try_from(((numerator1 * numerator2) / U512::from(b)) / U512::from(a))
                    .expect("Failed to convert U512 to U256"),
            )
            .expect("Failed to convert U256 to I256")
        };
        return amount0;
    }

    fn get_amount_0_delta_inverted(
        mut a: U256,
        mut b: U256,
        amount0: U256,
    ) -> Result<U128, OverflowError> {
        if a > b {
            (a, b) = (b, a);
        }
        if a == b {
            return Ok(U128::MAX);
        }
        let amount0 = U512::from(amount0);
        let a = U512::from(a);
        let b = U512::from(b);
        let liq: U512 = amount0 * ((a * b) >> 96) / (b - a);
        if liq > U512::from(U128::MAX) {
            return Err(OverflowError);
        }
        Ok(U128::from(liq))
    }

    fn get_amount_1_delta(mut a: U256, mut b: U256, liq: i128) -> I256 {
        let (liquidity, roundup) = if liq < 0 {
            (-liq as u128, false)
        } else {
            (liq as u128, true)
        };
        if a > b {
            (a, b) = (b, a);
        }

        let amount1 = if roundup {
            let mut result = U256::uint_try_from((U512::from(liquidity) * U512::from(b - a)) >> 96)
                .expect("Failed to convert U512 to U256");
            if (U512::from(liquidity) * U512::from(b - a))
                % U512::from(0x1000000000000000000000000_u128)
                > U512::ZERO
            {
                result += ONE;
            }
            I256::try_from(result).expect("Failed to convert U256 to I256")
        } else {
            -I256::try_from(
                U256::uint_try_from((U512::from(liquidity) * U512::from(b - a)) >> 96)
                    .expect("Failed to convert U512 to U256"),
            )
            .expect("Failed to convert U256 to I256")
        };
        return amount1;
    }

    fn get_amount_1_delta_inverted(
        mut a: U256,
        mut b: U256,
        amount1: U256,
    ) -> Result<U128, OverflowError> {
        if a > b {
            (a, b) = (b, a);
        }
        if a == b {
            return Ok(U128::MAX);
        }

        let denom = U512::from(b - a);
        let res: U512 = (U512::from(amount1) << 96) / denom;
        if res > U512::from(U128::MAX) {
            return Err(OverflowError);
        }
        Ok(U128::from(res))
    }

    pub fn check_ticks(tick_lower: i32, tick_upper: i32) -> Result<(), AMMError> {
        require(tick_lower < tick_upper, "TLU")?;
        require(tick_lower >= MIN_TICK, "TLM")?;
        require(tick_upper <= MAX_TICK, "TUM")?;
        Ok(())
    }

    pub fn read_raw(&self, slot: U256) -> U256 {
        if slot == U256::ZERO {
            return self.slot0.into();
        }
        if slot == U256::from(1) {
            return self.fee_growth_global_0_x128;
        }
        if slot == U256::from(2) {
            return self.fee_growth_global_1_x128;
        }
        if slot == U256::from(4) {
            return U256::from(self.liquidity);
        }
        if let Some(value) = self.positions.read_raw(slot) {
            return value;
        }
        if let Some(value) = self.ticks.read_raw(slot) {
            return value;
        }
        if let Some(value) = self.observations.read_raw(slot) {
            return value;
        }
        if let Some(value) = self.tick_bitmap.read_raw(slot) {
            return value;
        }
        U256::ZERO
    }

    pub fn modify_position(
        &mut self,
        params: ModifyPositionParams,
        block_timestamp: u64,
    ) -> Result<(I256, I256), AMMError> {
        Self::check_ticks(params.tick_lower, params.tick_upper)?;

        self.update_position(
            params.owner,
            params.tick_lower,
            params.tick_upper,
            params.liquidity_delta,
            self.slot0.tick,
            block_timestamp,
        )?;

        let mut amount0 = Default::default();
        let mut amount1 = Default::default();
        if params.liquidity_delta != 0 {
            if self.slot0.tick < params.tick_lower {
                amount0 = Self::get_amount_0_delta(
                    Self::get_sqrt_ratio_at_tick(params.tick_lower),
                    Self::get_sqrt_ratio_at_tick(params.tick_upper),
                    params.liquidity_delta,
                )
            }
            //if the tick is between the tick lower and tick upper, update the liquidity between the ticks
            else if self.slot0.tick < params.tick_upper {
                let liquidity_before = self.liquidity;

                // write an oracle entry
                (
                    self.slot0.observation_index,
                    self.slot0.observation_cardinality,
                ) = self.observations.write(
                    self.slot0.observation_index,
                    block_timestamp as u32,
                    self.slot0.tick,
                    liquidity_before,
                    self.slot0.observation_cardinality,
                    self.slot0.observation_cardinality_next,
                );

                amount0 = Self::get_amount_0_delta(
                    self.slot0.sqrt_price_x96,
                    Self::get_sqrt_ratio_at_tick(params.tick_upper),
                    params.liquidity_delta,
                );
                amount1 = Self::get_amount_1_delta(
                    Self::get_sqrt_ratio_at_tick(params.tick_lower),
                    self.slot0.sqrt_price_x96,
                    params.liquidity_delta,
                );

                self.liquidity = if params.liquidity_delta < 0 {
                    liquidity_before - ((-params.liquidity_delta) as u128)
                } else {
                    liquidity_before + (params.liquidity_delta as u128)
                }
            } else {
                amount1 = Self::get_amount_1_delta(
                    Self::get_sqrt_ratio_at_tick(params.tick_lower),
                    Self::get_sqrt_ratio_at_tick(params.tick_upper),
                    params.liquidity_delta,
                )
            }
        }
        Ok((amount0, amount1))
    }

    pub fn update_position(
        &mut self,
        owner: Address,
        tick_lower: i32,
        tick_upper: i32,
        liquidity_delta: i128,
        tick: i32,
        block_timestamp: u64,
    ) -> Result<(), AMMError> {
        let position = self
            .positions
            .entry((owner, tick_lower, tick_upper))
            .or_default();

        let fee_growth_global_0_x128 = self.fee_growth_global_0_x128;
        let fee_growth_global_1_x128 = self.fee_growth_global_1_x128;

        let mut flipped_lower = false;
        let mut flipped_upper = false;
        if liquidity_delta != 0 {
            let time = block_timestamp as u32;
            let (tick_cumulative, seconds_per_liquidity_cumulative_x128) =
                self.observations.observe_single(
                    time,
                    0,
                    self.slot0.tick,
                    self.slot0.observation_index,
                    self.liquidity,
                    self.slot0.observation_cardinality,
                );

            flipped_lower = Tick::update(
                &mut self.ticks,
                tick_lower,
                tick,
                liquidity_delta,
                fee_growth_global_0_x128,
                fee_growth_global_1_x128,
                seconds_per_liquidity_cumulative_x128,
                tick_cumulative,
                0,
                false,
                self.max_liquidity_per_tick,
            )?;
            flipped_upper = Tick::update(
                &mut self.ticks,
                tick_upper,
                tick,
                liquidity_delta,
                fee_growth_global_0_x128,
                fee_growth_global_1_x128,
                seconds_per_liquidity_cumulative_x128,
                tick_cumulative,
                0,
                true,
                self.max_liquidity_per_tick,
            )?;
            if flipped_lower {
                self.tick_bitmap.flip_tick(tick_lower, self.tick_spacing)?;
            }
            if flipped_upper {
                self.tick_bitmap.flip_tick(tick_upper, self.tick_spacing)?;
            }
        }

        let (fee_growth_inside_0_x128, fee_growth_inside_1_x128) = Tick::get_fee_growth_inside(
            &mut self.ticks,
            tick_lower,
            tick_upper,
            tick,
            fee_growth_global_0_x128,
            fee_growth_global_1_x128,
        );

        position.update(
            liquidity_delta,
            fee_growth_inside_0_x128,
            fee_growth_inside_1_x128,
        )?;

        if liquidity_delta < 0 {
            if flipped_lower {
                self.ticks.remove(&tick_lower);
            }
            if flipped_upper {
                self.ticks.remove(&tick_upper);
            }
        }

        Ok(())
    }

    /// Updates the pool state from a swap event log.
    pub fn sync_from_swap_log(&mut self, log: Log) -> Result<(), alloy::sol_types::Error> {
        let event = IUniswapV3Pool::Swap::decode_log(log.as_ref())?;

        let zero_for_one = event.amount1 <= I256::ZERO;
        let (amount_specified, amount_in_from_log, limit) = if zero_for_one {
            if event.amount1.is_zero() {
                (event.amount0, None, MIN_SQRT_RATIO + U256::ONE)
            } else {
                (
                    event.amount1,
                    Some(event.amount0),
                    U256::from(event.sqrtPriceX96),
                )
            }
        } else {
            if event.amount0.is_zero() {
                (event.amount1, None, MAX_SQRT_RATIO - U256::ONE)
            } else {
                (
                    event.amount0,
                    Some(event.amount1),
                    U256::from(event.sqrtPriceX96),
                )
            }
        };

        dbg!(&event);
        // Edge case where both amounts are zero, we need to continue because sqrt_price_limit can be updated by this
        if !amount_specified.is_zero() {
            let (a0, a1) = self
                .swap(
                    event.recipient,
                    zero_for_one,
                    amount_specified,
                    limit,
                    log.block_timestamp.unwrap(),
                    amount_in_from_log,
                )
                .map_err(|e| alloy::sol_types::Error::custom(e.to_string()))?;
            dbg!(self.slot0.sqrt_price_x96);
            dbg!(self.liquidity);
            dbg!(self.slot0.tick);
            dbg!(a0);
            dbg!(a1);
            assert_eq!(a0, event.amount0);
            assert_eq!(a1, event.amount1);
        }
        assert_eq!(self.liquidity, event.liquidity);

        // we can not assert because of the edge case where liquidity is zero.
        // we simply cannot know where the swap stopped, so we assert other data points
        // are correct and trust these ones.
        // A little sanity check

        if self.slot0.sqrt_price_x96 != event.sqrtPriceX96.to() {
            assert_eq!(self.liquidity, 0);
        }
        self.slot0.sqrt_price_x96 = event.sqrtPriceX96.to();
        self.slot0.tick = event.tick.as_i32();

        tracing::debug!(?event, address = ?self.address, sqrt_price = ?self.slot0.sqrt_price_x96, liquidity = ?self.liquidity, tick = ?self.slot0.tick, "UniswapV3 swap event");

        Ok(())
    }

    pub fn sync_from_flash_log(&mut self, log: Log) -> Result<(), alloy::sol_types::Error> {
        let event = IUniswapV3Pool::Flash::decode_log(log.as_ref())?;

        match self.flash(
            event.recipient,
            event.amount0,
            event.amount1,
            event.paid0,
            event.paid1,
        ) {
            Ok(_) => {}
            Err(e) => return Err(alloy::sol_types::Error::custom(e.to_string())),
        }

        tracing::debug!(?event, address = ?self.address, sqrt_price = ?self.slot0.sqrt_price_x96, liquidity = ?self.liquidity, tick = ?self.slot0.tick, "UniswapV3 flash event");

        Ok(())
    }
    pub fn sync_from_collect_log(&mut self, log: Log) -> Result<(), alloy::sol_types::Error> {
        let event = IUniswapV3Pool::Collect::decode_log(log.as_ref())?;

        let (amount_0, amount_1) = self.collect(
            event.owner,
            event.recipient,
            event.tickLower.as_i32(),
            event.tickUpper.as_i32(),
            event.amount0,
            event.amount1,
        );
        assert_eq!(amount_0, event.amount0);
        assert_eq!(amount_1, event.amount1);

        tracing::debug!(?event, address = ?self.address, sqrt_price = ?self.slot0.sqrt_price_x96, liquidity = ?self.liquidity, tick = ?self.slot0.tick, "UniswapV3 collect event");

        Ok(())
    }

    pub fn sync_from_collect_protocol_log(
        &mut self,
        log: Log,
    ) -> Result<(), alloy::sol_types::Error> {
        let event = IUniswapV3Pool::CollectProtocol::decode_log(log.as_ref())?;

        self.protocol_fees.token0 -= event.amount0;
        self.protocol_fees.token1 -= event.amount1;
        tracing::debug!(?event, address = ?self.address, sqrt_price = ?self.slot0.sqrt_price_x96, liquidity = ?self.liquidity, tick = ?self.slot0.tick, "UniswapV3 CollectProtocol event");

        Ok(())
    }

    pub fn sync_from_set_fee_protocol_log(
        &mut self,
        log: Log,
    ) -> Result<(), alloy::sol_types::Error> {
        let event = IUniswapV3Pool::SetFeeProtocol::decode_log(log.as_ref())?;

        self.slot0.fee_protocol = event.feeProtocol0New + (event.feeProtocol1New << 4);
        tracing::debug!(?event, address = ?self.address, sqrt_price = ?self.slot0.sqrt_price_x96, liquidity = ?self.liquidity, tick = ?self.slot0.tick, "UniswapV3 SetFeeProtocol event");

        Ok(())
    }

    pub fn sync_from_increase_observation_cardinality_next_log(
        &mut self,
        log: Log,
    ) -> Result<(), alloy::sol_types::Error> {
        let event = IUniswapV3Pool::IncreaseObservationCardinalityNext::decode_log(log.as_ref())?;

        self.observations.grow(
            event.observationCardinalityNextOld,
            event.observationCardinalityNextNew,
        );
        self.slot0.observation_cardinality_next = event.observationCardinalityNextNew;
        tracing::debug!(?event, address = ?self.address, sqrt_price = ?self.slot0.sqrt_price_x96, liquidity = ?self.liquidity, tick = ?self.slot0.tick, "UniswapV3 IncreaseObservationCardinalityNext event");

        Ok(())
    }

    pub async fn get_token_decimals<N, P>(&mut self, provider: Arc<P>) -> Result<(u8, u8), AMMError>
    where
        N: Network,
        P: Provider<N>,
    {
        let token_a_decimals = IErc20::new(self.token_a, provider.clone())
            .decimals()
            .call()
            .await?;

        let token_b_decimals = IErc20::new(self.token_b, provider)
            .decimals()
            .call()
            .await?;

        Ok((token_a_decimals, token_b_decimals))
    }

    pub async fn get_fee<N, P>(&mut self, provider: Arc<P>) -> Result<u32, AMMError>
    where
        N: Network,
        P: Provider<N>,
    {
        let fee = IUniswapV3Pool::new(self.address, provider)
            .fee()
            .call()
            .await?;

        Ok(fee.to())
    }

    pub async fn get_token_0<N, P>(&self, provider: Arc<P>) -> Result<Address, AMMError>
    where
        N: Network,
        P: Provider<N>,
    {
        let v3_pool = IUniswapV3Pool::new(self.address, provider);

        let token_0 = match v3_pool.token0().call().await {
            Ok(result) => result,
            Err(contract_error) => return Err(AMMError::ContractError(contract_error)),
        };

        Ok(token_0)
    }

    pub async fn get_token_1<N, P>(&self, provider: Arc<P>) -> Result<Address, AMMError>
    where
        N: Network,
        P: Provider<N>,
    {
        let v3_pool = IUniswapV3Pool::new(self.address, provider);

        let token_1 = match v3_pool.token1().call().await {
            Ok(result) => result,
            Err(contract_error) => return Err(AMMError::ContractError(contract_error)),
        };

        Ok(token_1)
    }
    /* Legend:
       sqrt(price) = sqrt(y/x)
       L = sqrt(x*y)
       ==> x = L^2/price
       ==> y = L^2*price
    */
    pub fn calculate_virtual_reserves(&self) -> Result<(u128, u128), ArithmeticError> {
        let tick = uniswap_v3_math::tick_math::get_tick_at_sqrt_ratio(self.slot0.sqrt_price_x96)?;
        let price = 1.0001_f64.powi(tick);

        let sqrt_price = BigFloat::from_f64(price.sqrt());

        //Sqrt price is stored as a Q64.96 so we need to left shift the liquidity by 96 to be represented as Q64.96
        //We cant right shift sqrt_price because it could move the value to 0, making division by 0 to get reserve_x
        let liquidity = BigFloat::from_u128(self.liquidity);

        let (reserve_0, reserve_1) = if !sqrt_price.is_zero() {
            let reserve_x = liquidity.div(&sqrt_price);
            let reserve_y = liquidity.mul(&sqrt_price);

            (reserve_x, reserve_y)
        } else {
            (BigFloat::from(0), BigFloat::from(0))
        };

        Ok((
            reserve_0
                .to_u128()
                .ok_or(ArithmeticError::U128ConversionError)?,
            reserve_1
                .to_u128()
                .ok_or(ArithmeticError::U128ConversionError)?,
        ))
    }

    pub fn calculate_compressed(&self, tick: i32) -> i32 {
        if tick < 0 && tick % self.tick_spacing != 0 {
            (tick / self.tick_spacing) - 1
        } else {
            tick / self.tick_spacing
        }
    }

    pub fn calculate_word_pos_bit_pos(&self, compressed: i32) -> (i16, u8) {
        uniswap_v3_math::tick_bitmap::position(compressed)
    }

    /// Returns the call data for a swap.
    pub fn swap_calldata(
        &self,
        recipient: Address,
        zero_for_one: bool,
        amount_specified: I256,
        sqrt_price_limit_x_96: U256,
        calldata: Vec<u8>,
    ) -> Result<Bytes, alloy::dyn_abi::Error> {
        Ok(IUniswapV3Pool::swapCall {
            recipient,
            zeroForOne: zero_for_one,
            amountSpecified: amount_specified,
            sqrtPriceLimitX96: sqrt_price_limit_x_96.to(),
            data: calldata.into(),
        }
        .abi_encode()
        .into())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProtocolFees {
    pub token0: u128,
    pub token1: u128,
}

#[derive(Debug, Copy, Clone, Default, Serialize, Deserialize)]
pub struct Slot0 {
    sqrt_price_x96: U256,
    tick: i32,
    observation_index: u16,
    observation_cardinality: u16,
    observation_cardinality_next: u16,
    fee_protocol: u8,
    unlocked: bool,
}

impl Into<U256> for Slot0 {
    fn into(self) -> U256 {
        let mut data = self.sqrt_price_x96;
        let tick: U256 = I24::unchecked_from(self.tick).into_raw().to();
        data |= tick << 160;
        let mut obs = self.observation_index as u64;
        obs |= (self.observation_cardinality as u64) << 16;
        obs |= (self.observation_cardinality_next as u64) << 32;
        obs |= (self.fee_protocol as u64) << 48;
        obs |= (self.unlocked as u64) << 56;
        data |= U256::from(obs) << 184;
        data
    }
}

pub struct SwapCache {
    fee_protocol: u8,
    liquidity_start: u128,
    block_timestamp: u32,
    tick_cumulative: i64,
    seconds_per_liquidity_cumulative_x128: U256,
    computed_last_observations: bool,
}

pub struct SwapState {
    pub amount_specified_remaining: I256,
    pub amount_calculated: I256,
    pub sqrt_price_x_96: U256,
    pub tick: i32,
    pub fee_growth_global_x128: U256,
    pub protocol_fee: u128,
    pub liquidity: u128,
}

#[derive(Default)]
pub struct StepComputations {
    pub sqrt_price_start_x_96: U256,
    pub tick_next: i32,
    pub initialized: bool,
    pub sqrt_price_next_x96: U256,
    pub amount_in: U256,
    pub amount_out: U256,
    pub fee_amount: U256,
}

#[cfg(test)]
mod test {

    use alloy::{
        primitives::{address, aliases::U24, U256},
        providers::ProviderBuilder,
    };

    use super::*;

    sol! {
        /// Interface of the Quoter
        #[derive(Debug, PartialEq, Eq)]
        #[sol(rpc)]
        contract IQuoter {
            function quoteExactInputSingle(address tokenIn, address tokenOut,uint24 fee, uint256 amountIn, uint160 sqrtPriceLimitX96) external returns (uint256 amountOut);
        }
    }

    async fn initialize_usdc_weth_pool<N, P>(provider: Arc<P>) -> eyre::Result<(UniswapV3Pool, u64)>
    where
        N: Network,
        P: Provider<N>,
    {
        let mut pool = UniswapV3Pool {
            address: address!("88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640"),
            ..Default::default()
        };

        let creation_block = 12369620;
        pool.tick_spacing = pool.get_tick_spacing(provider.clone()).await?;
        let synced_block = pool
            .populate_tick_data(creation_block, provider.clone())
            .await?;
        pool.populate_data(Some(synced_block), provider).await?;

        Ok((pool, synced_block))
    }

    async fn initialize_weth_link_pool<N, P>(provider: Arc<P>) -> eyre::Result<(UniswapV3Pool, u64)>
    where
        N: Network,
        P: Provider<N>,
    {
        let mut pool = UniswapV3Pool {
            address: address!("a6Cc3C2531FdaA6Ae1A3CA84c2855806728693e8"),
            ..Default::default()
        };

        let creation_block = 12375680;
        pool.tick_spacing = pool.get_tick_spacing(provider.clone()).await?;
        let synced_block = pool
            .populate_tick_data(creation_block, provider.clone())
            .await?;
        pool.populate_data(Some(synced_block), provider).await?;

        Ok((pool, synced_block))
    }

    #[tokio::test]
    #[ignore] // Ignoring to not throttle the Provider on workflows
    async fn test_simulate_swap_usdc_weth() {
        let rpc_endpoint = std::env::var("ETHEREUM_RPC_ENDPOINT").unwrap();
        let provider = Arc::new(ProviderBuilder::new().on_http(rpc_endpoint.parse().unwrap()));

        let (pool, synced_block) = initialize_usdc_weth_pool(provider.clone()).await.unwrap();
        let quoter = IQuoter::new(
            address!("b27308f9f90d607463bb33ea1bebb41c27ce5ab6"),
            provider.clone(),
        );

        let amount_in = U256::from(100000000); // 100 USDC
        let amount_out = pool.simulate_swap(pool.token_a, amount_in).unwrap();
        let expected_amount_out = quoter
            .quoteExactInputSingle(
                pool.token_a,
                pool.token_b,
                U24::from(pool.fee),
                amount_in,
                U160::ZERO,
            )
            .block(synced_block.into())
            .call()
            .await
            .unwrap();

        assert_eq!(amount_out, expected_amount_out);

        let amount_in_1 = U256::from(10000000000_u64); // 10_000 USDC
        let amount_out_1 = pool.simulate_swap(pool.token_a, amount_in_1).unwrap();
        let expected_amount_out_1 = quoter
            .quoteExactInputSingle(
                pool.token_a,
                pool.token_b,
                pool.fee,
                amount_in_1,
                U256::ZERO,
            )
            .block(synced_block.into())
            .call()
            .await
            .unwrap();

        assert_eq!(amount_out_1, expected_amount_out_1.amountOut);

        let amount_in_2 = U256::from(10000000000000_u128); // 10_000_000 USDC
        let amount_out_2 = pool.simulate_swap(pool.token_a, amount_in_2).unwrap();
        let expected_amount_out_2 = quoter
            .quoteExactInputSingle(
                pool.token_a,
                pool.token_b,
                pool.fee,
                amount_in_2,
                U256::ZERO,
            )
            .block(synced_block.into())
            .call()
            .await
            .unwrap();

        assert_eq!(amount_out_2, expected_amount_out_2.amountOut);

        let amount_in_3 = U256::from(100000000000000_u128); // 100_000_000 USDC
        let amount_out_3 = pool.simulate_swap(pool.token_a, amount_in_3).unwrap();
        let expected_amount_out_3 = quoter
            .quoteExactInputSingle(
                pool.token_a,
                pool.token_b,
                pool.fee,
                amount_in_3,
                U256::ZERO,
            )
            .block(synced_block.into())
            .call()
            .await
            .unwrap();

        assert_eq!(amount_out_3, expected_amount_out_3.amountOut);
    }

    #[tokio::test]
    #[ignore] // Ignoring to not throttle the Provider on workflows
    async fn test_simulate_swap_weth_usdc() {
        let rpc_endpoint = std::env::var("ETHEREUM_RPC_ENDPOINT").unwrap();
        let provider = Arc::new(ProviderBuilder::new().on_http(rpc_endpoint.parse().unwrap()));

        let (pool, synced_block) = initialize_usdc_weth_pool(provider.clone()).await.unwrap();
        let quoter = IQuoter::new(
            address!("b27308f9f90d607463bb33ea1bebb41c27ce5ab6"),
            provider.clone(),
        );

        let amount_in = U256::from(1000000000000000000_u128); // 1 ETH
        let amount_out = pool.simulate_swap(pool.token_b, amount_in).unwrap();
        let expected_amount_out = quoter
            .quoteExactInputSingle(pool.token_b, pool.token_a, pool.fee, amount_in, U256::ZERO)
            .block(synced_block.into())
            .call()
            .await
            .unwrap();

        assert_eq!(amount_out, expected_amount_out.amountOut);

        let amount_in_1 = U256::from(10000000000000000000_u128); // 10 ETH
        let amount_out_1 = pool.simulate_swap(pool.token_b, amount_in_1).unwrap();
        let expected_amount_out_1 = quoter
            .quoteExactInputSingle(
                pool.token_b,
                pool.token_a,
                pool.fee,
                amount_in_1,
                U256::ZERO,
            )
            .block(synced_block.into())
            .call()
            .await
            .unwrap();

        assert_eq!(amount_out_1, expected_amount_out_1.amountOut);

        let amount_in_2 = U256::from(100000000000000000000_u128); // 100 ETH
        let amount_out_2 = pool.simulate_swap(pool.token_b, amount_in_2).unwrap();
        let expected_amount_out_2 = quoter
            .quoteExactInputSingle(
                pool.token_b,
                pool.token_a,
                pool.fee,
                amount_in_2,
                U256::ZERO,
            )
            .block(synced_block.into())
            .call()
            .await
            .unwrap();

        assert_eq!(amount_out_2, expected_amount_out_2.amountOut);

        let amount_in_3 = U256::from(100000000000000000000_u128); // 100_000 ETH
        let amount_out_3 = pool.simulate_swap(pool.token_b, amount_in_3).unwrap();
        let expected_amount_out_3 = quoter
            .quoteExactInputSingle(
                pool.token_b,
                pool.token_a,
                pool.fee,
                amount_in_3,
                U256::ZERO,
            )
            .block(synced_block.into())
            .call()
            .await
            .unwrap();

        assert_eq!(amount_out_3, expected_amount_out_3.amountOut);
    }

    #[tokio::test]
    #[ignore] // Ignoring to not throttle the Provider on workflows
    async fn test_simulate_swap_link_weth() {
        let rpc_endpoint = std::env::var("ETHEREUM_RPC_ENDPOINT").unwrap();
        let provider = Arc::new(ProviderBuilder::new().on_http(rpc_endpoint.parse().unwrap()));

        let (pool, synced_block) = initialize_weth_link_pool(provider.clone()).await.unwrap();
        let quoter = IQuoter::new(
            address!("b27308f9f90d607463bb33ea1bebb41c27ce5ab6"),
            provider.clone(),
        );

        let amount_in = U256::from(1000000000000000000_u128); // 1 LINK
        let amount_out = pool.simulate_swap(pool.token_a, amount_in).unwrap();
        let expected_amount_out = quoter
            .quoteExactInputSingle(pool.token_a, pool.token_b, pool.fee, amount_in, U256::ZERO)
            .block(synced_block.into())
            .call()
            .await
            .unwrap();

        assert_eq!(amount_out, expected_amount_out.amountOut);

        let amount_in_1 = U256::from(100000000000000000000_u128); // 100 LINK
        let amount_out_1 = pool.simulate_swap(pool.token_a, amount_in_1).unwrap();
        let expected_amount_out_1 = quoter
            .quoteExactInputSingle(
                pool.token_a,
                pool.token_b,
                pool.fee,
                amount_in_1,
                U256::ZERO,
            )
            .block(synced_block.into())
            .call()
            .await
            .unwrap();

        assert_eq!(amount_out_1, expected_amount_out_1.amountOut);

        let amount_in_2 = U256::from(10000000000000000000000_u128); // 10_000 LINK
        let amount_out_2 = pool.simulate_swap(pool.token_a, amount_in_2).unwrap();
        let expected_amount_out_2 = quoter
            .quoteExactInputSingle(
                pool.token_a,
                pool.token_b,
                pool.fee,
                amount_in_2,
                U256::ZERO,
            )
            .block(synced_block.into())
            .call()
            .await
            .unwrap();

        assert_eq!(amount_out_2, expected_amount_out_2.amountOut);

        let amount_in_3 = U256::from(10000000000000000000000_u128); // 1_000_000 LINK
        let amount_out_3 = pool.simulate_swap(pool.token_a, amount_in_3).unwrap();
        let expected_amount_out_3 = quoter
            .quoteExactInputSingle(
                pool.token_a,
                pool.token_b,
                pool.fee,
                amount_in_3,
                U256::ZERO,
            )
            .block(synced_block.into())
            .call()
            .await
            .unwrap();

        assert_eq!(amount_out_3, expected_amount_out_3.amountOut);
    }

    #[tokio::test]
    #[ignore] // Ignoring to not throttle the Provider on workflows
    async fn test_simulate_swap_weth_link() {
        let rpc_endpoint = std::env::var("ETHEREUM_RPC_ENDPOINT").unwrap();
        let provider = Arc::new(ProviderBuilder::new().on_http(rpc_endpoint.parse().unwrap()));

        let (pool, synced_block) = initialize_weth_link_pool(provider.clone()).await.unwrap();
        let quoter = IQuoter::new(
            address!("b27308f9f90d607463bb33ea1bebb41c27ce5ab6"),
            provider.clone(),
        );

        let amount_in = U256::from(1000000000000000000_u128); // 1 ETH
        let amount_out = pool.simulate_swap(pool.token_b, amount_in).unwrap();
        let expected_amount_out = quoter
            .quoteExactInputSingle(pool.token_b, pool.token_a, pool.fee, amount_in, U256::ZERO)
            .block(synced_block.into())
            .call()
            .await
            .unwrap();

        assert_eq!(amount_out, expected_amount_out.amountOut);

        let amount_in_1 = U256::from(10000000000000000000_u128); // 10 ETH
        let amount_out_1 = pool.simulate_swap(pool.token_b, amount_in_1).unwrap();
        let expected_amount_out_1 = quoter
            .quoteExactInputSingle(
                pool.token_b,
                pool.token_a,
                pool.fee,
                amount_in_1,
                U256::ZERO,
            )
            .block(synced_block.into())
            .call()
            .await
            .unwrap();

        assert_eq!(amount_out_1, expected_amount_out_1.amountOut);

        let amount_in_2 = U256::from(100000000000000000000_u128); // 100 ETH
        let amount_out_2 = pool.simulate_swap(pool.token_b, amount_in_2).unwrap();
        let expected_amount_out_2 = quoter
            .quoteExactInputSingle(
                pool.token_b,
                pool.token_a,
                pool.fee,
                amount_in_2,
                U256::ZERO,
            )
            .block(synced_block.into())
            .call()
            .await
            .unwrap();

        assert_eq!(amount_out_2, expected_amount_out_2.amountOut);

        let amount_in_3 = U256::from(100000000000000000000_u128); // 100_000 ETH
        let amount_out_3 = pool.simulate_swap(pool.token_b, amount_in_3).unwrap();
        let expected_amount_out_3 = quoter
            .quoteExactInputSingle(
                pool.token_b,
                pool.token_a,
                pool.fee,
                amount_in_3,
                U256::ZERO,
            )
            .block(synced_block.into())
            .call()
            .await
            .unwrap();

        assert_eq!(amount_out_3, expected_amount_out_3.amountOut);
    }

    #[tokio::test]
    #[ignore] // Ignoring to not throttle the Provider on workflows
    async fn test_simulate_swap_mut_usdc_weth() {
        let rpc_endpoint = std::env::var("ETHEREUM_RPC_ENDPOINT").unwrap();
        let provider = Arc::new(ProviderBuilder::new().on_http(rpc_endpoint.parse().unwrap()));

        let (pool, synced_block) = initialize_usdc_weth_pool(provider.clone()).await.unwrap();
        let quoter = IQuoter::new(
            address!("b27308f9f90d607463bb33ea1bebb41c27ce5ab6"),
            provider.clone(),
        );

        let amount_in = U256::from(100000000_u64); // 100 USDC
        let amount_out = pool.simulate_swap(pool.token_a, amount_in).unwrap();
        let expected_amount_out = quoter
            .quoteExactInputSingle(pool.token_a, pool.token_b, pool.fee, amount_in, U256::ZERO)
            .block(synced_block.into())
            .call()
            .await
            .unwrap();

        assert_eq!(amount_out, expected_amount_out.amountOut);

        let amount_in_1 = U256::from(10000000000_u128); // 10_000 USDC
        let amount_out_1 = pool.simulate_swap(pool.token_a, amount_in_1).unwrap();
        let expected_amount_out_1 = quoter
            .quoteExactInputSingle(
                pool.token_a,
                pool.token_b,
                pool.fee,
                amount_in_1,
                U256::ZERO,
            )
            .block(synced_block.into())
            .call()
            .await
            .unwrap();

        assert_eq!(amount_out_1, expected_amount_out_1.amountOut);

        let amount_in_2 = U256::from(10000000000000_u128); // 10_000_000 USDC
        let amount_out_2 = pool.simulate_swap(pool.token_a, amount_in_2).unwrap();
        let expected_amount_out_2 = quoter
            .quoteExactInputSingle(
                pool.token_a,
                pool.token_b,
                pool.fee,
                amount_in_2,
                U256::ZERO,
            )
            .block(synced_block.into())
            .call()
            .await
            .unwrap();

        assert_eq!(amount_out_2, expected_amount_out_2.amountOut);

        let amount_in_3 = U256::from(100000000000000_u128); // 100_000_000 USDC
        let amount_out_3 = pool.simulate_swap(pool.token_a, amount_in_3).unwrap();
        let expected_amount_out_3 = quoter
            .quoteExactInputSingle(
                pool.token_a,
                pool.token_b,
                pool.fee,
                amount_in_3,
                U256::ZERO,
            )
            .block(synced_block.into())
            .call()
            .await
            .unwrap();

        assert_eq!(amount_out_3, expected_amount_out_3.amountOut);
    }

    #[tokio::test]
    #[ignore] // Ignoring to not throttle the Provider on workflows
    async fn test_simulate_swap_mut_weth_usdc() {
        let rpc_endpoint = std::env::var("ETHEREUM_RPC_ENDPOINT").unwrap();
        let provider = Arc::new(ProviderBuilder::new().on_http(rpc_endpoint.parse().unwrap()));

        let (pool, synced_block) = initialize_usdc_weth_pool(provider.clone()).await.unwrap();
        let quoter = IQuoter::new(
            address!("b27308f9f90d607463bb33ea1bebb41c27ce5ab6"),
            provider.clone(),
        );

        let amount_in = U256::from(1000000000000000000_u128); // 1 ETH
        let amount_out = pool.simulate_swap(pool.token_b, amount_in).unwrap();
        let expected_amount_out = quoter
            .quoteExactInputSingle(pool.token_b, pool.token_a, pool.fee, amount_in, U256::ZERO)
            .block(synced_block.into())
            .call()
            .await
            .unwrap();

        assert_eq!(amount_out, expected_amount_out.amountOut);

        let amount_in_1 = U256::from(10000000000000000000_u128); // 10 ETH
        let amount_out_1 = pool.simulate_swap(pool.token_b, amount_in_1).unwrap();
        let expected_amount_out_1 = quoter
            .quoteExactInputSingle(
                pool.token_b,
                pool.token_a,
                pool.fee,
                amount_in_1,
                U256::ZERO,
            )
            .block(synced_block.into())
            .call()
            .await
            .unwrap();

        assert_eq!(amount_out_1, expected_amount_out_1.amountOut);

        let amount_in_2 = U256::from(100000000000000000000_u128); // 100 ETH
        let amount_out_2 = pool.simulate_swap(pool.token_b, amount_in_2).unwrap();
        let expected_amount_out_2 = quoter
            .quoteExactInputSingle(
                pool.token_b,
                pool.token_a,
                pool.fee,
                amount_in_2,
                U256::ZERO,
            )
            .block(synced_block.into())
            .call()
            .await
            .unwrap();

        assert_eq!(amount_out_2, expected_amount_out_2.amountOut);

        let amount_in_3 = U256::from(100000000000000000000_u128); // 100_000 ETH
        let amount_out_3 = pool.simulate_swap(pool.token_b, amount_in_3).unwrap();
        let expected_amount_out_3 = quoter
            .quoteExactInputSingle(
                pool.token_b,
                pool.token_a,
                pool.fee,
                amount_in_3,
                U256::ZERO,
            )
            .block(synced_block.into())
            .call()
            .await
            .unwrap();

        assert_eq!(amount_out_3, expected_amount_out_3.amountOut);
    }

    #[tokio::test]
    #[ignore] // Ignoring to not throttle the Provider on workflows
    async fn test_simulate_swap_mut_link_weth() {
        let rpc_endpoint = std::env::var("ETHEREUM_RPC_ENDPOINT").unwrap();
        let provider = Arc::new(ProviderBuilder::new().on_http(rpc_endpoint.parse().unwrap()));

        let (pool, synced_block) = initialize_weth_link_pool(provider.clone()).await.unwrap();
        let quoter = IQuoter::new(
            address!("b27308f9f90d607463bb33ea1bebb41c27ce5ab6"),
            provider.clone(),
        );

        let amount_in = U256::from(1000000000000000000_u128); // 1 LINK
        let amount_out = pool.simulate_swap(pool.token_a, amount_in).unwrap();
        let expected_amount_out = quoter
            .quoteExactInputSingle(pool.token_a, pool.token_b, pool.fee, amount_in, U256::ZERO)
            .block(synced_block.into())
            .call()
            .await
            .unwrap();

        assert_eq!(amount_out, expected_amount_out.amountOut);

        let amount_in_1 = U256::from(100000000000000000000_u128); // 100 LINK
        let amount_out_1 = pool.simulate_swap(pool.token_a, amount_in_1).unwrap();
        let expected_amount_out_1 = quoter
            .quoteExactInputSingle(
                pool.token_a,
                pool.token_b,
                pool.fee,
                amount_in_1,
                U256::ZERO,
            )
            .block(synced_block.into())
            .call()
            .await
            .unwrap();

        assert_eq!(amount_out_1, expected_amount_out_1.amountOut);

        let amount_in_2 = U256::from(10000000000000000000000_u128); // 10_000 LINK
        let amount_out_2 = pool.simulate_swap(pool.token_a, amount_in_2).unwrap();
        let expected_amount_out_2 = quoter
            .quoteExactInputSingle(
                pool.token_a,
                pool.token_b,
                pool.fee,
                amount_in_2,
                U256::ZERO,
            )
            .block(synced_block.into())
            .call()
            .await
            .unwrap();

        assert_eq!(amount_out_2, expected_amount_out_2.amountOut);

        let amount_in_3 = U256::from(10000000000000000000000_u128); // 1_000_000 LINK
        let amount_out_3 = pool.simulate_swap(pool.token_a, amount_in_3).unwrap();
        let expected_amount_out_3 = quoter
            .quoteExactInputSingle(
                pool.token_a,
                pool.token_b,
                pool.fee,
                amount_in_3,
                U256::ZERO,
            )
            .block(synced_block.into())
            .call()
            .await
            .unwrap();

        assert_eq!(amount_out_3, expected_amount_out_3.amountOut);
    }

    #[tokio::test]
    #[ignore] // Ignoring to not throttle the Provider on workflows
    async fn test_simulate_swap_mut_weth_link() {
        let rpc_endpoint = std::env::var("ETHEREUM_RPC_ENDPOINT").unwrap();
        let provider = Arc::new(ProviderBuilder::new().on_http(rpc_endpoint.parse().unwrap()));

        let (pool, synced_block) = initialize_weth_link_pool(provider.clone()).await.unwrap();
        let quoter = IQuoter::new(
            address!("b27308f9f90d607463bb33ea1bebb41c27ce5ab6"),
            provider.clone(),
        );

        let amount_in = U256::from(1000000000000000000_u128); // 1 ETH
        let amount_out = pool.simulate_swap(pool.token_b, amount_in).unwrap();
        let expected_amount_out = quoter
            .quoteExactInputSingle(pool.token_b, pool.token_a, pool.fee, amount_in, U256::ZERO)
            .block(synced_block.into())
            .call()
            .await
            .unwrap();

        assert_eq!(amount_out, expected_amount_out.amountOut);

        let amount_in_1 = U256::from(10000000000000000000_u128); // 10 ETH
        let amount_out_1 = pool.simulate_swap(pool.token_b, amount_in_1).unwrap();
        let expected_amount_out_1 = quoter
            .quoteExactInputSingle(
                pool.token_b,
                pool.token_a,
                pool.fee,
                amount_in_1,
                U256::ZERO,
            )
            .block(synced_block.into())
            .call()
            .await
            .unwrap();

        assert_eq!(amount_out_1, expected_amount_out_1.amountOut);

        let amount_in_2 = U256::from(100000000000000000000_u128); // 100 ETH
        let amount_out_2 = pool.simulate_swap(pool.token_b, amount_in_2).unwrap();
        let expected_amount_out_2 = quoter
            .quoteExactInputSingle(
                pool.token_b,
                pool.token_a,
                pool.fee,
                amount_in_2,
                U256::ZERO,
            )
            .block(synced_block.into())
            .call()
            .await
            .unwrap();

        assert_eq!(amount_out_2, expected_amount_out_2.amountOut);

        let amount_in_3 = U256::from(100000000000000000000_u128); // 100_000 ETH
        let amount_out_3 = pool.simulate_swap(pool.token_b, amount_in_3).unwrap();
        let expected_amount_out_3 = quoter
            .quoteExactInputSingle(
                pool.token_b,
                pool.token_a,
                pool.fee,
                amount_in_3,
                U256::ZERO,
            )
            .block(synced_block.into())
            .call()
            .await
            .unwrap();

        assert_eq!(amount_out_3, expected_amount_out_3.amountOut);
    }

    #[tokio::test]
    #[ignore] // Ignoring to not throttle the Provider on workflows
    async fn test_get_new_from_address() {
        let rpc_endpoint = std::env::var("ETHEREUM_RPC_ENDPOINT").unwrap();
        let provider = Arc::new(ProviderBuilder::new().on_http(rpc_endpoint.parse().unwrap()));

        let pool = UniswapV3Pool::new_from_address(
            address!("88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640"),
            12369620,
            provider.clone(),
        )
        .await
        .unwrap();

        assert_eq!(
            pool.address,
            address!("88e6a0c2ddd26feeb64f039a2c41296fcb3f5640")
        );
        assert_eq!(
            pool.token_a,
            address!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48")
        );
        assert_eq!(pool.token_a_decimals, 6);
        assert_eq!(
            pool.token_b,
            address!("c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2")
        );
        assert_eq!(pool.token_b_decimals, 18);
        assert_eq!(pool.fee, 500);
        assert!(pool.slot0.tick != 0);
        assert_eq!(pool.tick_spacing, 10);
    }

    #[tokio::test]
    #[ignore] // Ignoring to not throttle the Provider on workflows
    async fn test_get_pool_data() {
        let rpc_endpoint = std::env::var("ETHEREUM_RPC_ENDPOINT").unwrap();
        let provider = Arc::new(ProviderBuilder::new().on_http(rpc_endpoint.parse().unwrap()));

        let (pool, _synced_block) = initialize_usdc_weth_pool(provider.clone()).await.unwrap();
        assert_eq!(
            pool.address,
            address!("88e6a0c2ddd26feeb64f039a2c41296fcb3f5640")
        );
        assert_eq!(
            pool.token_a,
            address!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48")
        );
        assert_eq!(pool.token_a_decimals, 6);
        assert_eq!(
            pool.token_b,
            address!("c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2")
        );
        assert_eq!(pool.token_b_decimals, 18);
        assert_eq!(pool.fee, 500);
        assert!(pool.slot0.tick != 0);
        assert_eq!(pool.tick_spacing, 10);
    }

    #[tokio::test]
    async fn test_sync_pool() {
        let rpc_endpoint = std::env::var("ETHEREUM_RPC_ENDPOINT").unwrap();
        let provider = Arc::new(ProviderBuilder::new().on_http(rpc_endpoint.parse().unwrap()));

        let mut pool = UniswapV3Pool {
            address: address!("88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640"),
            ..Default::default()
        };

        pool.sync(provider).await.unwrap();

        //TODO: need to assert values
    }

    #[tokio::test]
    async fn test_calculate_virtual_reserves() {
        let rpc_endpoint = std::env::var("ETHEREUM_RPC_ENDPOINT").unwrap();
        let provider = Arc::new(ProviderBuilder::new().on_http(rpc_endpoint.parse().unwrap()));

        let mut pool = UniswapV3Pool {
            address: address!("88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640"),
            ..Default::default()
        };

        pool.populate_data(None, provider.clone()).await.unwrap();

        let pool_at_block = IUniswapV3Pool::new(
            address!("88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640"),
            provider.clone(),
        );

        let sqrt_price = pool_at_block
            .slot0()
            .block(16515398.into())
            .call()
            .await
            .unwrap();

        let liquidity = pool_at_block
            .liquidity()
            .block(16515398.into())
            .call()
            .await
            .unwrap();

        pool.slot0.sqrt_price_x96 = U256::from(sqrt_price._0);
        pool.liquidity = liquidity;

        let (r_0, r_1) = pool.calculate_virtual_reserves().unwrap();

        assert_eq!(1067543429906214, r_0);
        assert_eq!(649198362624067343572319, r_1);
    }

    #[tokio::test]
    async fn test_calculate_price() {
        let rpc_endpoint = std::env::var("ETHEREUM_RPC_ENDPOINT").unwrap();
        let provider = Arc::new(ProviderBuilder::new().on_http(rpc_endpoint.parse().unwrap()));

        let mut pool = UniswapV3Pool {
            address: address!("88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640"),
            ..Default::default()
        };

        pool.populate_data(None, provider.clone()).await.unwrap();

        let block_pool = IUniswapV3Pool::new(
            address!("88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640"),
            provider.clone(),
        );

        let sqrt_price = block_pool
            .slot0()
            .block(16515398.into())
            .call()
            .await
            .unwrap();

        pool.slot0.sqrt_price_x96 = U256::from(sqrt_price._0);

        let float_price_a = pool.calculate_price(pool.token_a).unwrap();
        let float_price_b = pool.calculate_price(pool.token_b).unwrap();

        assert_eq!(float_price_a, 0.0006081236083117488);
        assert_eq!(float_price_b, 1644.4025299004006);
    }
}
