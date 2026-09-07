//! Parameter changes, and the timelock that separates the safe ones from the
//! rest.
//!
//! The rule from PARAMS.md: **tightening is instant, loosening waits.** Pausing
//! and cutting caps are emergency actions and must not be delayed by the same
//! mechanism that protects users from a captured admin. Raising LTV or raising
//! caps is how a captured admin would drain the pool, so those queue.
//!
//! `set_params` decides which path a change takes by comparing it to the live
//! config — the caller does not choose, so there is no flag to get wrong.

use anchor_lang::prelude::*;

use crate::errors::AeraError;
use crate::state::{Global, PendingConfig, Reserve, ReserveConfig};

pub fn handle_set_params(context: Context<SetParams>, config: ReserveConfig) -> Result<()> {
    config.validate()?;

    let timelock = context.accounts.global.param_timelock_seconds;
    let reserve = &mut context.accounts.reserve;

    if reserve.config.is_tightening_from(&config) {
        // Instant path.
        reserve.config = config;
        reserve.pending = PendingConfig::default();
        emit!(ParamsApplied {
            reserve: reserve.key(),
            timelocked: false,
        });
        return Ok(());
    }

    let now = Clock::get()?.unix_timestamp;
    reserve.pending = PendingConfig {
        config,
        eta: now.checked_add(timelock).ok_or(AeraError::MathOverflow)?,
    };
    emit!(ParamsQueued {
        reserve: reserve.key(),
        eta: reserve.pending.eta,
    });
    Ok(())
}

/// Execute a queued change once its timelock has elapsed. Permissionless: the
/// delay is the protection, and requiring the admin to return would let a
/// change sit half-applied.
pub fn handle_apply_pending_params(context: Context<ApplyPendingParams>) -> Result<()> {
    let reserve = &mut context.accounts.reserve;
    require!(reserve.pending.eta != 0, AeraError::NoPendingConfig);

    let now = Clock::get()?.unix_timestamp;
    require!(now >= reserve.pending.eta, AeraError::TimelockNotElapsed);

    // Re-validate at apply time: the hard maxima may have been the reason a
    // change was queued, and nothing should be applied that would fail today.
    reserve.pending.config.validate()?;

    reserve.config = reserve.pending.config;
    reserve.pending = PendingConfig::default();
    emit!(ParamsApplied {
        reserve: reserve.key(),
        timelocked: true,
    });
    Ok(())
}

/// Drop a queued change without applying it.
pub fn handle_cancel_pending_params(context: Context<SetParams>) -> Result<()> {
    let reserve = &mut context.accounts.reserve;
    require!(reserve.pending.eta != 0, AeraError::NoPendingConfig);
    reserve.pending = PendingConfig::default();
    Ok(())
}

/// Change the timelock itself. Bounded by `MAX_PARAM_TIMELOCK_SECONDS` so it
/// cannot be set so long that queued changes never land.
pub fn handle_set_timelock(context: Context<SetTimelock>, seconds: i64) -> Result<()> {
    Global::validate_timelock(seconds)?;
    context.accounts.global.param_timelock_seconds = seconds;
    Ok(())
}

#[event]
pub struct ParamsQueued {
    pub reserve: Pubkey,
    pub eta: i64,
}

#[event]
pub struct ParamsApplied {
    pub reserve: Pubkey,
    pub timelocked: bool,
}

#[derive(Accounts)]
pub struct SetParams<'info> {
    #[account(has_one = admin @ AeraError::NotAdmin)]
    pub global: Box<Account<'info, Global>>,

    pub admin: Signer<'info>,

    #[account(
        mut,
        constraint = reserve.market == market.key() @ AeraError::MarketMismatch,
    )]
    pub reserve: Box<Account<'info, Reserve>>,

    #[account(has_one = global @ AeraError::GlobalMismatch)]
    pub market: Account<'info, crate::state::Market>,
}

#[derive(Accounts)]
pub struct ApplyPendingParams<'info> {
    #[account(mut)]
    pub reserve: Box<Account<'info, Reserve>>,
}

#[derive(Accounts)]
pub struct SetTimelock<'info> {
    #[account(mut, has_one = admin @ AeraError::NotAdmin)]
    pub global: Box<Account<'info, Global>>,

    pub admin: Signer<'info>,
}
