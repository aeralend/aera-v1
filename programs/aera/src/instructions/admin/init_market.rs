use anchor_lang::prelude::*;
use anchor_spl::token_interface::Mint;

use crate::constants::MARKET_SEED;
use crate::state::{Global, Market};

/// Create a risk-isolated market. Aera launches with market_id 0, "Aera Core".
pub fn handle_init_market(
    context: Context<InitMarket>,
    market_id: u64,
    name: String,
) -> Result<()> {
    require!(name.len() <= 32, crate::errors::AeraError::InvalidConfig);

    let market = &mut context.accounts.market;
    market.global = context.accounts.global.key();
    market.market_id = market_id;
    market.quote_currency_mint = context.accounts.quote_currency_mint.key();
    market.name = name;
    market.bump = context.bumps.market;
    Ok(())
}

#[derive(Accounts)]
#[instruction(market_id: u64)]
pub struct InitMarket<'info> {
    #[account(has_one = admin @ crate::errors::AeraError::NotAdmin)]
    pub global: Box<Account<'info, Global>>,

    #[account(mut)]
    pub admin: Signer<'info>,

    // Seeded by `market_id` alone — a market is not identified by any
    // individual's address.
    #[account(
        init,
        payer = admin,
        space = Market::DISCRIMINATOR.len() + Market::INIT_SPACE,
        seeds = [MARKET_SEED, &market_id.to_le_bytes()],
        bump,
    )]
    pub market: Box<Account<'info, Market>>,

    pub quote_currency_mint: Box<InterfaceAccount<'info, Mint>>,

    pub system_program: Program<'info, System>,
}
