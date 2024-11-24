use crate::amm::uniswap_v2::UniswapV2Pool;
use crate::errors::SwapSimulationError;
use alloy::primitives::ruint::UintTryFrom;
use alloy::primitives::{Address, U128, U256};
#[derive(Debug, Clone, Copy)]
pub struct ShallowV2 {
    reserve_0: u128,
    reserve_1: u128,
}

impl ShallowV2 {
    pub fn new(base: &UniswapV2Pool) -> Self {
        Self {
            reserve_0: base.reserve_0,
            reserve_1: base.reserve_1,
        }
    }
    pub fn simulate_swap(
        &self,
        token_in: Address,
        amount_in: U256,
        base: &UniswapV2Pool,
    ) -> Result<(U256, Self), SwapSimulationError> {
        if base.token_a == token_in {
            let amount_out = base.get_amount_out(
                amount_in,
                U256::from(self.reserve_0),
                U256::from(self.reserve_1),
            );
            let reserve_0 = self.reserve_0
                + U128::uint_try_from(amount_in)
                    .map_err(|_| SwapSimulationError::ReserveOverflow)?
                    .to::<u128>();
            let reserve_1 = self.reserve_1
                - U128::uint_try_from(amount_out)
                    .map_err(|_| SwapSimulationError::ReserveOverflow)?
                    .to::<u128>();
            let state = ShallowV2 {
                reserve_0,
                reserve_1,
            };
            Ok((amount_out, state))
        } else {
            let amount_out = base.get_amount_out(
                amount_in,
                U256::from(self.reserve_1),
                U256::from(self.reserve_0),
            );

            let reserve_0 = self.reserve_0
                - U128::uint_try_from(amount_out)
                    .map_err(|_| SwapSimulationError::ReserveOverflow)?
                    .to::<u128>();
            let reserve_1 = self.reserve_1
                + U128::uint_try_from(amount_in)
                    .map_err(|_| SwapSimulationError::ReserveOverflow)?
                    .to::<u128>();
            let state = ShallowV2 {
                reserve_0,
                reserve_1,
            };
            Ok((amount_out, state))
        }
    }
}
