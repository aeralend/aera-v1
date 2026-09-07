//! Repay borrowed COOK.
//!
//! Never gated by the protocol pause or the circuit breaker. Refusing
//! repayment while prices move would manufacture liquidations the borrower
//! could have avoided, so this path stays open in every state the protocol can
//! be in.
//!
//! Anyone may repay on behalf of an obligation, so there is no owner check.

use anchor_lang::prelude::*;
use anchor_spl::token_interface::{
    transfer_checked, Mint, TokenAccount, TokenInterface, TransferChecked,
};

use crate::constants::FIXED_POINT_SCALE;
use crate::errors::AeraError;
use crate::math::mul_div_floor;
use crate::state::{Obligation, Reserve};

pub fn handle_repay(context: Context<Repay>, liquidity_amount: u64) -> Result<()> {
    require!(liquidity_amount > 0, AeraError::ZeroAmount);
    let reserve_key = context.accounts.reserve.key();
    context.accounts.reserve.require_accrued()?;

    let index = context.accounts.reserve.borrow_index;
    let decimals = context.accounts.reserve.liquidity_decimals;

    let borrow_index = context.accounts.obligation.find_borrow(reserve_key)?;
    let principal = context.accounts.obligation.borrows[borrow_index].borrowed_principal;

    let debt_now = context.accounts.obligation.debt_at(borrow_index, index)?;
    let repay = liquidity_amount.min(debt_now);
    require!(repay > 0, AeraError::ZeroAmount);

    // Principal removed rounds DOWN, so a sub-unit of principal lingers with
    // the borrower rather than being forgiven by rounding.
    let scaled_removed = mul_div_floor(repay as u128, FIXED_POINT_SCALE, index)?.min(principal);

    {
        let reserve = &mut context.accounts.reserve;
        reserve.borrowed_principal = reserve
            .borrowed_principal
            .checked_sub(scaled_removed)
            .ok_or(AeraError::MathOverflow)?;
        reserve.available_liquidity = reserve
            .available_liquidity
            .checked_add(repay)
            .ok_or(AeraError::MathOverflow)?;
    }

    {
        let obligation = &mut context.accounts.obligation;
        obligation.borrows[borrow_index].borrowed_principal = principal
            .checked_sub(scaled_removed)
            .ok_or(AeraError::MathOverflow)?;
        if obligation.borrows[borrow_index].borrowed_principal == 0 {
            obligation.borrows.remove(borrow_index);
        }
        obligation.stale = true;
    }

    transfer_checked(
        CpiContext::new(
            context.accounts.liquidity_token_program.key(),
            TransferChecked {
                from: context.accounts.user_liquidity.to_account_info(),
                mint: context.accounts.liquidity_mint.to_account_info(),
                to: context.accounts.liquidity_vault.to_account_info(),
                authority: context.accounts.repayer.to_account_info(),
            },
        ),
        repay,
        decimals,
    )?;

    Ok(())
}

#[derive(Accounts)]
pub struct Repay<'info> {
    #[account(mut)]
    pub obligation: Box<Account<'info, Obligation>>,

    #[account(
        mut,
        has_one = liquidity_mint,
        has_one = liquidity_vault,
        constraint = reserve.market == obligation.market @ AeraError::MarketMismatch,
    )]
    pub reserve: Box<Account<'info, Reserve>>,

    pub liquidity_mint: Box<InterfaceAccount<'info, Mint>>,

    #[account(mut)]
    pub liquidity_vault: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(mut)]
    pub user_liquidity: Box<InterfaceAccount<'info, TokenAccount>>,

    pub repayer: Signer<'info>,

    pub liquidity_token_program: Interface<'info, TokenInterface>,
}
