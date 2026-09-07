//! A second client, sharing no code with the first.
//!
//! Every other test in this repo reaches the program through `aera::accounts::*`
//! and `aera::instruction::*` — the same generated types the SDK uses. A wrong
//! account *order* would therefore be wrong identically on both sides and every
//! steal test would still pass.
//!
//! So these two build the instruction by hand:
//!
//!   * seeds written out as byte strings, not taken from `crate::constants`
//!   * PDAs derived with `find_program_address` here
//!   * `AccountMeta` listed in the order read off the program's `#[derive(Accounts)]`
//!     struct, with signer and writable flags decided here
//!   * the discriminator computed as `sha256("global:<name>")[..8]` rather than
//!     imported
//!
//! If a hand-built instruction and the generated one both work, the misconception
//! surface is split. If only the generated one works, the layout is not what the
//! source says it is.
//!
//! This is the minimum version of "a second client". A real one is a firm's job.

mod common;

use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use common::audit::*;
use common::*;
use sha2::{Digest, Sha256};
use solana_keypair::Keypair;

/// Anchor's discriminator: the first eight bytes of `sha256("global:<name>")`.
fn discriminator(name: &str) -> [u8; 8] {
    let digest = Sha256::digest(format!("global:{name}").as_bytes());
    let mut out = [0u8; 8];
    out.copy_from_slice(&digest[..8]);
    out
}

/// PDA derivation, from the seed literals rather than the crate's constants.
fn pda(seeds: &[&[u8]]) -> Pubkey {
    Pubkey::find_program_address(seeds, &aera::id()).0
}

fn actor(env: &mut Env, mint: Pubkey, amount: u64) -> Keypair {
    let user = env.create_user();
    env.fund(&user, mint, amount);
    user
}

// ---------------------------------------------------------------------------
// raw withdraw
// ---------------------------------------------------------------------------

/// Withdraw, built entirely by hand, must behave exactly like the generated one.
#[test]
fn raw_withdraw_matches_the_generated_instruction() {
    let (mut env, cook, _bcook) = Env::core(1_000);

    let user = actor(&mut env, cook.mint, tokens(10_000));
    env.supply(&user, &cook, tokens(10_000));

    // Derive every address independently of the harness helpers.
    let market = pda(&[b"market", &0u64.to_le_bytes()]);
    let global = pda(&[b"global"]);
    let reserve = pda(&[b"reserve", market.as_ref(), cook.mint.as_ref()]);
    let liquidity_vault = pda(&[b"liquidity_vault", reserve.as_ref()]);
    let share_mint = pda(&[b"share_mint", reserve.as_ref()]);
    let supply_position = pda(&[b"supply_position", reserve.as_ref(), user.pubkey().as_ref()]);

    // If these disagree, the SDK and the program have drifted apart.
    assert_eq!(global, env.global, "global PDA derived differently");
    assert_eq!(market, env.market, "market PDA derived differently");
    assert_eq!(reserve, cook.reserve, "reserve PDA derived differently");
    assert_eq!(
        liquidity_vault, cook.liquidity_vault,
        "vault PDA derived differently"
    );
    assert_eq!(
        share_mint, cook.share_mint,
        "share mint PDA derived differently"
    );

    let shares = tokens(1_000);
    let mut data = discriminator("withdraw").to_vec();
    data.extend_from_slice(&shares.to_le_bytes());

    // Order read off `#[derive(Accounts)] pub struct Withdraw`, by hand.
    let accounts = vec![
        AccountMeta::new_readonly(global, false),
        AccountMeta::new(reserve, false),
        AccountMeta::new_readonly(cook.mint, false),
        AccountMeta::new(liquidity_vault, false),
        AccountMeta::new(share_mint, false),
        AccountMeta::new(ata(&user.pubkey(), &cook.mint), false),
        AccountMeta::new(share_ata(&user.pubkey(), &share_mint), false),
        AccountMeta::new(supply_position, false),
        AccountMeta::new(user.pubkey(), true),
        AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
        AccountMeta::new_readonly(TOKEN_2022_PROGRAM_ID, false),
        AccountMeta::new_readonly(anchor_lang::system_program::ID, false),
    ];

    let before = env.balance(&ata(&user.pubkey(), &cook.mint));

    env.send_raw(
        vec![
            env.accrue_ix(&cook),
            Instruction { program_id: aera::id(), accounts, data },
        ],
        &[&user],
    )
    .expect("a hand-built withdraw must work — if it does not, the account layout is not what the source says");

    let after = env.balance(&ata(&user.pubkey(), &cook.mint));
    assert!(after > before, "the hand-built withdraw paid nothing out");
    assert_solvent(&env, &cook, "raw withdraw");
}

