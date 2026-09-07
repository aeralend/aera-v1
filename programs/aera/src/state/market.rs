//! Domain 2 of 4: **Market**.
//!
//! A market is a risk-isolated group of reserves sharing one quote currency.
//! Aera launches with exactly one — "Aera Core", quoted in COOK — but the
//! account is keyed by `market_id` so a second market never shares risk with
//! the first.

use anchor_lang::prelude::*;

#[account]
#[derive(InitSpace)]
pub struct Market {
    /// The protocol instance this market belongs to.
    pub global: Pubkey,

    /// Index this market's PDA is derived from (`["market", market_id]`). The
    /// market is identified by this id, never by an individual's address.
    pub market_id: u64,

    /// The mint every obligation value in this market is denominated in. For
    /// Aera Core this is COOK itself, so a "value" in this program is a COOK
    /// amount and the borrow reserve's own price is 1.0.
    pub quote_currency_mint: Pubkey,

    /// Human label, e.g. "Aera Core". Fixed width so the account size is known.
    #[max_len(32)]
    pub name: String,

    pub bump: u8,
}
