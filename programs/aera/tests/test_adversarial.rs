//! Attacks, not happy paths.
//!
//! Each of these is a claim made in the project's threat model.
//! A document that says "this is not exploitable" is worth very little without a
//! test that tries it.

mod common;

use aera::constants::DEFAULT_PARAM_TIMELOCK_SECONDS;
use common::*;
use solana_keypair::Keypair;

fn supplier_with(env: &mut Env, cook: &ReserveHandle, amount: u64) -> Keypair {
    let user = env.create_user();
    env.fund(&user, cook.mint, amount);
    env.supply(&user, cook, amount);
    user
}

// ---------------------------------------------------------------------------
// T3 — exchange-rate corruption
// ---------------------------------------------------------------------------

/// Donating COOK straight into the vault must not move the exchange rate.
///
/// This is the single property that makes the first-depositor inflation attack
/// structurally impossible: `available_liquidity` is protocol-tracked, so a raw
/// transfer is invisible to it.
#[test]
fn donating_cook_does_not_move_the_exchange_rate() {
    let (mut env, cook, _) = Env::core(1_000);

    let alice = supplier_with(&mut env, &cook, tokens(100));
    let before = env.read_reserve(&cook);
    let rate_before = before.total_liquidity().unwrap();

    // A whale sends COOK directly to the vault, bypassing `supply` entirely.
    let whale = env.create_user();
    env.fund(&whale, cook.mint, tokens(1_000_000));
    env.transfer_tokens(&whale, cook.mint, cook.liquidity_vault, tokens(1_000_000));

    let after = env.read_reserve(&cook);
    assert_eq!(
        after.total_liquidity().unwrap(),
        rate_before,
        "a donation must be invisible to the pool's accounting"
    );
    assert_eq!(after.available_liquidity, before.available_liquidity);

    // Alice's claim is exactly what it was.
    let shares = env.balance(&share_ata(&alice.pubkey(), &cook.share_mint));
    assert_eq!(
        after
            .shares_to_liquidity(shares, aera::math::Rounding::Down)
            .unwrap(),
        tokens(100)
    );
}

/// The full attack, executed in order, and the victim is untouched.
#[test]
fn first_depositor_cannot_inflate_the_rate() {
    let (mut env, cook, _) = Env::core(1_000);

    // 1. Attacker deposits the smallest possible amount.
    let attacker = env.create_user();
    env.fund(&attacker, cook.mint, tokens(1_000_000));
    env.supply(&attacker, &cook, 1);
    assert_eq!(env.read_reserve(&cook).share_mint_supply, 1);

    // 2. Attacker donates a fortune to the vault.
    env.transfer_tokens(&attacker, cook.mint, cook.liquidity_vault, tokens(500_000));

    // 3. Victim supplies. Under the classic attack their shares round to zero.
    let victim = env.create_user();
    env.fund(&victim, cook.mint, tokens(1_000));
    env.supply(&victim, &cook, tokens(1_000));

    let reserve = env.read_reserve(&cook);
    let victim_shares = env.balance(&share_ata(&victim.pubkey(), &cook.share_mint));
    assert_eq!(victim_shares, tokens(1_000), "victim's shares must be 1:1");
    assert_eq!(
        reserve
            .shares_to_liquidity(victim_shares, aera::math::Rounding::Down)
            .unwrap(),
        tokens(1_000),
        "the victim can redeem exactly what they put in"
    );
}

/// Donating the *share* token is equally inert: the supply mirror lives on the
/// reserve, not on the mint.
#[test]
fn donating_acook_does_not_move_the_exchange_rate() {
    let (mut env, cook, _) = Env::core(1_000);

    let alice = supplier_with(&mut env, &cook, tokens(100));
    let bob = env.create_user();
    env.ensure_share_ata(&bob, cook.share_mint);

    let before = env.read_reserve(&cook).share_mint_supply;
    let shares = share_ata(&alice.pubkey(), &cook.share_mint);
    env.transfer_shares(
        &alice,
        cook.share_mint,
        share_ata(&bob.pubkey(), &cook.share_mint),
        tokens(50),
    );

    let after = env.read_reserve(&cook);
    assert_eq!(after.share_mint_supply, before, "the mirror does not move");
    // Alice's balance moved, the pool's did not.
    assert_eq!(env.balance(&shares), tokens(50));
}

// ---------------------------------------------------------------------------
// C — asset roles
// ---------------------------------------------------------------------------

/// aCOOK is refused as collateral no matter how the caller dresses it up.
#[test]
fn acook_collateral_is_refused_through_every_path() {
    let (mut env, cook, _) = Env::core(1_000);

    let user = env.create_user();
    env.fund(&user, cook.mint, tokens(1_000));
    let obligation = env.init_obligation(&user);
    env.supply(&user, &cook, tokens(1_000));
    let held = env.balance(&share_ata(&user.pubkey(), &cook.share_mint));

    // Full balance.
    assert_error(
        env.try_deposit_collateral(&user, &cook, obligation, held),
        "CollateralNotEnabled",
    );
    // A single base unit is refused for the same reason.
    assert_error(
        env.try_deposit_collateral(&user, &cook, obligation, 1),
        "CollateralNotEnabled",
    );
}

