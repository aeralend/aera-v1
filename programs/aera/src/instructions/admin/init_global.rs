use anchor_lang::prelude::*;

use crate::constants::GLOBAL_SEED;
use crate::state::Global;

/// Create the protocol instance. Runs once per deployment.
pub fn handle_init_global(context: Context<InitGlobal>, fee_destination: Pubkey) -> Result<()> {
    let global = &mut context.accounts.global;
    global.admin = context.accounts.admin.key();
    global.fee_destination = fee_destination;
    global.paused = false;
    global.borrow_paused = false;
    global.param_timelock_seconds = Global::default_timelock();
    global.bump = context.bumps.global;
    Ok(())
}

#[derive(Accounts)]
pub struct InitGlobal<'info> {
    #[account(
        init,
        payer = admin,
        space = Global::DISCRIMINATOR.len() + Global::INIT_SPACE,
        seeds = [GLOBAL_SEED],
        bump,
    )]
    pub global: Box<Account<'info, Global>>,

    #[account(mut)]
    pub admin: Signer<'info>,

    pub system_program: Program<'info, System>,
}