// ---------------------------------------------------------------------------
// raw borrow
// ---------------------------------------------------------------------------

/// Borrow, built by hand, including the `remaining_accounts` refresh pairs.
#[test]
fn raw_borrow_matches_the_generated_instruction() {
    let (mut env, cook, bcook) = Env::core(1_000);

    let supplier = actor(&mut env, cook.mint, tokens(100_000));
    env.supply(&supplier, &cook, tokens(100_000));

    let borrower = actor(&mut env, bcook.mint, tokens(10_000));
    env.fund(&borrower, cook.mint, 0);
    let obligation = env.open_position(&borrower, &bcook, tokens(10_000));

    let market = pda(&[b"market", &0u64.to_le_bytes()]);
    let derived_obligation = pda(&[b"obligation", market.as_ref(), borrower.pubkey().as_ref()]);
    assert_eq!(
        derived_obligation, obligation,
        "obligation PDA derived differently by hand"
    );

    let reserve = pda(&[b"reserve", market.as_ref(), cook.mint.as_ref()]);
    let liquidity_vault = pda(&[b"liquidity_vault", reserve.as_ref()]);
    let oracle = pda(&[b"oracle", market.as_ref(), cook.mint.as_ref()]);
    assert_eq!(oracle, cook.oracle, "oracle PDA derived differently");

    let amount = tokens(1_000);
    let mut data = discriminator("borrow").to_vec();
    data.extend_from_slice(&amount.to_le_bytes());

    /*
     * Order read off `#[derive(Accounts)] pub struct Borrow`.
     *
     * Written from assumption the first time - owner last, no price feed - and
     * the program refused it with AccountNotSigner. That is the whole point of
     * building this by hand: a list copied from the generated type can only ever
     * agree with the generated type.
     */
    let accounts = vec![
        AccountMeta::new_readonly(pda(&[b"global"]), false),
        AccountMeta::new(obligation, false),
        AccountMeta::new(borrower.pubkey(), true),
        AccountMeta::new(reserve, false),
        AccountMeta::new_readonly(oracle, false),
        AccountMeta::new_readonly(cook.mint, false),
        AccountMeta::new(liquidity_vault, false),
        AccountMeta::new(ata(&borrower.pubkey(), &cook.mint), false),
        AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
        /*
         * The per-wallet borrow cap's config, added in v0.3 and appended last so
         * every account index above it is unchanged.
         *
         * Passed even though this reserve has none. It is deliberately not an
         * `Option`: an optional account can be omitted by the caller, and the
         * cap would then be evadable by leaving it out. The address is derived
         * from the reserve, so a config belonging to another reserve is refused,
         * and an account that was never created reads as unlimited.
         */
        AccountMeta::new_readonly(pda(&[b"risk_config", reserve.as_ref()]), false),
    ];

    let before = env.balance(&ata(&borrower.pubkey(), &cook.mint));

    let result = env.send_raw(
        vec![
            env.accrue_ix(&cook),
            env.accrue_ix(&bcook),
            env.refresh_obligation_ix(obligation),
            Instruction {
                program_id: aera::id(),
                accounts,
                data,
            },
        ],
        &[&borrower],
    );

    /*
     * The account list above is written from the source. If the program's real
     * `Borrow` struct differs - an extra account, a different order, a flag - the
     * hand-built call fails here while the generated one succeeds, and the
     * failure message is the finding.
     */
    result.unwrap_or_else(|e| {
        panic!("a hand-built borrow was refused; the layout is not what the source says: {e}")
    });

    /*
     * The payout is the draw less the origination fee, and that is the point of
     * the check: the hand-built instruction must move exactly what the
     * generated one moves, fee included.
     */
    let fee = amount * u64::from(env.read_reserve(&cook).config.origination_fee_bps) / 10_000;
    let after = env.balance(&ata(&borrower.pubkey(), &cook.mint));
    assert_eq!(
        after - before,
        amount - fee,
        "the hand-built borrow paid the wrong amount"
    );
    assert_solvent(&env, &cook, "raw borrow");
}
