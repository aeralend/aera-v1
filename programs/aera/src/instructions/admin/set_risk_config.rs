//! Create or change a reserve's per-wallet borrow cap.
//!
//! Follows the same rule as every other risk parameter in Aera: a **tightening
//! lands immediately, a loosening waits.** Lowering what one wallet may owe can
//! never make the market less safe, and an operator responding to an incident
//! should not have to wait a day to do it. Raising it can, so it does.
//!
//! The account is created on first use. Reserves that never need a limit --
//! Core's, whose collateral is a stake-pool rate that cannot be moved by trading
//! -- never have one created, and `borrow` reads their absent config as
//! unlimited.

use anchor_lang::prelude::*;

use crate::constants::MAX_PROTOCOL_LIQUIDATION_SHARE_BPS;
use crate::errors::AeraError;
use crate::state::{Global, Reserve, RiskConfig};

/// Queue or apply a per-wallet borrow cap.
///
/// A tightening is written straight to `per_wallet_borrow_cap`. A loosening is
/// parked in `pending_*` and becomes applicable after the global timelock, at
/// which point `apply_pending_risk_config` promotes it.
pub fn handle_set_risk_config(
    context: Context<SetRiskConfig>,
    per_wallet_borrow_cap: u64,
    protocol_liquidation_share_bps: u16,
) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    let timelock = context.accounts.global.param_timelock_seconds;
    let reserve_key = context.accounts.reserve.key();

    /*
     * Aera's share of the liquidation bonus, bounded twice.
     *
     * The hard ceiling is what no admin may exceed. The second bound is the
     * one that makes the design true rather than merely bounded: a share above
     * this reserve's own bonus would be taken out of the liquidator's
     * principal, not out of the bonus, which is exactly what Gap D exists to
     * prevent. Both are checked here and the split clamps again at liquidation
     * time, because `set_params` can lower the bonus afterwards.
     */
    require!(
        protocol_liquidation_share_bps <= MAX_PROTOCOL_LIQUIDATION_SHARE_BPS,
        AeraError::InvalidConfig
    );
    require!(
        protocol_liquidation_share_bps <= context.accounts.reserve.config.liquidation_bonus_bps,
        AeraError::InvalidConfig
    );

    let config = &mut context.accounts.risk_config;

    // First write: record which reserve this belongs to. `borrow` checks it, so
    // a config can never be pointed at a different reserve after the fact.
    if config.reserve == Pubkey::default() {
        config.reserve = reserve_key;
        config.bump = context.bumps.risk_config;
    }
    require_keys_eq!(config.reserve, reserve_key, AeraError::MarketMismatch);

    /*
     * Both values move together, and a change is a tightening only if BOTH
     * halves are.
     *
     * Lowering Aera's share is a tightening: it leaves the liquidator more, so
     * it can only make liquidation more likely to happen. Raising it is a
     * loosening and waits out the timelock, like every other loosening in the
     * protocol.
     *
     * Requiring both to tighten is the conservative reading of an ambiguous
     * case. An operator lowering a cap while raising Aera's cut has made one
     * safe change and one unsafe one, and the unsafe half should not ride in
     * immediately on the safe half's authority.
     */
    let cap_tightens =
        RiskConfig::cap_is_tighter(config.per_wallet_borrow_cap, per_wallet_borrow_cap);
    let share_tightens = protocol_liquidation_share_bps <= config.protocol_liquidation_share_bps;

    if cap_tightens && share_tightens {
        config.per_wallet_borrow_cap = per_wallet_borrow_cap;
        config.protocol_liquidation_share_bps = protocol_liquidation_share_bps;
        // A tightening supersedes any queued loosening. Otherwise an operator
        // who tightened during an incident would find yesterday's raise still
        // waiting to undo it.
        config.pending_per_wallet_borrow_cap = 0;
        config.pending_protocol_liquidation_share_bps = 0;
        config.pending_eta = 0;
    } else {
        config.pending_per_wallet_borrow_cap = per_wallet_borrow_cap;
        config.pending_protocol_liquidation_share_bps = protocol_liquidation_share_bps;
        config.pending_eta = now.checked_add(timelock).ok_or(AeraError::MathOverflow)?;
    }

    Ok(())
}

/// Promote a queued loosening once its delay has elapsed.
pub fn handle_apply_pending_risk_config(context: Context<ApplyPendingRiskConfig>) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    let config = &mut context.accounts.risk_config;

    require!(config.pending_eta != 0, AeraError::NoPendingConfig);
    require!(now >= config.pending_eta, AeraError::TimelockNotElapsed);

    config.per_wallet_borrow_cap = config.pending_per_wallet_borrow_cap;
    config.protocol_liquidation_share_bps = config.pending_protocol_liquidation_share_bps;
    config.pending_per_wallet_borrow_cap = 0;
    config.pending_protocol_liquidation_share_bps = 0;
    config.pending_eta = 0;

    Ok(())
}

#[derive(Accounts)]
pub struct SetRiskConfig<'info> {
    #[account(has_one = admin)]
    pub global: Box<Account<'info, Global>>,

    #[account(mut)]
    pub admin: Signer<'info>,

    pub reserve: Box<Account<'info, Reserve>>,

    #[account(
        init_if_needed,
        payer = admin,
        space = 8 + RiskConfig::INIT_SPACE,
        seeds = [RiskConfig::SEED, reserve.key().as_ref()],
        bump,
    )]
    pub risk_config: Box<Account<'info, RiskConfig>>,

    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct ApplyPendingRiskConfig<'info> {
    pub global: Box<Account<'info, Global>>,

    pub reserve: Box<Account<'info, Reserve>>,

    /*
     * Permissionless, like `apply_pending_params`.
     *
     * The delay is the control, not the signature: the change was authorised
     * when it was queued and the wait is what makes it observable. Requiring the
     * admin to come back and sign again would only mean a queued loosening could
     * sit applied-in-principle but unapplied in fact.
     */
    #[account(
        mut,
        seeds = [RiskConfig::SEED, reserve.key().as_ref()],
        bump = risk_config.bump,
        constraint = risk_config.reserve == reserve.key() @ AeraError::MarketMismatch,
    )]
    pub risk_config: Box<Account<'info, RiskConfig>>,
}