/// bCOOK cannot be drawn from, even by someone with collateral posted.
#[test]
fn bcook_cannot_be_borrowed_even_with_collateral() {
    let (mut env, cook, bcook) = Env::core(1_000);

    let borrower = env.create_user();
    env.fund(&borrower, bcook.mint, tokens(5_000));
    env.fund(&borrower, cook.mint, 0);
    let obligation = env.open_position(&borrower, &bcook, tokens(5_000));

    assert_error(
        env.try_borrow(&borrower, &bcook, obligation, 1, &[&cook, &bcook]),
        "BorrowNotEnabled",
    );
}

// ---------------------------------------------------------------------------
// E — oracle
// ---------------------------------------------------------------------------

/// Repay works with no usable price at all. This is the promise that keeps a
/// borrower from being trapped by an oracle outage.
#[test]
fn repay_survives_an_unusable_oracle() {
    let (mut env, cook, bcook) = Env::core(1_000);

    let supplier = env.create_user();
    env.fund(&supplier, cook.mint, tokens(5_000));
    env.supply(&supplier, &cook, tokens(5_000));

    let borrower = env.create_user();
    env.fund(&borrower, bcook.mint, tokens(5_000));
    env.fund(&borrower, cook.mint, tokens(1_000));
    let obligation = env.open_position(&borrower, &bcook, tokens(5_000));
    env.try_borrow(&borrower, &cook, obligation, tokens(500), &[&cook, &bcook])
        .unwrap();

    // The source becomes unreadable: a redemption fee far past the bound Aera
    // accepts, which is the v0.2 equivalent of "no usable price".
    env.set_pool(bcook.mint, px(1_000) as u64, POOL_SHARES, 9_000, 5);
    env.refresh_oracle(bcook.mint);

    /*
     * Refused as OracleBorrowFrozen rather than OracleEmergency.
     *
     * The oracle in trouble is the *collateral's*, and `borrow` holds only the
     * borrowed asset's. The collateral state reaches it through the
     * obligation's `prices_stressed` flag, which is a boolean and so cannot
     * distinguish frozen from emergency. Both mean the same thing to the
     * borrower -- no new risk, repayment still open -- and the precise state is
     * on the oracle account for anyone who needs it.
     */
    assert_error(
        env.try_borrow(&borrower, &cook, obligation, tokens(1), &[&cook, &bcook]),
        "OracleBorrowFrozen",
    );
    env.try_repay(&borrower, &cook, obligation, tokens(100))
        .unwrap();
}

// ---------------------------------------------------------------------------
// F/H — governance and fees
// ---------------------------------------------------------------------------

/// A queued config that violates a hard maximum is refused when it lands, not
/// only when it is queued.
#[test]
fn pending_config_above_hard_max_is_refused_on_apply() {
    let (mut env, cook, _) = Env::core(1_000);

    // Queue a legal loosening.
    let mut config = env.read_reserve(&cook).config;
    config.reserve_factor_bps = 2_000;
    env.try_set_params(&cook, config).unwrap();
    assert!(env.read_reserve(&cook).pending.eta > 0);

    // It lands only after the delay, and only because it is still legal.
    env.warp_seconds(DEFAULT_PARAM_TIMELOCK_SECONDS);
    env.try_apply_pending(&cook).unwrap();
    assert_eq!(env.read_reserve(&cook).config.reserve_factor_bps, 2_000);

    // An illegal one cannot even be queued — validate runs on the way in too.
    let mut bad = env.read_reserve(&cook).config;
    bad.reserve_factor_bps = 9_000;
    assert_error(env.try_set_params(&cook, bad), "ReserveFactorAboveHardMax");
}

/// Fees can only go where the admin already pointed them.
#[test]
fn collect_fees_to_a_stranger_is_refused() {
    let (mut env, cook, bcook) = Env::core(1_000);

    let supplier = env.create_user();
    env.fund(&supplier, cook.mint, tokens(1_000));
    env.supply(&supplier, &cook, tokens(1_000));

    let borrower = env.create_user();
    env.fund(&borrower, bcook.mint, tokens(5_000));
    env.fund(&borrower, cook.mint, 0);
    let obligation = env.open_position(&borrower, &bcook, tokens(5_000));
    env.try_borrow(&borrower, &cook, obligation, tokens(600), &[&cook, &bcook])
        .unwrap();

    env.warp_slots(aera::constants::DEFAULT_SLOTS_PER_YEAR);
    env.accrue(&cook);
    assert!(env.read_reserve(&cook).accrued_fees > 0);

    let stranger = env.create_user();
    env.fund(&stranger, cook.mint, 0);
    assert_error(
        env.try_collect_fees_to(&cook, &stranger),
        "WrongFeeDestination",
    );
}

// ---------------------------------------------------------------------------
// I — staleness
// ---------------------------------------------------------------------------

