//! STEAL-001..010 — first depositor, inflation, donation.
//!
//! This is the family that empties a lending vault. The classic attack: be the
//! first depositor for one unit, donate a large amount straight into the vault
//! so one share is suddenly worth a fortune, then let a victim deposit and
//! redeem their rounding loss.
//!
//! Aera's claimed defence is structural rather than a virtual-share offset:
//! `Reserve::available_liquidity` is protocol-tracked and only moves inside an
//! instruction, so a raw token transfer into the vault is invisible to the
//! exchange rate. These tests exist to prove that claim rather than repeat it,
//! and to measure what a donor actually loses when they try.
//!
//! Every test ends by asserting the reserve is still solvent. An attack that is
//! "blocked" but leaves the book short is still a loss.

mod common;

use common::audit::*;
use common::*;
use solana_keypair::Keypair;

/// A funded actor holding `amount` of `mint`.
fn actor(env: &mut Env, mint: Pubkey, amount: u64) -> Keypair {
    let user = env.create_user();
    env.fund(&user, mint, amount);
    user
}

// ---------------------------------------------------------------------------
// STEAL-001 — the classic: seed one unit, donate, wait for a victim
// ---------------------------------------------------------------------------

#[test]
fn steal_001_donate_then_front_run_a_large_deposit() {
    let (mut env, cook, _bcook) = Env::core(1_000);

    let attacker = actor(&mut env, cook.mint, tokens(1_000_000));
    let victim = actor(&mut env, cook.mint, tokens(1_000_000));

    // 1. Be the first depositor, for the smallest amount that mints anything.
    env.supply(&attacker, &cook, 1);
    let shares_after_seed = env.balance(&share_ata(&attacker.pubkey(), &cook.share_mint));
    assert_eq!(shares_after_seed, 1, "first deposit should mint 1:1");

    // 2. Donate a fortune straight into the vault ATA, bypassing every
    //    instruction. If the exchange rate reads the token balance, one share is
    //    now worth ~1,000,000 COOK.
    let donation = tokens(1_000_000) - 1;
    env.transfer_tokens(&attacker, cook.mint, cook.liquidity_vault, donation);

    let rate_after_donation = env.acook_rate(&cook);
    assert_eq!(
        rate_after_donation, FIXED_POINT,
        "a donation moved the share rate — the inflation attack is live"
    );

    // 3. The victim deposits. If the rate had moved, they would mint 0 shares
    //    and their whole deposit would belong to the attacker's single share.
    let deposit = tokens(1_000_000);
    env.supply(&victim, &cook, deposit);
    let victim_shares = env.balance(&share_ata(&victim.pubkey(), &cook.share_mint));
    assert_eq!(
        victim_shares, deposit,
        "victim should mint 1:1 — they minted {victim_shares} for {deposit}"
    );

    // 4. The attacker redeems everything they hold and must not come out ahead.
    let price = 1_000_000_000_000_000_000u128;
    let rate = env.acook_rate(&cook);
    let before = env.value_of(&attacker.pubkey(), &cook, &_bcook, None);

    env.try_withdraw(&attacker, &cook, shares_after_seed)
        .expect("attacker may redeem their own share");

    let after = env.value_of(&attacker.pubkey(), &cook, &_bcook, None);

    // The donation is gone: it was a gift to the pool, not a claim.
    assert!(
        after.cook <= before.cook + 2,
        "attacker recovered {} COOK from a 1-unit share",
        after.cook.saturating_sub(before.cook)
    );
    assert_no_profit(before, after, price, rate, 2, "STEAL-001");
    assert_solvent(&env, &cook, "STEAL-001");
}

// ---------------------------------------------------------------------------
// STEAL-002 — one lamport in, a million donated
// ---------------------------------------------------------------------------

#[test]
fn steal_002_one_unit_deposit_then_massive_donation() {
    let (mut env, cook, bcook) = Env::core(1_000);
    let attacker = actor(&mut env, cook.mint, tokens(2_000_000));

    env.supply(&attacker, &cook, 1);
    env.transfer_tokens(
        &attacker,
        cook.mint,
        cook.liquidity_vault,
        tokens(1_000_000),
    );

    // The reserve's own accounting must still say it holds one unit.
    let reserve = env.read_reserve(&cook);
    assert_eq!(
        reserve.available_liquidity, 1,
        "donation entered available_liquidity — the rate is now attacker-controlled"
    );
    assert_eq!(reserve.share_mint_supply, 1);

    // And the vault genuinely holds more than it tracks, which is the safe
    // direction: nobody can withdraw the donation.
    let solvency = env.solvency(&cook);
    assert!(solvency.vault_tokens > solvency.tracked_available);
    assert_solvent(&env, &cook, "STEAL-002");

    let _ = bcook;
}

// ---------------------------------------------------------------------------
// STEAL-003 — can a deposit ever mint zero shares?
// ---------------------------------------------------------------------------

