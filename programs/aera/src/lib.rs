//! # Aera
//!
//! An overcollateralized COOK money market on Cookie Chain.
//!
//! Suppliers deposit COOK and receive aCOOK. Borrowers lock bCOOK and borrow
//! COOK. Collateral must exceed the loan. Interest is paid by borrowers and
//! mostly goes to aCOOK holders; the protocol keeps 15%, all of which accrues
//! to a single fee destination.
//!
//! This is not a second staking farm. If you only want yield on COOK, stake it
//! at BakeYourStake — Aera is for idle COOK, or for borrowing against bCOOK you
//! would rather not sell.
//!
//! ## Account domains
//!
//! One program, four account domains (see docs/DECISIONS.md for why this is one
//! program rather than four):
//!
//! | Domain     | Account                        | Scope                          |
//! |------------|--------------------------------|--------------------------------|
//! | Global     | [`state::Global`]              | one per deployment             |
//! | Market     | [`state::Market`]              | one per risk-isolated group    |
//! | Reserve    | [`state::Reserve`]             | one per asset per market       |
//! | Obligation | [`state::Obligation`]          | one per borrower per market    |
//!
//! Supporting accounts: [`state::OracleState`] (one per mint per market) and
//! [`state::SupplyPosition`] (per-wallet supply cap accounting).
//!
//! ## Transaction shape
//!
//! Every health-dependent instruction expects, in the same transaction:
//!   1. `refresh_oracle` for each oracle involved,
//!   2. `accrue` for each reserve involved,
//!   3. `refresh_obligation` with `[reserve, oracle]` pairs in
//!      `remaining_accounts` — deposits first, then borrows,
//!   4. the action itself.
//!
//! Handlers reject a reserve not accrued this slot, an oracle not refreshed
//! this slot, and an obligation not refreshed this slot, so a stale-value
//! attack has no path.
//!
//! ## v0.2
//!
//! There is no guardian oracle. Nobody publishes a price. bCOOK is valued from
//! BakeYourStake's own stake-pool accounting, net of its redemption fee, and
//! COOK is one COOK by definition. See `docs/ORACLE_V0_2.md`.
//!
//! ## Provenance
//!
//! The reserve/obligation/vault/share-token model, the rounding discipline and
//! the interest-index approach are adapted from the QuickNode
//! `finance/lending` Anchor example (MIT). See docs/DECISIONS.md.

use anchor_lang::prelude::*;

pub mod constants;
pub mod errors;
pub mod instructions;
pub mod launch;
pub mod math;
pub mod oracle;
pub mod risk;
pub mod state;

use instructions::*;
use state::ReserveConfig;

declare_id!("AerafpFHsY4N16i4KufPJQyURryxKtYgCr6oZMwbp76q");

#[program]
pub mod aera {
    use super::*;

    // ----- Global -----

    pub fn init_global(context: Context<InitGlobal>, fee_destination: Pubkey) -> Result<()> {
        instructions::handle_init_global(context, fee_destination)
    }

    pub fn set_admin(context: Context<PauseControl>, new_admin: Pubkey) -> Result<()> {
        instructions::handle_set_admin(context, new_admin)
    }

    pub fn set_fee_destination(
        context: Context<PauseControl>,
        fee_destination: Pubkey,
    ) -> Result<()> {
        instructions::handle_set_fee_destination(context, fee_destination)
    }

    pub fn pause_borrow(context: Context<PauseControl>) -> Result<()> {
        instructions::handle_pause_borrow(context)
    }

    pub fn pause_all(context: Context<PauseControl>) -> Result<()> {
        instructions::handle_pause_all(context)
    }

    pub fn unpause(context: Context<PauseControl>) -> Result<()> {
        instructions::handle_unpause(context)
    }

    pub fn set_timelock(context: Context<SetTimelock>, seconds: i64) -> Result<()> {
        instructions::handle_set_timelock(context, seconds)
    }

