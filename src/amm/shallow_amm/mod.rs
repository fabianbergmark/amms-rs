use crate::amm::shallow_amm::shallow_v2::ShallowV2;
use crate::amm::shallow_amm::shallow_v3::ShallowV3;
use crate::amm::shallow_amm::ShallowAMM::{V2, V3};
use crate::amm::AMM;
use crate::errors::SwapSimulationError;
use crate::errors::SwapSimulationError::MixedTypes;
use alloy::primitives::{Address, U256};
use eyre::anyhow;

pub mod shallow_v2;
pub mod shallow_v3;
#[derive(Debug, Clone, Copy)]

pub enum ShallowAMM {
    V2(ShallowV2),
    V3(ShallowV3),
}

impl ShallowAMM {
    pub fn new(amm: &AMM) -> Self {
        match amm {
            AMM::UniswapV2Pool(amm) => V2(ShallowV2::new(amm)),
            AMM::UniswapV3Pool(amm) => V3(ShallowV3::new(amm)),
            AMM::ERC4626Vault(_) => todo!(),
        }
    }

    pub fn simulate_swap(
        &self,
        token_in: Address,
        amount_in: U256,
        base: &AMM,
    ) -> Result<(U256, Self), SwapSimulationError> {
        match (self, base) {
            (V2(state), AMM::UniswapV2Pool(base)) => state
                .simulate_swap(token_in, amount_in, base)
                .map(|(amount, pool)| (amount, V2(pool))),
            (V3(state), AMM::UniswapV3Pool(base)) => state
                .simulate_swap(token_in, amount_in, base)
                .map(|(amount, pool)| (amount, V3(pool))),
            _ => Err(MixedTypes),
        }
    }
}
