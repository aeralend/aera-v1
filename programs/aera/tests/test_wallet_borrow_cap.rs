//! The per-wallet borrow cap.
//!
//! A cap on how much one wallet may owe in one reserve. It is **not** Sybil
//! resistance — a person can open a second wallet and nothing here stops them —
//! and it must never be described as such.
//!
//! What it bounds is the size of a single liquidation. Against COOKHOUSE's
//! measured pool depth a liquidator's profit peaks near 50,000 COOK of debt
//! closed and falls after it: at 75,000 the sale slips 9.17% against a 12%
//! bonus, and past roughly 100,000 liquidation stops paying at all. A
//! market-wide borrow cap does not stop one borrower reaching that size alone.
//!
//! The cap lives in a `RiskConfig` PDA rather than on `ReserveConfig`, because
//! growing `Reserve` would leave v0.2 reserves undeserialisable to this program
//! — which breaks `accrue`, which breaks repayment for the whole migration
//! window. `migrate.rs:145` records that being tried and caught by
//! `test_half_migrated::half_06`.

mod common;

use anchor_lang::prelude::Pubkey;
use common::*;
use solana_keypair::Keypair;

fn actor(env: &mut Env, mint: Pubkey, amount: u64) -> Keypair {
    let user = env.create_user();
    env.fund(&user, mint, amount);
    user
}

/// Debt is denominated in COOK, whose reserve is the borrowable one.
const CAP: u64 = 25_000;

/// Open a funded position with plenty of collateral, so the cap is the only
/// thing that can refuse a borrow.
fn market_with_cap(cap: u64) -> (Env, ReserveHandle, ReserveHandle, Keypair, Pubkey) {
    let (mut env, cook, bcook) = Env::core(1_000);

    let supplier = actor(&mut env, cook.mint, tokens(1_000_000));
    env.supply(&supplier, &cook, tokens(1_000_000));

    let borrower = actor(&mut env, bcook.mint, tokens(1_000_000));
    env.fund(&borrower, cook.mint, 0);
    let obligation = env.open_position(&borrower, &bcook, tokens(1_000_000));

    if cap > 0 {
        env.set_risk_config(&cook, tokens(cap)).expect("set cap");
    }

    (env, cook, bcook, borrower, obligation)
}

// ---------------------------------------------------------------------------
// The boundary
// ---------------------------------------------------------------------------

#[test]
fn cap_00_just_under_the_cap_is_allowed() {
    let (mut env, cook, bcook, borrower, obligation) = market_with_cap(CAP);

    env.try_borrow(
        &borrower,
        &cook,
        obligation,
        tokens(CAP) - 1,
        &[&cook, &bcook],
    )
    .expect("one raw unit under the cap must be allowed");
}

#[test]
fn cap_01_exactly_the_cap_is_allowed() {
    let (mut env, cook, bcook, borrower, obligation) = market_with_cap(CAP);

    // `<=`, not `<`. A cap of 25,000 that refused 25,000 would be a cap of
    // 24,999 and every operator reading the config would be wrong by one unit.
    env.try_borrow(&borrower, &cook, obligation, tokens(CAP), &[&cook, &bcook])
        .expect("exactly the cap must be allowed");
}

#[test]
fn cap_02_one_raw_unit_over_the_cap_is_refused() {
    let (mut env, cook, bcook, borrower, obligation) = market_with_cap(CAP);

    let result = env.try_borrow(
        &borrower,
        &cook,
        obligation,
        tokens(CAP) + 1,
        &[&cook, &bcook],
    );
    assert!(
        result.is_err(),
        "the smallest possible overshoot was accepted"
    );
}

// ---------------------------------------------------------------------------
// It is a cap on the position, not on one instruction
// ---------------------------------------------------------------------------

#[test]
fn cap_03_sequential_borrows_cannot_walk_past_the_cap() {
    let (mut env, cook, bcook, borrower, obligation) = market_with_cap(CAP);

    /*
     * Amounts differ so the transactions differ.
     *
     * Four identical borrows in one blockhash are one transaction as far as the
     * runtime is concerned, and the second returns `AlreadyProcessed` rather
     * than executing -- which would have made this test pass for the wrong
     * reason had the assertion been the other way round.
     */
    for amount in [6_000u64, 6_500, 5_500, 6_000] {
        env.try_borrow(
            &borrower,
            &cook,
            obligation,
            tokens(amount),
            &[&cook, &bcook],
        )
        .unwrap_or_else(|e| panic!("borrowing {amount} while under the cap: {e}"));
    }

    // 24,000 owed. Another 6,000 would reach 30,000. Checking only the
    // instruction's own amount would let this through, which is exactly why the
    // check measures the obligation's existing debt.
    let result = env.try_borrow(
        &borrower,
        &cook,
        obligation,
        tokens(6_000),
        &[&cook, &bcook],
    );
    assert!(
        result.is_err(),
        "sequential borrows walked past a 25,000 cap"
    );
}

#[test]
fn cap_04_repaying_frees_room_again() {
    let (mut env, cook, bcook, borrower, obligation) = market_with_cap(CAP);

    env.try_borrow(&borrower, &cook, obligation, tokens(CAP), &[&cook, &bcook])
        .expect("borrow to the cap");
    assert!(
        env.try_borrow(
            &borrower,
            &cook,
            obligation,
            tokens(1_000),
            &[&cook, &bcook]
        )
        .is_err(),
        "at the cap, more debt must be refused"
    );

    env.try_repay(&borrower, &cook, obligation, tokens(10_000))
        .expect("repayment is never gated by a borrow cap");

    env.try_borrow(
        &borrower,
        &cook,
        obligation,
        tokens(5_000),
        &[&cook, &bcook],
    )
    .expect("room freed by repaying must be usable");
}

