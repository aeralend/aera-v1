//! Collateral valuation, the haircut, and the borrow limit.

mod common;

use aera::constants::{DEFAULT_LTV_BPS, FIXED_POINT_SCALE};
use common::*;

/// A COOK amount as a FIXED_POINT_SCALE-scaled value.
fn value(whole_cook: u64) -> u128 {
    whole_cook as u128 * FIXED_POINT_SCALE
}

/// Two suppliers, because the per-wallet cap is 1,000,000 COOK.
fn fill_cook_pool(env: &mut Env, cook: &ReserveHandle, each: u64) {
    for _ in 0..2 {
        let supplier = env.create_user();
        env.fund(&supplier, cook.mint, each);
        env.supply(&supplier, cook, each);
    }
}

/// PARAMS.md's worked example, asserted against the program rather than a
/// spreadsheet:
///
///   2,000,000 bCOOK at 1.2 COOK
///   face             = 2,000,000 * 1.2       = 2,400,000 COOK
///   after 5% haircut = 2,400,000 * 0.95      = 2,280,000 COOK
///   max borrow       = 2,280,000 * 0.55      = 1,254,000 COOK
///   liquidation line = 2,280,000 * 0.65      = 1,482,000 COOK
#[test]
fn worked_example_matches_the_spec() {
    let (mut env, cook, bcook) = Env::core(1_200);

    let borrower = env.create_user();
    env.fund(&borrower, bcook.mint, tokens(2_000_000));
    env.fund(&borrower, cook.mint, 0);
    let obligation = env.open_position(&borrower, &bcook, tokens(2_000_000));

    fill_cook_pool(&mut env, &cook, tokens(1_000_000));

    // Refresh so the cached valuations are populated.
    env.try_borrow(&borrower, &cook, obligation, 1, &[&cook, &bcook])
        .unwrap();

    let state = env.read_obligation(obligation);
    assert_eq!(state.deposited_value, value(2_400_000), "face value");
    assert_eq!(
        state.effective_collateral_value,
        value(2_280_000),
        "value after the 5% bCOOK haircut"
    );
    assert_eq!(
        state.allowed_borrow_value,
        value(1_254_000),
        "max borrow at 55% LTV"
    );
    assert_eq!(
        state.unhealthy_borrow_value,
        value(1_482_000),
        "liquidation line at 65% LT"
    );
}

/// The limit is enforced, not merely reported: one base unit past it fails.
#[test]
fn borrow_beyond_ltv_is_refused() {
    let (mut env, cook, bcook) = Env::core(1_200);

    let borrower = env.create_user();
    env.fund(&borrower, bcook.mint, tokens(1_000));
    env.fund(&borrower, cook.mint, 0);
    let obligation = env.open_position(&borrower, &bcook, tokens(1_000));

    fill_cook_pool(&mut env, &cook, tokens(1_000_000));

    // 1,000 bCOOK * 1.2 * 0.95 * 0.55 = 627 COOK.
    let limit = tokens(627);

    assert_error(
        env.try_borrow(&borrower, &cook, obligation, limit + 1, &[&cook, &bcook]),
        "BorrowTooLarge",
    );

    env.try_borrow(&borrower, &cook, obligation, limit, &[&cook, &bcook])
        .unwrap();
}

/// The haircut is what separates Aera's limit from a naive LTV: without it the
/// same collateral would support 5% more debt.
#[test]
fn haircut_reduces_borrow_power() {
    let (mut env, cook, bcook) = Env::core(1_000);

    let borrower = env.create_user();
    env.fund(&borrower, bcook.mint, tokens(1_000));
    env.fund(&borrower, cook.mint, 0);
    let obligation = env.open_position(&borrower, &bcook, tokens(1_000));
    fill_cook_pool(&mut env, &cook, tokens(1_000_000));

    env.try_borrow(&borrower, &cook, obligation, 1, &[&cook, &bcook])
        .unwrap();
    let state = env.read_obligation(obligation);

    // Face 1,000 COOK; a naive 55% would allow 550.
    assert_eq!(state.deposited_value, value(1_000));
    let naive = value(1_000) * DEFAULT_LTV_BPS as u128 / 10_000;
    assert_eq!(naive, value(550));
    // Aera allows 1,000 * 0.95 * 0.55 = 522.5.
    assert_eq!(
        state.allowed_borrow_value,
        value(1_000) * 9_500 / 10_000 * 5_500 / 10_000
    );
    assert!(state.allowed_borrow_value < naive);
}

/// Collateral cannot be pulled back out from under a live loan.
#[test]
fn withdrawing_collateral_under_a_loan_is_refused() {
    let (mut env, cook, bcook) = Env::core(1_200);

    let borrower = env.create_user();
    env.fund(&borrower, bcook.mint, tokens(1_000));
    env.fund(&borrower, cook.mint, 0);
    let obligation = env.open_position(&borrower, &bcook, tokens(1_000));
    fill_cook_pool(&mut env, &cook, tokens(1_000_000));

    env.try_borrow(&borrower, &cook, obligation, tokens(600), &[&cook, &bcook])
        .unwrap();

    // Pulling the whole bag would leave 600 COOK of debt with no collateral.
    assert_error(
        env.try_withdraw_collateral(
            &borrower,
            &bcook,
            obligation,
            tokens(1_000),
            &[&cook, &bcook],
        ),
        "WithdrawTooLarge",
    );

    // A small slice that keeps the position inside its limit is fine.
    env.try_withdraw_collateral(&borrower, &bcook, obligation, tokens(10), &[&cook, &bcook])
        .unwrap();
}

/// Health factor is reported as a ratio against the liquidation line, and a
/// position with no debt has none.
#[test]
fn health_factor_is_the_ratio_to_the_liquidation_line() {
    let (mut env, cook, bcook) = Env::core(1_000);

    let borrower = env.create_user();
    env.fund(&borrower, bcook.mint, tokens(1_000));
    env.fund(&borrower, cook.mint, 0);
    let obligation = env.open_position(&borrower, &bcook, tokens(1_000));
    fill_cook_pool(&mut env, &cook, tokens(1_000_000));

    // No debt yet.
    env.try_borrow(&borrower, &cook, obligation, 1, &[&cook, &bcook])
        .unwrap();

    // Borrow 400 against a 617.5 liquidation line (1000 * 0.95 * 0.65).
    env.try_borrow(&borrower, &cook, obligation, tokens(400), &[&cook, &bcook])
        .unwrap();
    // Refresh to update cached values after the borrow.
    env.try_repay(&borrower, &cook, obligation, 1).unwrap();
    env.try_borrow(&borrower, &cook, obligation, 1, &[&cook, &bcook])
        .unwrap();

    let state = env.read_obligation(obligation);
    let hf = state.health_factor_bps().unwrap().unwrap();
    // 617.5 / ~400 is about 1.54
    assert!(
        (15_000..=15_600).contains(&hf),
        "health factor {hf} bps outside the expected band"
    );
    assert!(!state.is_liquidatable());
}