    // ----- Market & Reserve -----

    pub fn init_market(context: Context<InitMarket>, market_id: u64, name: String) -> Result<()> {
        instructions::handle_init_market(context, market_id, name)
    }

    pub fn init_reserve(
        context: Context<InitReserve>,
        config: ReserveConfig,
        metadata: ShareMetadata,
    ) -> Result<()> {
        instructions::handle_init_reserve(context, config, metadata)
    }

    /// Tightening changes apply immediately; loosening changes queue behind the
    /// timelock. `set_caps` is not a separate instruction — caps are part of
    /// `ReserveConfig`, and cutting them is a tightening, so it lands instantly
    /// through this same path.
    pub fn set_params(context: Context<SetParams>, config: ReserveConfig) -> Result<()> {
        instructions::handle_set_params(context, config)
    }

    pub fn apply_pending_params(context: Context<ApplyPendingParams>) -> Result<()> {
        instructions::handle_apply_pending_params(context)
    }

    /// Create or change a reserve's per-wallet borrow cap.
    ///
    /// Lives in its own account rather than on `ReserveConfig`: growing
    /// `Reserve` would leave older reserves undeserialisable to this program,
    /// which breaks `accrue`, which breaks repayment for a whole migration.
    /// `migrate.rs:145` records that being tried and caught.
    /// Attach pools and thresholds to a `MarketTwap` oracle.
    pub fn init_market_oracle(
        context: Context<InitMarketOracle>,
        collateral_mint: Pubkey,
        quote_mint: Pubkey,
        collateral_decimals: u8,
        quote_decimals: u8,
        pools: [crate::state::PoolRef; 2],
        config: crate::state::MarketOracleConfig,
    ) -> Result<()> {
        instructions::handle_init_market_oracle(
            context,
            collateral_mint,
            quote_mint,
            collateral_decimals,
            quote_decimals,
            pools,
            config,
        )
    }

    pub fn set_market_oracle_config(
        context: Context<SetMarketOracleConfig>,
        config: crate::state::MarketOracleConfig,
    ) -> Result<()> {
        instructions::handle_set_market_oracle_config(context, config)
    }

    pub fn apply_pending_market_oracle_config(
        context: Context<ApplyPendingMarketOracleConfig>,
    ) -> Result<()> {
        instructions::handle_apply_pending_market_oracle_config(context)
    }

    /// Derive a market price from two AMM pools and append it to the history.
    ///
    /// **Permissionless.** The caller supplies no price -- they hand over pool
    /// accounts and the program reads the reserves itself. There is nothing to
    /// lie about, so there is no privilege to hold, and Aera does not become a
    /// liveness monopoly for the markets that use it.
    pub fn refresh_market_oracle(context: Context<RefreshMarketOracle>) -> Result<()> {
        instructions::handle_refresh_market_oracle(context)
    }

    /// Set the reserve's per-wallet borrow cap and Aera's share of its
    /// liquidation bonus.
    ///
    /// Both in one instruction because both live in one account and both follow
    /// the same tightening/loosening rule. `protocol_liquidation_share_bps` is
    /// carved out of the reserve's existing `liquidation_bonus_bps` and never
    /// added to it, so raising it cannot increase a borrower's penalty.
    pub fn set_risk_config(
        context: Context<SetRiskConfig>,
        per_wallet_borrow_cap: u64,
        protocol_liquidation_share_bps: u16,
    ) -> Result<()> {
        instructions::handle_set_risk_config(
            context,
            per_wallet_borrow_cap,
            protocol_liquidation_share_bps,
        )
    }

    pub fn apply_pending_risk_config(context: Context<ApplyPendingRiskConfig>) -> Result<()> {
        instructions::handle_apply_pending_risk_config(context)
    }

    pub fn cancel_pending_params(context: Context<SetParams>) -> Result<()> {
        instructions::handle_cancel_pending_params(context)
    }

