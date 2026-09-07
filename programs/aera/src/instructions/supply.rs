//! Supply COOK, receive aCOOK.
//!
//! The first deposit mints shares 1:1; later deposits mint
//! `amount * share_supply / total_liquidity`, floored so the protocol keeps any
//! rounding dust. Three caps apply: the reserve supply cap, this wallet's cap,
//! and (implicitly) nothing else — a supply never touches health.

use anchor_lang::prelude::*;
use anchor_spl::token_2022::Token2022;
use anchor_spl::token_interface::{
    mint_to, transfer_checked, Mint, MintTo, TokenAccount, TokenInterface, TransferChecked,
};

use crate::constants::SUPPLY_POSITION_SEED;
use crate::errors::AeraError;
use crate::math::Rounding;
use crate::risk::{check_per_wallet_cap, check_supply_cap};
use crate::state::{reserve_signer_seeds, Global, Reserve, SupplyPosition};

pub fn handle_supply(context: Context<Supply>, liquidity_amount: u64) -> Result<()> {
    require!(liquidity_amount > 0, AeraError::ZeroAmount);
    context.accounts.global.require_not_paused()?;

    let reserve = &mut context.accounts.reserve;
    reserve.require_accrued()?;

    let position = &mut context.accounts.supply_position;
    check_supply_cap(reserve, liquidity_amount)?;
    check_per_wallet_cap(reserve, position.supplied_liquidity, liquidity_amount)?;

    let share_amount = reserve.liquidity_to_shares(liquidity_amount, Rounding::Down)?;
    require!(share_amount > 0, AeraError::DepositTooSmall);

    // Effects before interactions.
    reserve.available_liquidity = reserve
        .available_liquidity
        .checked_add(liquidity_amount)
        .ok_or(AeraError::MathOverflow)?;
    reserve.share_mint_supply = reserve
        .share_mint_supply
        .checked_add(share_amount)
        .ok_or(AeraError::MathOverflow)?;

    position.reserve = reserve.key();
    position.owner = context.accounts.owner.key();
    position.supplied_liquidity = position
        .supplied_liquidity
        .checked_add(liquidity_amount)
        .ok_or(AeraError::MathOverflow)?;
    position.bump = context.bumps.supply_position;

    // Liquidity moves on whatever program owns the liquidity mint.
    transfer_checked(
        CpiContext::new(
            context.accounts.liquidity_token_program.key(),
            TransferChecked {
                from: context.accounts.user_liquidity.to_account_info(),
                mint: context.accounts.liquidity_mint.to_account_info(),
                to: context.accounts.liquidity_vault.to_account_info(),
                authority: context.accounts.owner.to_account_info(),
            },
        ),
        liquidity_amount,
        reserve.liquidity_decimals,
    )?;

    let bump = [reserve.bump];
    let seeds = reserve_signer_seeds(&reserve.market, &reserve.liquidity_mint, &bump);
    // Shares are minted on Token-2022, which is the only program that can own
    // a mint carrying metadata extensions.
    mint_to(
        CpiContext::new_with_signer(
            context.accounts.share_token_program.key(),
            MintTo {
                mint: context.accounts.share_mint.to_account_info(),
                to: context.accounts.user_share.to_account_info(),
                authority: reserve.to_account_info(),
            },
            &[&seeds],
        ),
        share_amount,
    )?;

    Ok(())
}

#[derive(Accounts)]
pub struct Supply<'info> {
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

    /// Created on first supply. Exists only to make the per-wallet cap a
    /// program rule.
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
