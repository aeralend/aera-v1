//! Pause switches. All instant, none timelocked — a delay on an emergency stop
//! is not a safety feature.

use anchor_lang::prelude::*;

use crate::errors::AeraError;
use crate::state::Global;

/// Stop new borrows. Supply, withdraw, repay and liquidate keep working.
pub fn handle_pause_borrow(context: Context<PauseControl>) -> Result<()> {
    context.accounts.global.borrow_paused = true;
    Ok(())
}

/// Stop everything except repay. Repay is never blocked: refusing repayment
/// while prices move would manufacture liquidations the borrower could
/// otherwise have avoided.
pub fn handle_pause_all(context: Context<PauseControl>) -> Result<()> {
    let global = &mut context.accounts.global;
    global.paused = true;
    global.borrow_paused = true;
    Ok(())
}

/// Lift both pauses.
pub fn handle_unpause(context: Context<PauseControl>) -> Result<()> {
    let global = &mut context.accounts.global;
    global.paused = false;
    global.borrow_paused = false;
    Ok(())
}

/// Move the admin key.
pub fn handle_set_admin(context: Context<PauseControl>, new_admin: Pubkey) -> Result<()> {
    require_keys_neq!(new_admin, Pubkey::default(), AeraError::InvalidConfig);
    context.accounts.global.admin = new_admin;
    Ok(())
}

/// Repoint where the protocol's cut of interest is paid.
pub fn handle_set_fee_destination(
    context: Context<PauseControl>,
    fee_destination: Pubkey,
) -> Result<()> {
    require_keys_neq!(fee_destination, Pubkey::default(), AeraError::InvalidConfig);
    context.accounts.global.fee_destination = fee_destination;
    Ok(())
}

#[derive(Accounts)]
pub struct PauseControl<'info> {
    #[account(mut, has_one = admin @ AeraError::NotAdmin)]
    pub global: Box<Account<'info, Global>>,

    pub admin: Signer<'info>,
}
