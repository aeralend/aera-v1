//! Change a share mint's metadata after the reserve exists.
//!
//! ## Why this had to be added
//!
//! `init_reserve` writes the share token's name, symbol and URI once, and sets
//! the metadata update authority to the reserve PDA. That is the right owner --
//! no wallet can rename aCOOK -- but it also means nothing outside this program
//! can ever sign an update, and until now the program had no instruction that
//! did. The values written at creation were permanent.
//!
//! They were also wrong. Both deployed mints point at
//! `https://aera.io/acook.json`, a domain Aera does not own, which answers 200
//! with a hosting provider's suspended-account page. Every wallet asking aCOOK
//! what it is receives HTML where JSON should be, and no error saying so, which
//! is why the token renders with no identity at all.
//!
//! A URI is not an economic parameter. It decides what a wallet draws next to a
//! balance, and nothing about what the protocol enforces or what anything is
//! worth. It is also the one field guaranteed to rot: it names a host, and hosts
//! move. Making it immutable was the mistake.
//!
//! ## What it cannot do
//!
//! Only the three metadata fields, and only on a reserve that already exists.
//! It does not touch `ReserveConfig`, so nothing here can move an LTV, a cap or
//! a fee, and it is not routed through the parameter timelock for the same
//! reason: there is no position whose value it can change.
//!
//! ## Growing the account
//!
//! TokenMetadata is variable length and lives inside the mint. Token-2022
//! reallocates the account when a field grows, but it will not fund the extra
//! rent, and a mint that falls below rent exemption is a mint the runtime can
//! reclaim. So the difference is transferred from the admin first, and only
//! upward -- shrinking leaves the surplus where it is rather than paying a
//! refund out of an account other people's tokens are minted from.

use anchor_lang::prelude::*;
use anchor_lang::system_program::{transfer, Transfer};
use anchor_spl::token_2022_extensions::token_metadata::{
    token_metadata_update_field, TokenMetadataUpdateField,
};
use anchor_spl::token_interface::{Mint, Token2022};
use spl_token_metadata_interface::state::{Field, TokenMetadata};

use crate::constants::*;
use crate::errors::AeraError;
use crate::instructions::admin::init_reserve::ShareMetadata;
use crate::state::reserve_signer_seeds;
use crate::state::{Global, Market, Reserve};

pub fn handle_set_share_metadata(
    context: Context<SetShareMetadata>,
    metadata: ShareMetadata,
) -> Result<()> {
    metadata.validate()?;

    let market_key = context.accounts.market.key();
    let liquidity_mint_key = context.accounts.reserve.liquidity_mint;
    let reserve_bump = [context.accounts.reserve.bump];

    /*
     * Fund the growth before asking for it.
     *
     * `update_field` reallocates and then checks rent exemption; it does not
     * move lamports. Sizing from the new values rather than from a delta because
     * the fields are written one at a time below and the account is only allowed
     * to be short between two of those calls if nobody looks -- which the last
     * one does.
     */
    let mint_info = context.accounts.share_mint.to_account_info();
    let projected = TokenMetadata {
        update_authority: Some(context.accounts.reserve.key()).try_into().unwrap(),
        mint: context.accounts.share_mint.key(),
        name: metadata.name.clone(),
        symbol: metadata.symbol.clone(),
        uri: metadata.uri.clone(),
        additional_metadata: Vec::new(),
    }
    .tlv_size_of()
    .map_err(|_| AeraError::MathOverflow)?;

    let needed = mint_info
        .data_len()
        .checked_add(projected)
        .ok_or(AeraError::MathOverflow)?;
    let required_lamports = Rent::get()?.minimum_balance(needed);
    let held = mint_info.lamports();

    if required_lamports > held {
        transfer(
            CpiContext::new(
                context.accounts.system_program.key(),
                Transfer {
                    from: context.accounts.admin.to_account_info(),
                    to: mint_info.clone(),
                },
            ),
            required_lamports.saturating_sub(held),
        )?;
    }

    let reserve_seeds = reserve_signer_seeds(&market_key, &liquidity_mint_key, &reserve_bump);

    for (field, value) in [
        (Field::Name, metadata.name.clone()),
        (Field::Symbol, metadata.symbol.clone()),
        (Field::Uri, metadata.uri.clone()),
    ] {
        token_metadata_update_field(
            CpiContext::new_with_signer(
                context.accounts.share_token_program.key(),
                TokenMetadataUpdateField {
                    program_id: context.accounts.share_token_program.to_account_info(),
                    metadata: mint_info.clone(),
                    update_authority: context.accounts.reserve.to_account_info(),
                },
                &[&reserve_seeds],
            ),
            field,
            value,
        )?;
    }

    emit!(ShareMetadataChanged {
        reserve: context.accounts.reserve.key(),
        share_mint: context.accounts.share_mint.key(),
        symbol: metadata.symbol,
        uri: metadata.uri,
    });
    Ok(())
}

#[event]
pub struct ShareMetadataChanged {
    pub reserve: Pubkey,
    pub share_mint: Pubkey,
    pub symbol: String,
    pub uri: String,
}

#[derive(Accounts)]
pub struct SetShareMetadata<'info> {
    #[account(has_one = admin @ AeraError::NotAdmin)]
    pub global: Box<Account<'info, Global>>,

    #[account(mut)]
    pub admin: Signer<'info>,

    #[account(has_one = global @ AeraError::GlobalMismatch)]
    pub market: Box<Account<'info, Market>>,

    /*
     * Seeded by the market and the liquidity mint, so the reserve cannot be one
     * from another market, and `has_one = share_mint` stops a caller pairing a
     * real reserve with somebody else's mint to make this program sign for it.
     */
    #[account(
        seeds = [RESERVE_SEED, market.key().as_ref(), reserve.liquidity_mint.as_ref()],
        bump = reserve.bump,
        has_one = market @ AeraError::MarketMismatch,
        has_one = share_mint @ AeraError::MarketMismatch,
    )]
    pub reserve: Box<Account<'info, Reserve>>,

    #[account(mut)]
    pub share_mint: Box<InterfaceAccount<'info, Mint>>,

    pub share_token_program: Program<'info, Token2022>,
    pub system_program: Program<'info, System>,
}
