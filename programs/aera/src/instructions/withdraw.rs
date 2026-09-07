//! Burn aCOOK, take COOK back.
//!
//! Redeems `shares * total_liquidity / share_supply`, floored so the protocol
//! keeps any rounding dust, and capped by the reserve's un-borrowed liquidity.

use anchor_lang::prelude::*;
use anchor_spl::token_2022::Token2022;
use anchor_spl::token_interface::{
    burn, transfer_checked, Burn, Mint, TokenAccount, TokenInterface, TransferChecked,
};

use crate::constants::SUPPLY_POSITION_SEED;
use crate::errors::AeraError;
use crate::math::Rounding;
use crate::state::{reserve_signer_seeds, Global, Reserve, SupplyPosition};

pub fn handle_withdraw(context: Context<Withdraw>, share_amount: u64) -> Result<()> {
    require!(share_amount > 0, AeraError::ZeroAmount);
    context.accounts.global.require_not_paused()?;

    let reserve = &mut context.accounts.reserve;
    reserve.require_accrued()?;

    require!(
        reserve.share_mint_supply > 0,
        AeraError::InsufficientReserveLiquidity
    );
    let liquidity_amount = reserve.shares_to_liquidity(share_amount, Rounding::Down)?;
    require!(
        liquidity_amount <= reserve.available_liquidity,
        AeraError::InsufficientReserveLiquidity
    );

    reserve.available_liquidity = reserve
        .available_liquidity
        .checked_sub(liquidity_amount)
        .ok_or(AeraError::MathOverflow)?;
    reserve.share_mint_supply = reserve
        .share_mint_supply
        .checked_sub(share_amount)
        .ok_or(AeraError::MathOverflow)?;

    // Free the wallet's cap headroom. Saturating because interest means a
    // supplier can redeem more than they put in, and the cap tracks principal
    // supplied, not value owned — going "below zero" just means no headroom is
    // consumed any more.
    let position = &mut context.accounts.supply_position;
    position.supplied_liquidity = position.supplied_liquidity.saturating_sub(liquidity_amount);

    burn(
        CpiContext::new(
            context.accounts.share_token_program.key(),
            Burn {
                mint: context.accounts.share_mint.to_account_info(),
                from: context.accounts.user_share.to_account_info(),
                authority: context.accounts.owner.to_account_info(),
            },
        ),
        share_amount,
    )?;

    let bump = [reserve.bump];
    let seeds = reserve_signer_seeds(&reserve.market, &reserve.liquidity_mint, &bump);
    transfer_checked(
        CpiContext::new_with_signer(
            context.accounts.liquidity_token_program.key(),
            TransferChecked {
                from: context.accounts.liquidity_vault.to_account_info(),
                mint: context.accounts.liquidity_mint.to_account_info(),
                to: context.accounts.user_liquidity.to_account_info(),
                authority: reserve.to_account_info(),
            },
            &[&seeds],
        ),
        liquidity_amount,
        reserve.liquidity_decimals,
    )?;

    Ok(())
}

#[derive(Accounts)]
pub struct Withdraw<'info> {
    pub global: Box<Account<'info, Global>>,

    #[account(
        mut,
        has_one = liquidity_mint,
        has_one = liquidity_vault,
        has_one = share_mint,
    )]
    pub reserve: Box<Account<'info, Reserve>>,

    pub liquidity_mint: Box<InterfaceAccount<'info, Mint>>,

    #[account(mut)]
    pub liquidity_vault: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(mut)]
    pub share_mint: Box<InterfaceAccount<'info, Mint>>,

    #[account(mut)]
    pub user_liquidity: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(mut)]
    pub user_share: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(
        init_if_needed,
        payer = owner,
        space = SupplyPosition::DISCRIMINATOR.len() + SupplyPosition::INIT_SPACE,
        seeds = [SUPPLY_POSITION_SEED, reserve.key().as_ref(), owner.key().as_ref()],
        bump,
    )]
    pub supply_position: Box<Account<'info, SupplyPosition>>,

    #[account(mut)]
    pub owner: Signer<'info>,

    pub liquidity_token_program: Interface<'info, TokenInterface>,

    /// Always Token-2022: the share mint carries metadata extensions.
    pub share_token_program: Program<'info, Token2022>,

    pub system_program: Program<'info, System>,
}