/// The cap blocks new debt. It must never block getting out of debt.
#[test]
fn cap_05_a_cap_lowered_below_existing_debt_still_permits_repayment() {
    let (mut env, cook, bcook, borrower, obligation) = market_with_cap(CAP);

    env.try_borrow(
        &borrower,
        &cook,
        obligation,
        tokens(20_000),
        &[&cook, &bcook],
    )
    .expect("borrow under the cap");

    // Tightening lands immediately, and now the wallet is over its cap.
    env.set_risk_config(&cook, tokens(5_000))
        .expect("tightening is immediate");

    assert!(
        env.try_borrow(&borrower, &cook, obligation, 1, &[&cook, &bcook])
            .is_err(),
        "a wallet over its cap must not add debt"
    );
    env.try_repay(&borrower, &cook, obligation, tokens(5_000))
        .expect("a wallet over its cap must still be able to repay");
}

// ---------------------------------------------------------------------------
// Absent means unlimited
// ---------------------------------------------------------------------------

#[test]
fn cap_06_a_reserve_with_no_config_is_unlimited() {
    // Core is deliberately left without one: its collateral is a stake-pool
    // rate that cannot be moved by trading, and its liquidations exit into a
    // far deeper book.
    let (mut env, cook, bcook, borrower, obligation) = market_with_cap(0);

    assert!(
        env.read_risk_config(&cook).is_none(),
        "no config should exist"
    );
    env.try_borrow(
        &borrower,
        &cook,
        obligation,
        tokens(400_000),
        &[&cook, &bcook],
    )
    .expect("a reserve with no risk config must be unconstrained");
}

#[test]
fn cap_07_zero_means_unlimited_not_frozen() {
    let (mut env, cook, bcook, borrower, obligation) = market_with_cap(CAP);

    // Zero is the loosest value everywhere else in Aera (`supply_cap`,
    // `borrow_cap`, `per_wallet_supply_cap`). Two caps in one protocol that
    // meant opposite things by zero would be a configuration footgun.
    env.set_risk_config(&cook, 0).expect("queue the loosening");
    env.warp_seconds(24 * 60 * 60 + 1);
    env.apply_pending_risk_config(&cook).expect("apply");

    env.try_borrow(
        &borrower,
        &cook,
        obligation,
        tokens(200_000),
        &[&cook, &bcook],
    )
    .expect("zero must mean unlimited, not frozen");
}

// ---------------------------------------------------------------------------
// Timelock
// ---------------------------------------------------------------------------

#[test]
fn cap_08_tightening_is_immediate_loosening_waits() {
    let (mut env, cook, _bcook, _borrower, _obligation) = market_with_cap(CAP);

    env.set_risk_config(&cook, tokens(10_000))
        .expect("tightening");
    assert_eq!(
        env.read_risk_config(&cook).unwrap().per_wallet_borrow_cap,
        tokens(10_000),
        "a tightening must land immediately"
    );

    env.set_risk_config(&cook, tokens(50_000))
        .expect("queue a loosening");
    assert_eq!(
        env.read_risk_config(&cook).unwrap().per_wallet_borrow_cap,
        tokens(10_000),
        "a loosening must not take effect before its delay"
    );

    assert!(
        env.apply_pending_risk_config(&cook).is_err(),
        "applying early must be refused"
    );

    env.warp_seconds(24 * 60 * 60 + 1);
    env.apply_pending_risk_config(&cook).expect("apply");
    assert_eq!(
        env.read_risk_config(&cook).unwrap().per_wallet_borrow_cap,
        tokens(50_000),
    );
}

#[test]
fn cap_09_a_tightening_supersedes_a_queued_loosening() {
    let (mut env, cook, _bcook, _borrower, _obligation) = market_with_cap(CAP);

    env.set_risk_config(&cook, tokens(100_000))
        .expect("queue a big loosening");
    env.set_risk_config(&cook, tokens(5_000))
        .expect("then tighten during an incident");

    // Otherwise an operator who tightened under pressure would find yesterday's
    // raise still sitting there, ready to undo it a day later.
    let config = env.read_risk_config(&cook).unwrap();
    assert_eq!(config.per_wallet_borrow_cap, tokens(5_000));
    assert_eq!(
        config.pending_eta, 0,
        "the queued loosening must be cleared"
    );

    env.warp_seconds(24 * 60 * 60 + 1);
    assert!(
        env.apply_pending_risk_config(&cook).is_err(),
        "there must be nothing left to apply"
    );
}

// ---------------------------------------------------------------------------
// It cannot be evaded
// ---------------------------------------------------------------------------

/// One wallet has exactly one obligation per market, so debt cannot be split.
#[test]
fn cap_10_one_wallet_has_one_obligation_per_market() {
    let (mut env, _cook, bcook, borrower, obligation) = market_with_cap(CAP);

    // `init_obligation` opens `["obligation", market, owner]` with `init`, so a
    // second attempt hits an already-initialised account.
    let second = env.try_open_obligation(&borrower);
    assert!(
        second.is_err(),
        "a wallet opened a second obligation in one market, which would split \
         its debt across two and evade the cap entirely"
    );

    let _ = (bcook, obligation);
}
