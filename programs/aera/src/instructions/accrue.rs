//! Advance a reserve's borrow index to the current slot.
//!
//! Must run — as its own instruction in the same transaction — before any
//! handler that reads the reserve's value, and before `refresh_obligation` for
//! every reserve the obligation touches. Permissionless; the `accrue` keeper
//! cranks it, but any user transaction can include it.

use anchor_lang::prelude::*;

use crate::state::Reserve;

pub fn handle_accrue(context: Context<Accrue>) -> Result<()> {
    context.accounts.reserve.accrue_interest(Clock::get()?.slot)
}

#[derive(Accounts)]
pub struct Accrue<'info> {
    #[account(mut)]
    pub reserve: Box<Account<'info, Reserve>>,
}
