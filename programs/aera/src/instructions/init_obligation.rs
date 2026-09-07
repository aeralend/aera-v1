use anchor_lang::prelude::*;

use crate::constants::OBLIGATION_SEED;
use crate::state::{Market, Obligation};

pub fn handle_init_obligation(context: Context<InitObligation>) -> Result<()> {
    let obligation = &mut context.accounts.obligation;
    obligation.market = context.accounts.market.key();
    obligation.owner = context.accounts.owner.key();
    obligation.last_update_slot = Clock::get()?.slot;
    // Stale until the first refresh; an empty obligation has nothing to value.
    obligation.stale = true;
    obligation.prices_stressed = false;
    obligation.deposited_value = 0;
    obligation.effective_collateral_value = 0;
    obligation.borrowed_value = 0;
    obligation.allowed_borrow_value = 0;
    obligation.unhealthy_borrow_value = 0;
    obligation.deposits = Vec::new();
    obligation.borrows = Vec::new();
    obligation.bump = context.bumps.obligation;
    Ok(())
}

#[derive(Accounts)]
pub struct InitObligation<'info> {
    pub market: Box<Account<'info, Market>>,

    #[account(
        init,
        payer = owner,
        space = Obligation::DISCRIMINATOR.len() + Obligation::INIT_SPACE,
        seeds = [OBLIGATION_SEED, market.key().as_ref(), owner.key().as_ref()],
        bump,
    )]
    pub obligation: Box<Account<'info, Obligation>>,

    #[account(mut)]
    pub owner: Signer<'info>,

    pub system_program: Program<'info, System>,
}
