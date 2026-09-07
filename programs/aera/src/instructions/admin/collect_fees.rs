//! Pay out accrued interest to the protocol's fee destination.
//!
//! Permissionless: the destination is fixed in `Global`, so anyone may crank
//! this and the money can only go where the admin already set. The token
//! account is checked against that owner, not merely passed in.

use anchor_lang::prelude::*;
use anchor_spl::token_interface::{
    transfer_checked, Mint, TokenAccount, TokenInterface, TransferChecked,
};

use crate::errors::AeraError;
use crate::state::{reserve_signer_seeds, Global, Reserve};

pub fn handle_collect_fees(context: Context<CollectFees>) -> Result<()> {
    context.accounts.reserve.require_accrued()?;

    // Fees are a claim on liquidity; only what is currently un-borrowed can be
    // paid now. Any remainder stays owed until borrowers repay.
    let amount = {
        let reserve = &context.accounts.reserve;
        let payable = reserve.accrued_fees.min(reserve.available_liquidity);
        require!(payable > 0, AeraError::NothingToCollect);
        payable
    };

    {
        let reserve = &mut context.accounts.reserve;
        reserve.accrued_fees = reserve
            .accrued_fees
            .checked_sub(amount)
            .ok_or(AeraError::MathOverflow)?;
        reserve.available_liquidity = reserve
            .available_liquidity
            .checked_sub(amount)
            .ok_or(AeraError::MathOverflow)?;
    }

    let reserve = &context.accounts.reserve;
    let bump = [reserve.bump];
    let seeds = reserve_signer_seeds(&reserve.market, &reserve.liquidity_mint, &bump);

    transfer_checked(
        CpiContext::new_with_signer(
            context.accounts.liquidity_token_program.key(),
            TransferChecked {
                from: context.accounts.liquidity_vault.to_account_info(),
                mint: context.accounts.liquidity_mint.to_account_info(),
                to: context.accounts.fee_token.to_account_info(),
                authority: reserve.to_account_info(),
            },
            &[&seeds],
        ),
        amount,
        reserve.liquidity_decimals,
    )?;

    Ok(())
}

#[derive(Accounts)]
pub struct CollectFees<'info> {
    pub global: Box<Account<'info, Global>>,

    #[account(
        mut,
        has_one = liquidity_mint,
        has_one = liquidity_vault,
    )]
    pub reserve: Box<Account<'info, Reserve>>,

    pub liquidity_mint: Box<InterfaceAccount<'info, Mint>>,

    #[account(mut)]
    pub liquidity_vault: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(
        mut,
        constraint = fee_token.owner == global.fee_destination @ AeraError::WrongFeeDestination,
        constraint = fee_token.mint == reserve.liquidity_mint @ AeraError::WrongFeeDestination,
    )]
    pub fee_token: Box<InterfaceAccount<'info, TokenAccount>>,

    pub liquidity_token_program: Interface<'info, TokenInterface>,
}
