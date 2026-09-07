//! Unlock collateral, but only while the position stays inside its borrow limit.
//!
//! The post-withdraw allowed-borrow value is simulated and the withdraw
//! rejected if existing debt would exceed it. Every step of the removal rounds
//! UP: subtracting an over-estimate of the removed borrow power guarantees the
//! resulting allowance is never higher than a full recompute would give, so
//! independent flooring cannot let a withdraw squeak past by a rounding
//! sub-unit.

use anchor_lang::prelude::*;
use anchor_spl::token_2022::Token2022;
use anchor_spl::token_interface::{transfer_checked, Mint, TokenAccount, TransferChecked};

use crate::constants::{BPS_DENOMINATOR, OBLIGATION_SHARE_VAULT_SEED};
use crate::errors::AeraError;
use crate::math::{market_value, mul_div_ceil, Rounding};
use crate::oracle::breaker::RiskAction;
use crate::risk::require_within_borrow_limit;
use crate::state::{obligation_signer_seeds, Global, Obligation, OracleState, Reserve};

pub fn handle_withdraw_collateral(
    context: Context<WithdrawCollateral>,
    share_amount: u64,
) -> Result<()> {
    require!(share_amount > 0, AeraError::ZeroAmount);
    context.accounts.global.require_not_paused()?;

    let clock = Clock::get()?;
    context.accounts.obligation.require_refreshed()?;
    context.accounts.reserve.require_accrued()?;

    let reserve = &context.accounts.reserve;
    // Removing collateral can only worsen a health factor, so it is gated
    // exactly as borrowing is.
    let oracle = &context.accounts.oracle;
    oracle.require_fresh(clock.slot)?;
    oracle.require_permits(RiskAction::WithdrawCollateral)?;
    let price_scaled = oracle.effective_rate()?;

    let reserve_key = reserve.key();
    let haircut_bps = reserve.config.collateral_haircut_bps;
    let ltv_bps = reserve.config.loan_to_value_bps;
    let decimals = reserve.liquidity_decimals;
    let removed_liquidity = reserve.shares_to_liquidity(share_amount, Rounding::Up)?;

    let obligation = &mut context.accounts.obligation;
    let index = obligation.find_collateral(reserve_key)?;
    require!(
        obligation.deposits[index].deposited_shares >= share_amount,
        AeraError::WithdrawTooLarge
    );

    let removed_face = market_value(removed_liquidity, decimals, price_scaled, Rounding::Up)?;
    // Haircut rounds UP here (unlike refresh, which floors) so the borrow power
    // removed is never under-estimated.
    let keep = BPS_DENOMINATOR
        .checked_sub(haircut_bps as u128)
        .ok_or(AeraError::MathOverflow)?;
    let removed_effective = mul_div_ceil(removed_face, keep, BPS_DENOMINATOR)?;
    let removed_allowed = mul_div_ceil(removed_effective, ltv_bps as u128, BPS_DENOMINATOR)?;

    // saturating_sub is correct here, not balance math: the ceil-rounded removal
    // can exceed the floor-cached total by a sub-unit when withdrawing
    // everything, and zero remaining allowance is the conservative answer.
    let new_allowed = obligation
        .allowed_borrow_value
        .saturating_sub(removed_allowed);
    require_within_borrow_limit(
        obligation.borrowed_value,
        new_allowed,
        AeraError::WithdrawTooLarge,
    )?;

    obligation.deposits[index].deposited_shares = obligation.deposits[index]
        .deposited_shares
        .checked_sub(share_amount)
        .ok_or(AeraError::MathOverflow)?;
    if obligation.deposits[index].deposited_shares == 0 {
        obligation.deposits.remove(index);
    }
    obligation.stale = true;

    let market = obligation.market;
    let owner = obligation.owner;
    let bump = [obligation.bump];
    let seeds = obligation_signer_seeds(&market, &owner, &bump);
    transfer_checked(
        CpiContext::new_with_signer(
            context.accounts.share_token_program.key(),
            TransferChecked {
                from: context.accounts.obligation_share_vault.to_account_info(),
                mint: context.accounts.share_mint.to_account_info(),
                to: context.accounts.user_share.to_account_info(),
                authority: obligation.to_account_info(),
            },
            &[&seeds],
        ),
        share_amount,
        context.accounts.share_mint.decimals,
    )?;

    Ok(())
}

#[derive(Accounts)]
pub struct WithdrawCollateral<'info> {
    pub global: Box<Account<'info, Global>>,

    #[account(mut, has_one = owner)]
    pub obligation: Box<Account<'info, Obligation>>,

    pub owner: Signer<'info>,

    #[account(
        has_one = share_mint,
        has_one = oracle,
        constraint = reserve.market == obligation.market @ AeraError::MarketMismatch,
    )]
    pub reserve: Box<Account<'info, Reserve>>,

    pub oracle: Box<Account<'info, OracleState>>,

    pub share_mint: Box<InterfaceAccount<'info, Mint>>,

    #[account(
        mut,
        seeds = [OBLIGATION_SHARE_VAULT_SEED, reserve.key().as_ref(), obligation.key().as_ref()],
        bump,
        token::mint = share_mint,
        token::authority = obligation,
        token::token_program = share_token_program,
    )]
    pub obligation_share_vault: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(mut)]
    pub user_share: Box<InterfaceAccount<'info, TokenAccount>>,

    /// Shares are Token-2022; collateral only ever moves share tokens.
    pub share_token_program: Program<'info, Token2022>,
}