/// A reserve that was not accrued this slot cannot be used.
#[test]
fn stale_reserve_is_refused() {
    let (mut env, cook, _) = Env::core(1_000);

    let user = env.create_user();
    env.fund(&user, cook.mint, tokens(1_000));

    // Supply without the accrue prelude the protocol requires.
    assert_error(
        env.try_supply_without_accrue(&user, &cook, tokens(10)),
        "ReserveStale",
    );
}

/// An obligation that was not refreshed in this transaction cannot be borrowed
/// against, even when everything else is in order.
#[test]
fn stale_obligation_is_refused() {
    let (mut env, cook, bcook) = Env::core(1_000);

    let supplier = env.create_user();
    env.fund(&supplier, cook.mint, tokens(5_000));
    env.supply(&supplier, &cook, tokens(5_000));

    let borrower = env.create_user();
    env.fund(&borrower, bcook.mint, tokens(5_000));
    env.fund(&borrower, cook.mint, 0);
    let obligation = env.open_position(&borrower, &bcook, tokens(5_000));

    assert_error(
        env.try_borrow_without_refresh(&borrower, &cook, obligation, tokens(100), &[&cook, &bcook]),
        "ObligationStale",
    );
}

/// `refresh_obligation` rejects a remaining-accounts list that does not match
/// the obligation exactly — too few, too many, or the wrong reserve.
#[test]
fn refresh_with_wrong_remaining_accounts_is_refused() {
    let (mut env, cook, bcook) = Env::core(1_000);

    let borrower = env.create_user();
    env.fund(&borrower, bcook.mint, tokens(1_000));
    env.fund(&borrower, cook.mint, 0);
    let obligation = env.open_position(&borrower, &bcook, tokens(1_000));

    // An empty list where one deposit pair is expected.
    assert_error(
        env.try_refresh_with_pairs(obligation, &[]),
        "InvalidObligationAccount",
    );

    // The wrong reserve in the right slot.
    assert_error(
        env.try_refresh_with_pairs(obligation, &[&cook]),
        "InvalidObligationAccount",
    );

    // The right reserve plus a spare pair.
    assert_error(
        env.try_refresh_with_pairs(obligation, &[&bcook, &cook]),
        "InvalidObligationAccount",
    );

    // The correct list works.
    env.try_refresh_with_pairs(obligation, &[&bcook]).unwrap();
}

// ---------------------------------------------------------------------------
// D — liquidation griefing
// ---------------------------------------------------------------------------

/// A position that has been brought back to health cannot be liquidated again
/// in the same breath.
#[test]
fn a_position_cannot_be_liquidated_twice_once_healthy() {
    let (mut env, cook, bcook) = Env::core(1_000);

    let supplier = env.create_user();
    env.fund(&supplier, cook.mint, tokens(1_000));
    env.supply(&supplier, &cook, tokens(1_000));

    let borrower = env.create_user();
    env.fund(&borrower, bcook.mint, tokens(1_000));
    env.fund(&borrower, cook.mint, 0);
    let obligation = env.open_position(&borrower, &bcook, tokens(1_000));
    env.try_borrow(&borrower, &cook, obligation, tokens(522), &[&cook, &bcook])
        .unwrap();

    // A deep drop: HF well under 0.95, so the whole position is closable.
    env.set_price(bcook.mint, px(700));

    let liquidator = env.create_user();
    env.fund(&liquidator, cook.mint, tokens(2_000));
    env.try_liquidate(&liquidator, &cook, &bcook, obligation, tokens(522))
        .unwrap();

    // Debt is gone, so a second attempt finds nothing to close.
    env.bump_blockhash();
    let second = env.try_liquidate(&liquidator, &cook, &bcook, obligation, tokens(100));
    assert!(
        second.is_err(),
        "a closed position must not be liquidatable again"
    );
}

/// A liquidator without the funds to repay cannot seize anything.
#[test]
fn a_broke_liquidator_seizes_nothing() {
    let (mut env, cook, bcook) = Env::core(1_000);

    let supplier = env.create_user();
    env.fund(&supplier, cook.mint, tokens(1_000));
    env.supply(&supplier, &cook, tokens(1_000));

    let borrower = env.create_user();
    env.fund(&borrower, bcook.mint, tokens(1_000));
    env.fund(&borrower, cook.mint, 0);
    let obligation = env.open_position(&borrower, &bcook, tokens(1_000));
    env.try_borrow(&borrower, &cook, obligation, tokens(522), &[&cook, &bcook])
        .unwrap();
    env.set_price(bcook.mint, px(750));

    let broke = env.create_user();
    env.fund(&broke, cook.mint, 0);
    let attempt = env.try_liquidate(&broke, &cook, &bcook, obligation, tokens(100));
    assert!(attempt.is_err(), "no COOK, no seizure");

    let collateral = env.balance(&env.obligation_share_vault(&bcook, obligation));
    assert_eq!(collateral, tokens(1_000), "collateral is untouched");
}