#[test]
fn steal_003_a_deposit_never_mints_zero_shares() {
    let (mut env, cook, _bcook) = Env::core(1_000);

    let seeder = actor(&mut env, cook.mint, tokens(1_000));
    env.supply(&seeder, &cook, tokens(1_000));

    // Drive the rate up honestly, through a borrower paying interest, then check
    // the smallest possible deposit still mints something. If it can mint zero,
    // COOK enters the vault and the depositor owns none of it.
    let borrower = actor(&mut env, _bcook.mint, tokens(10_000));
    // Borrowed COOK is paid into this account, so it must exist first.
    env.fund(&borrower, cook.mint, 0);
    let obligation = env.open_position(&borrower, &_bcook, tokens(10_000));
    env.try_borrow(&borrower, &cook, obligation, tokens(400), &[&cook, &_bcook])
        .expect("borrow against locked bCOOK");
    env.warp_slots(50_000_000);
    env.accrue(&cook);

    let rate = env.acook_rate(&cook);
    assert!(rate > FIXED_POINT, "interest should have lifted the rate");

    let dust = actor(&mut env, cook.mint, tokens(1));
    let result = env.try_supply(&dust, &cook, 1);

    match result {
        Ok(_) => {
            let minted = env.balance(&share_ata(&dust.pubkey(), &cook.share_mint));
            assert!(
                minted > 0,
                "1 unit of COOK entered the vault and minted 0 shares — it was donated, not deposited"
            );
        }
        Err(message) => {
            // Refusing the deposit is the other correct answer.
            assert!(
                message.contains("DepositTooSmall"),
                "unexpected refusal: {message}"
            );
        }
    }

    let _ = obligation;
    assert_solvent(&env, &cook, "STEAL-003");
}

// ---------------------------------------------------------------------------
// STEAL-004 — donate bCOOK to fake collateral
// ---------------------------------------------------------------------------

#[test]
fn steal_004_donating_bcook_does_not_create_collateral() {
    let (mut env, cook, bcook) = Env::core(1_000);
    let attacker = actor(&mut env, bcook.mint, tokens(100_000));

    let obligation = env.init_obligation(&attacker);
    env.transfer_tokens(
        &attacker,
        bcook.mint,
        bcook.liquidity_vault,
        tokens(100_000),
    );

    // The obligation records no collateral, so nothing may be borrowed.
    let account = env.read_obligation(obligation);
    assert!(
        account.deposits.is_empty(),
        "a raw transfer created a collateral entry"
    );

    let supplier = actor(&mut env, cook.mint, tokens(10_000));
    env.supply(&supplier, &cook, tokens(10_000));

    let result = env.try_borrow(&attacker, &cook, obligation, tokens(1), &[&cook, &bcook]);
    assert!(
        result.is_err(),
        "borrowed against donated bCOOK that was never posted as collateral"
    );

    assert_solvent(&env, &cook, "STEAL-004");
    assert_solvent(&env, &bcook, "STEAL-004 bcook");
}

// ---------------------------------------------------------------------------
// STEAL-005/006 — aCOOK is a receipt, never collateral
// ---------------------------------------------------------------------------

#[test]
fn steal_006_acook_cannot_be_posted_as_collateral() {
    let (mut env, cook, bcook) = Env::core(1_000);

    let supplier = actor(&mut env, cook.mint, tokens(10_000));
    env.supply(&supplier, &cook, tokens(10_000));

    let attacker = env.create_user();
    env.ensure_share_ata(&attacker, cook.share_mint);
    // Receiving aCOOK by transfer is allowed; using it as collateral is not.
    env.transfer_shares(
        &supplier,
        cook.share_mint,
        share_ata(&attacker.pubkey(), &cook.share_mint),
        tokens(10_000),
    );
    let obligation = env.init_obligation(&attacker);

    let result = env.try_deposit_collateral(&attacker, &cook, obligation, tokens(1_000));
    assert!(
        result.is_err(),
        "aCOOK was accepted as collateral — the COOK reserve has collateral_enabled true"
    );
    if let Err(message) = result {
        assert!(
            message.contains("CollateralNotEnabled"),
            "wrong refusal for aCOOK collateral: {message}"
        );
    }

    let _ = bcook;
    assert_solvent(&env, &cook, "STEAL-006");
}

// ---------------------------------------------------------------------------
// STEAL-010 — the share-maths boundaries
// ---------------------------------------------------------------------------

#[test]
fn steal_010_supply_boundaries_do_not_break_share_maths() {
    let (mut env, cook, _bcook) = Env::core(1_000);

    // Zero must be refused outright rather than minting nothing for nothing.
    let user = actor(&mut env, cook.mint, tokens(1_000_000));
    let zero = env.try_supply(&user, &cook, 0);
    assert!(zero.is_err(), "a zero supply was accepted");

    // A ladder of sizes, each of which must mint proportionally and keep the
    // reserve solvent.
    let mut expected_shares = 0u64;
    for amount in [1u64, 2, 10, 1_000, tokens(1), tokens(1_000)] {
        let before = env.balance(&share_ata(&user.pubkey(), &cook.share_mint));
        env.try_supply(&user, &cook, amount)
            .unwrap_or_else(|e| panic!("supply of {amount} failed: {e}"));
        let after = env.balance(&share_ata(&user.pubkey(), &cook.share_mint));

        let minted = after - before;
        assert!(minted > 0, "supply of {amount} minted no shares");
        expected_shares += minted;
        assert_eq!(after, expected_shares);
        assert_solvent(&env, &cook, "STEAL-010");
    }

    // Redeeming everything must not return more than was put in.
    let deposited: u64 = [1u64, 2, 10, 1_000, tokens(1), tokens(1_000)].iter().sum();
    let shares = env.balance(&share_ata(&user.pubkey(), &cook.share_mint));
    let cook_before = env.balance(&ata(&user.pubkey(), &cook.mint));
    env.try_withdraw(&user, &cook, shares)
        .expect("withdraw all");
    let returned = env.balance(&ata(&user.pubkey(), &cook.mint)) - cook_before;

    assert!(
        returned <= deposited,
        "redeemed {returned} for {deposited} deposited — value created from nothing"
    );
    assert_solvent(&env, &cook, "STEAL-010 final");
}
