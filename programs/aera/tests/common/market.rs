//! The Tier 3 market oracle fixture: two mock Meteora pools and a reserve
//! priced from them.
//!
//! Shared by `test_market_oracle` (decoder, spacing, bootstrap) and
//! `test_market_oracle_attacks` (manipulation, crashes, integration). Both need
//! the same world, and duplicating it would let the two drift.

use crate::common::damm::*;
use crate::common::*;
use aera::state::{MarketOracleConfig, PoolRef};
use anchor_lang::prelude::Pubkey;

/// COOKHOUSE is 6 decimals and COOK is 9. Every price path has to survive that.
pub const COLLATERAL_DECIMALS: u8 = 6;
pub const QUOTE_DECIMALS: u8 = 9;

pub const AMM_PROGRAM_DATA: Pubkey = Pubkey::new_from_array([11u8; 32]);
pub const AMM_DEPLOY_SLOT: u64 = 3_176_591;

/// **TEST ONLY. NOT PRODUCTION CALIBRATED.**
///
/// Chosen so tests run in seconds, not so a market is safe. Real values need the
/// distributions `tools/oracle-calibrate.ts` is collecting; the live cross-pool
/// deviation alone measured 87.7 bps, and one earlier snapshot said 2 bps, which
/// is exactly why nothing here should be copied into a configuration.
pub fn test_market_config() -> MarketOracleConfig {
    MarketOracleConfig {
        amm_program: DAMM_PROGRAM,
        amm_program_data: AMM_PROGRAM_DATA,
        expected_deploy_slot: AMM_DEPLOY_SLOT,
        twap_window_seconds: 1_800,
        min_observations: 3,
        min_span_seconds: 300,
        min_spacing_seconds: 60,
        max_observation_age_seconds: 900,
        warn_age_seconds: 300,
        max_cross_pool_deviation_bps: 300,
        /*
         * QUOTE BASE UNITS, not whole tokens. 1e14 is 100,000 COOK at 9
         * decimals, against a fixture book of 850,000 COOK a side.
         *
         * An earlier value here was `100_000` -- i.e. 0.0001 COOK -- which no
         * pool could ever fall below, so every depth-degradation test passed
         * vacuously. The unit is the easiest thing to get wrong in this struct
         * and the hardest to notice, because getting it wrong disables a check
         * rather than breaking one.
         */
        min_pool_quote_depth: 100_000_000_000_000,
        max_rise_bps_per_window: 1_000,
    }
}

/// A market with two honest pools at the same price, ready to be refreshed.
pub struct Fixture {
    pub env: Env,
    pub cook: ReserveHandle,
    pub bcook: ReserveHandle,
    /// The market-priced reserve. Held so collateral and liquidation tests can
    /// use the same asset the oracle prices.
    pub collateral: ReserveHandle,
    pub pool_a: MockPool,
    pub pool_b: MockPool,
    pub collateral_mint: Pubkey,
    /// Reserves each pool started with, so a test can move from a known place.
    pub collateral_reserve: u64,
    pub quote_reserve: u64,
}

impl Fixture {
    pub fn new() -> Self {
        Self::with(Orientation::CollateralFirst, Orientation::QuoteFirst)
    }

    /// Both orientations by default: the two live COOKHOUSE pools disagree about
    /// which token is A, so a fixture that used one twice would test half the
    /// market.
    pub fn with(a: Orientation, b: Orientation) -> Self {
        let (mut env, cook, bcook) = Env::core(1_000);

        /*
         * The collateral asset is its own 6-decimal reserve, not bCOOK.
         *
         * Reusing bCOOK would be convenient and wrong twice over: it would test
         * the price path at 9 decimals when the real asset is 6, and it would
         * leave `core_00` -- which asserts a Core oracle cannot be refreshed
         * through this instruction -- pointing at an oracle this fixture had
         * already converted to `MarketTwap`.
         */
        let collateral = env.add_market_reserve(COLLATERAL_DECIMALS, bcook_config());
        let collateral_mint = collateral.mint;
        let quote_mint = cook.mint;

        let collateral_reserve = 30_000_000_000_000u64; // 30M at 6dp
        let quote_reserve = 850_000_000_000_000u64; // 850k at 9dp

        let pool_a = create_mock_damm_pool(
            &mut env.svm,
            &PoolSpec::new(
                Pubkey::new_from_array([21u8; 32]),
                collateral_mint,
                quote_mint,
                collateral_reserve,
                quote_reserve,
            )
            .orientation(a),
        );
        let pool_b = create_mock_damm_pool(
            &mut env.svm,
            &PoolSpec::new(
                Pubkey::new_from_array([22u8; 32]),
                collateral_mint,
                quote_mint,
                collateral_reserve,
                quote_reserve,
            )
            .orientation(b),
        );

        set_amm_program_data(&mut env.svm, AMM_PROGRAM_DATA, AMM_DEPLOY_SLOT);

        Self {
            env,
            cook,
            bcook,
            collateral,
            pool_a,
            pool_b,
            collateral_mint,
            collateral_reserve,
            quote_reserve,
        }
    }

