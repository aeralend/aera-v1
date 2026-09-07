//! Create the observation history for a market-priced oracle.
//!
//! Admin-only, and separate from `init_oracle`: the `OracleState` is created
//! there as usual, with `MarketTwap` as its source kind, and this attaches the
//! pools and thresholds it needs. Splitting them keeps `OracleState` identical
//! for all three source kinds, which is what lets Core's accounts stay untouched.
//!
//! The pools are configuration of *this* oracle, never global. A second Tier 3
//! market names its own, and no generic code carries one asset's addresses.

use anchor_lang::prelude::*;

use crate::errors::AeraError;
use crate::state::{MarketOracle, MarketOracleConfig, OracleState, PoolRef};

pub fn handle_init_market_oracle(
    context: Context<InitMarketOracle>,
    collateral_mint: Pubkey,
    quote_mint: Pubkey,
    collateral_decimals: u8,
    quote_decimals: u8,
    pools: [PoolRef; 2],
    config: MarketOracleConfig,
) -> Result<()> {
    config.validate()?;

    /*
     * The oracle must already be `MarketTwap`.
     *
     * `refresh_market_oracle` checks this too, so nothing unsafe gets through
     * either way -- but without it here the mistake is silent until the first
     * crank, and what an operator sees then is `UnknownOracleSource` from an
     * instruction they did not knowingly misconfigure. The order is documented
     * above: `set_oracle` with `MarketTwap`, then attach the pools.
     */
    require!(
        context.accounts.oracle.kind()? == crate::oracle::OracleSourceKind::MarketTwap,
        AeraError::UnknownOracleSource
    );

    // Two distinct pools, or the cross-pool check is comparing a reading with
    // itself and always agrees.
    require!(
        pools[0].pool != pools[1].pool,
        AeraError::InvalidOracleConfig
    );
    require!(
        collateral_mint != quote_mint,
        AeraError::InvalidOracleConfig
    );

    let market_oracle = &mut context.accounts.market_oracle;
    market_oracle.oracle = context.accounts.oracle.key();
    market_oracle.collateral_mint = collateral_mint;
    market_oracle.quote_mint = quote_mint;
    market_oracle.collateral_decimals = collateral_decimals;
    market_oracle.quote_decimals = quote_decimals;
    market_oracle.config = config;
    market_oracle.pools = pools;
    market_oracle.observations = Vec::new();
    market_oracle.next_index = 0;
    market_oracle.pending_eta = 0;
    market_oracle.bump = context.bumps.market_oracle;

    Ok(())
}

/// Queue or apply a change to the thresholds.
///
/// Same rule as everywhere else in Aera: a tightening lands immediately, a
/// loosening waits out the global timelock.
pub fn handle_set_market_oracle_config(
    context: Context<SetMarketOracleConfig>,
    config: MarketOracleConfig,
) -> Result<()> {
    config.validate()?;
    let now = Clock::get()?.unix_timestamp;
    let timelock = context.accounts.global.param_timelock_seconds;
    let market_oracle = &mut context.accounts.market_oracle;

    if market_oracle.config.is_tightening_from(&config) {
        market_oracle.config = config;
        // A tightening supersedes a queued loosening, so an operator acting in
        // an incident does not find yesterday's relaxation still pending.
        market_oracle.pending_eta = 0;
    } else {
        market_oracle.pending_config = config;
        market_oracle.pending_eta = now.checked_add(timelock).ok_or(AeraError::MathOverflow)?;
    }
    Ok(())
}

pub fn handle_apply_pending_market_oracle_config(
    context: Context<ApplyPendingMarketOracleConfig>,
) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    let market_oracle = &mut context.accounts.market_oracle;
    require!(market_oracle.pending_eta != 0, AeraError::NoPendingConfig);
    require!(
        now >= market_oracle.pending_eta,
        AeraError::TimelockNotElapsed
    );
    market_oracle.config = market_oracle.pending_config;
    market_oracle.pending_eta = 0;
    Ok(())
}

#[derive(Accounts)]
pub struct InitMarketOracle<'info> {
    #[account(has_one = admin)]
    pub global: Box<Account<'info, crate::state::Global>>,

    #[account(mut)]
    pub admin: Signer<'info>,

    pub oracle: Box<Account<'info, OracleState>>,

    #[account(
        init,
        payer = admin,
        space = 8 + MarketOracle::INIT_SPACE,
        seeds = [MarketOracle::SEED, oracle.key().as_ref()],
        bump,
    )]
    pub market_oracle: Box<Account<'info, MarketOracle>>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct SetMarketOracleConfig<'info> {
    #[account(has_one = admin)]
    pub global: Box<Account<'info, crate::state::Global>>,
    pub admin: Signer<'info>,

    #[account(
        mut,
        seeds = [MarketOracle::SEED, market_oracle.oracle.as_ref()],
        bump = market_oracle.bump,
    )]
    pub market_oracle: Box<Account<'info, MarketOracle>>,
}

#[derive(Accounts)]
pub struct ApplyPendingMarketOracleConfig<'info> {
    pub global: Box<Account<'info, crate::state::Global>>,

    /// Permissionless, like every other `apply_pending_*`: the delay is the
    /// control, and the change was authorised when it was queued.
    #[account(
        mut,
        seeds = [MarketOracle::SEED, market_oracle.oracle.as_ref()],
        bump = market_oracle.bump,
    )]
    pub market_oracle: Box<Account<'info, MarketOracle>>,
}
