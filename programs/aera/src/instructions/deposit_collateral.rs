//! Lock bCOOK share tokens as collateral.
//!
//! The shares move into a per-(reserve, obligation) vault owned by the
//! obligation PDA. No health check is needed — adding collateral only improves
//! health — but the obligation is marked stale so its cached values are
//! recomputed before the next health-dependent action.
//!
//! `check_collateral_enabled` is what refuses aCOOK here: the COOK reserve is
//! configured `collateral_enabled = false`.

use anchor_lang::prelude::*;
use anchor_spl::token_2022::Token2022;
use anchor_spl::token_interface::{transfer_checked, Mint, TokenAccount, TransferChecked};

use crate::constants::OBLIGATION_SHARE_VAULT_SEED;
use crate::errors::AeraError;
use crate::risk::{check_collateral_enabled, check_isolation};
use crate::state::{Global, Obligation, Reserve};

pub fn handle_deposit_collateral(
    context: Context<DepositCollateral>,
    share_amount: u64,
) -> Result<()> {
    require!(share_amount > 0, AeraError::ZeroAmount);
    context.accounts.global.require_not_paused()?;

    let reserve_key = context.accounts.reserve.key();
    check_collateral_enabled(&context.accounts.reserve)?;
    check_isolation(
        &context.accounts.obligation,
        &context.accounts.reserve,
        reserve_key,
    )?;

    let obligation = &mut context.accounts.obligation;
    let index = obligation.upsert_collateral(reserve_key)?;
    obligation.deposits[index].deposited_shares = obligation.deposits[index]
        .deposited_shares
        .checked_add(share_amount)
        .ok_or(AeraError::MathOverflow)?;
    obligation.stale = true;

    transfer_checked(
        CpiContext::new(
            context.accounts.share_token_program.key(),
            TransferChecked {
                from: context.accounts.user_share.to_account_info(),
                mint: context.accounts.share_mint.to_account_info(),
                to: context.accounts.obligation_share_vault.to_account_info(),
                authority: context.accounts.owner.to_account_info(),
            },
        ),
        share_amount,
        context.accounts.share_mint.decimals,
    )?;

    Ok(())
}

#[derive(Accounts)]
pub struct DepositCollateral<'info> {
    pub global: Box<Account<'info, Global>>,

    #[account(mut, has_one = owner)]
    pub obligation: Box<Account<'info, Obligation>>,

    #[account(mut)]
    pub owner: Signer<'info>,

    #[account(
        has_one = share_mint,
        constraint = reserve.market == obligation.market @ AeraError::MarketMismatch,
    )]
    pub reserve: Box<Account<'info, Reserve>>,

    pub share_mint: Box<InterfaceAccount<'info, Mint>>,

    #[account(
        init_if_needed,
        payer = owner,
        token::mint = share_mint,
        token::authority = obligation,
        token::token_program = share_token_program,
        seeds = [OBLIGATION_SHARE_VAULT_SEED, reserve.key().as_ref(), obligation.key().as_ref()],
        bump,
    )]
    pub obligation_share_vault: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(mut)]
    pub user_share: Box<InterfaceAccount<'info, TokenAccount>>,

    /// Shares are Token-2022; collateral only ever moves share tokens.
    pub share_token_program: Program<'info, Token2022>,

    pub system_program: Program<'info, System>,
}