    pub fn collect_fees(context: Context<CollectFees>) -> Result<()> {
        instructions::handle_collect_fees(context)
    }

    // ----- Oracle -----

    /// Configure where an asset's rate is derived from, and the bounds it must
    /// stay inside. Never the rate itself -- there is no instruction in this
    /// program that accepts one.
    pub fn set_oracle(context: Context<SetOracle>, config: OracleConfig) -> Result<()> {
        instructions::handle_set_oracle(context, config)
    }

    /// Re-derive the rate from its source and judge it against the breaker.
    /// Permissionless: the caller supplies nothing and cannot influence the
    /// result.
    pub fn refresh_oracle(context: Context<RefreshOracle>) -> Result<()> {
        instructions::handle_refresh_oracle(context)
    }

    /// Clear a breaker freeze by re-anchoring to whatever the source currently
    /// reports. The admin chooses when, never what.
    pub fn reset_oracle_breaker(context: Context<ResetOracleBreaker>) -> Result<()> {
        instructions::handle_reset_oracle_breaker(context)
    }

    // ----- Migration: v0.1 -> v0.2 -----

    /// Move one reserve off the guardian feed and onto the derived oracle.
    ///
    /// Send one per reserve in a single transaction. Atomic per reserve, and
    /// it asserts on chain that no economic field moved.
    pub fn migrate_reserve_to_v2(
        context: Context<MigrateReserveToV2>,
        config: OracleConfig,
    ) -> Result<()> {
        instructions::handle_migrate_reserve_to_v2(context, config)
    }

    /// Stamp the protocol version. Last, after every reserve has migrated.
    pub fn migrate_global_to_v2(context: Context<MigrateGlobalToV2>) -> Result<()> {
        instructions::handle_migrate_global_to_v2(context)
    }

    // ----- Cranks -----

    pub fn accrue(context: Context<Accrue>) -> Result<()> {
        instructions::handle_accrue(context)
    }

    pub fn refresh_obligation(context: Context<RefreshObligation>) -> Result<()> {
        instructions::handle_refresh_obligation(context)
    }

    // ----- Suppliers -----

    pub fn supply(context: Context<Supply>, liquidity_amount: u64) -> Result<()> {
        instructions::handle_supply(context, liquidity_amount)
    }

    pub fn withdraw(context: Context<Withdraw>, share_amount: u64) -> Result<()> {
        instructions::handle_withdraw(context, share_amount)
    }

    // ----- Borrowers -----

    pub fn init_obligation(context: Context<InitObligation>) -> Result<()> {
        instructions::handle_init_obligation(context)
    }

    pub fn deposit_collateral(
        context: Context<DepositCollateral>,
        share_amount: u64,
    ) -> Result<()> {
        instructions::handle_deposit_collateral(context, share_amount)
    }

    pub fn withdraw_collateral(
        context: Context<WithdrawCollateral>,
        share_amount: u64,
    ) -> Result<()> {
        instructions::handle_withdraw_collateral(context, share_amount)
    }

    pub fn borrow(context: Context<Borrow>, liquidity_amount: u64) -> Result<()> {
        instructions::handle_borrow(context, liquidity_amount)
    }

    pub fn repay(context: Context<Repay>, liquidity_amount: u64) -> Result<()> {
        instructions::handle_repay(context, liquidity_amount)
    }

    /// Recognise debt that no collateral stands behind.
    ///
    /// Permissionless, because the precondition is verifiable on chain and the
    /// effect only ever reduces what the protocol claims to hold. See
    /// `instructions::absorb_bad_debt` for who absorbs the loss.
    pub fn absorb_bad_debt(context: Context<AbsorbBadDebt>) -> Result<()> {
        instructions::absorb_bad_debt::handle_absorb_bad_debt(context)
    }

    pub fn liquidate(context: Context<Liquidate>, liquidity_amount: u64) -> Result<()> {
        instructions::handle_liquidate(context, liquidity_amount)
    }
}
