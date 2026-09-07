//! Building genuine v0.1 state.
//!
//! The migration suite runs the **actual v0.1 binary** (`fixtures/aera_v0_1.so`,
//! built from the last commit before the guardian oracle was removed), creates
//! real deposits, debt and interest with it, then swaps the program for v0.2 and
//! migrates. Constructing v0.1-shaped state with v0.2 structures and calling
//! that a migration test would prove nothing at all.
//!
//! ## Why these two instructions are hand-built
//!
//! The test crate compiles against the *v0.2* `aera` library, so
//! `aera::instruction::SetOracle` is the v0.2 shape and `PublishPrice` no
//! longer exists. Both are therefore assembled here from first principles:
//! discriminator as `sha256("global:<name>")[..8]`, arguments Borsh-encoded by
//! hand, accounts listed in the order v0.1's `#[derive(Accounts)]` declared
//! them.
//!
//! Everything else needs no special handling. `init_reserve`, `supply`,
//! `borrow`, `repay` and the rest kept identical discriminators and identical
//! account *orders* across the two versions -- only the name of one field
//! changed, from `price_feed` to `oracle`, and a name is not part of the wire
//! format. So the v0.2 accounts structs generate exactly the right metas for
//! the v0.1 program, provided the v0.1 PDA is passed where the oracle goes.

use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use anchor_lang::solana_program::system_program;

use super::Pubkey;

/// `sha256("global:set_oracle")[..8]`, as v0.1 declared it.
const SET_ORACLE: [u8; 8] = [186, 128, 81, 104, 74, 79, 18, 224];

/// `sha256("global:publish_price")[..8]`. No such instruction exists in v0.2.
const PUBLISH_PRICE: [u8; 8] = [117, 13, 6, 171, 29, 204, 11, 1];

/// `sha256("account:PriceFeed")[..8]`. The migration checks for this before it
/// closes anything, and the tests check it to prove the feed really existed.
pub const PRICE_FEED_DISCRIMINATOR: [u8; 8] = [189, 103, 252, 23, 152, 35, 243, 156];

/// v0.1's guardian-set instruction.
///
/// ```ignore
/// set_oracle(guardians: Vec<Pubkey>, quorum: u8, max_age_seconds: u64,
///            circuit_breaker_bps: u16)
/// ```
#[allow(clippy::too_many_arguments)] // mirrors v0.1's own signature exactly
pub fn set_oracle_ix(
    program: Pubkey,
    global: Pubkey,
    market: Pubkey,
    mint: Pubkey,
    price_feed: Pubkey,
    admin: Pubkey,
    guardians: &[Pubkey],
    quorum: u8,
    max_age_seconds: u64,
    circuit_breaker_bps: u16,
) -> Instruction {
    let mut data = SET_ORACLE.to_vec();
    // Vec<Pubkey>: u32 length prefix, then the keys.
    data.extend_from_slice(&(guardians.len() as u32).to_le_bytes());
    for key in guardians {
        data.extend_from_slice(key.as_ref());
    }
    data.push(quorum);
    data.extend_from_slice(&max_age_seconds.to_le_bytes());
    data.extend_from_slice(&circuit_breaker_bps.to_le_bytes());

    Instruction {
        program_id: program,
        /*
         * Order read off v0.1's `#[derive(Accounts)] pub struct SetOracle`:
         * global, admin, market, price_feed, mint, system_program.
         *
         * Written from assumption the first time -- admin last, mint third --
         * and the program refused it with AccountNotSigner. Which is the point
         * of building this by hand: a list generated from the v0.2 types could
         * only ever agree with v0.2.
         */
        accounts: vec![
            AccountMeta::new_readonly(global, false),
            AccountMeta::new(admin, true),
            AccountMeta::new_readonly(market, false),
            AccountMeta::new(price_feed, false),
            AccountMeta::new_readonly(mint, false),
            AccountMeta::new_readonly(system_program::id(), false),
        ],
        data,
    }
}

/// v0.1's guardian price posting.
///
/// ```ignore
/// publish_price(price_mantissa: i128, exponent: i32)
/// ```
pub fn publish_price_ix(
    program: Pubkey,
    price_feed: Pubkey,
    guardian: Pubkey,
    price_mantissa: i128,
    exponent: i32,
) -> Instruction {
    let mut data = PUBLISH_PRICE.to_vec();
    data.extend_from_slice(&price_mantissa.to_le_bytes());
    data.extend_from_slice(&exponent.to_le_bytes());

    Instruction {
        program_id: program,
        accounts: vec![
            AccountMeta::new(price_feed, false),
            AccountMeta::new_readonly(guardian, true),
        ],
        data,
    }
}