    pub fn pool_refs(&self) -> [PoolRef; 2] {
        [
            PoolRef {
                pool: self.pool_a.pool,
                collateral_vault: self.pool_a.collateral_vault,
                quote_vault: self.pool_a.quote_vault,
            },
            PoolRef {
                pool: self.pool_b.pool,
                collateral_vault: self.pool_b.collateral_vault,
                quote_vault: self.pool_b.quote_vault,
            },
        ]
    }

    /// Attach the two pools with the test thresholds.
    pub fn init(&mut self) -> &mut Self {
        self.init_with(test_market_config())
    }

    pub fn init_with(&mut self, config: MarketOracleConfig) -> &mut Self {
        let refs = self.pool_refs();
        let (collateral, quote) = (self.collateral_mint, self.cook.mint);
        self.env
            .init_market_oracle(
                collateral,
                collateral,
                quote,
                COLLATERAL_DECIMALS,
                QUOTE_DECIMALS,
                refs,
                config,
            )
            .expect("init_market_oracle");
        self
    }

    /// Wait `seconds` and take one observation, insisting it is accepted.
    pub fn observe_after(&mut self, seconds: i64) {
        self.env.warp_seconds(seconds);
        self.refresh().expect("observation");
    }

    /// Enough properly spaced observations to leave the oracle bootstrapped.
    pub fn bootstrap(&mut self) {
        self.refresh().expect("first observation");
        // 6 x 60s clears both `min_observations` (3) and `min_span_seconds`
        // (300) with one interval to spare.
        for _ in 0..6 {
            self.observe_after(60);
        }
        assert!(
            self.market_oracle().is_bootstrapped(),
            "the fixture failed to bootstrap; every later assertion would be vacuous"
        );
    }

    /// The accepted reference price, 1e18-scaled.
    pub fn accepted(&self) -> u128 {
        self.env
            .read_oracle(self.collateral_mint)
            .reference
            .effective_rate
    }

    pub fn health(&self) -> u8 {
        self.env.read_oracle(self.collateral_mint).health
    }

    pub fn market_oracle(&self) -> aera::state::MarketOracle {
        self.env.read_market_oracle(self.collateral_mint)
    }

    /// Set both pools to the same reserves, i.e. an honest market at one price.
    pub fn set_both_pools(&mut self, collateral: u64, quote: u64) {
        set_pool_reserves(&mut self.env.svm, &self.pool_a, collateral, quote);
        set_pool_reserves(&mut self.env.svm, &self.pool_b, collateral, quote);
    }

    /// The price both pools currently imply, as the program would compute it.
    pub fn price_of(&self, collateral: u64, quote: u64) -> u128 {
        expected_price(collateral, quote, COLLATERAL_DECIMALS, QUOTE_DECIMALS)
    }

    pub fn refresh(&mut self) -> Result<(), String> {
        let payer = self.env.admin.insecure_clone();
        let mint = self.collateral_mint;
        let (a, b) = (self.pool_a, self.pool_b);
        self.env
            .try_refresh_market_oracle_as(&payer, mint, &a, &b, AMM_PROGRAM_DATA)
    }

    /// Refresh paid for by a wallet that is not the admin.
    pub fn refresh_as_stranger(&mut self) -> Result<(), String> {
        let stranger = self.env.create_user();
        let mint = self.collateral_mint;
        let (a, b) = (self.pool_a, self.pool_b);
        self.env
            .try_refresh_market_oracle_as(&stranger, mint, &a, &b, AMM_PROGRAM_DATA)
    }
}
