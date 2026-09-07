//! The arithmetic nobody thought to fuzz.
//!
//! Behaviour tests exercise these functions through instructions, where a
//! multiply-before-divide error or a saturating index is masked by everything
//! else going on. Here they are called directly, across the same 0..50 grid, and
//! checked against the rule that decides who loses a unit:
//!
//!   **Amounts the user is owed round down. Amounts the user owes round up.**
//!
//! Invariant 11. Every drift of one base unit must land with the protocol, never
//! with the caller.

mod common;

use aera::constants::{BPS_DENOMINATOR, FIXED_POINT_SCALE};
use aera::math::{mul_div, mul_div_ceil, mul_div_floor, Rounding};
use aera::state::Reserve;
use common::*;

/// A reserve holding `liquidity` against `shares`, with nothing borrowed.
///
/// Built by hand so the share maths can be driven to ratios an instruction
/// sequence would take a long time to reach.
fn reserve_with(liquidity: u64, shares: u64, borrowed: u128, index: u128) -> Reserve {
    let mut reserve = Reserve {
        market: Pubkey::new_unique(),
        liquidity_mint: Pubkey::new_unique(),
        liquidity_vault: Pubkey::new_unique(),
        share_mint: Pubkey::new_unique(),
        oracle: Pubkey::new_unique(),
        liquidity_decimals: 9,
        available_liquidity: liquidity,
        share_mint_supply: shares,
        borrowed_principal: borrowed,
        borrow_index: index,
        last_update_slot: 0,
        accrued_fees: 0,
        config: cook_config(),
        pending: Default::default(),
        bump: 0,
    };
    reserve.config.slots_per_year = 67_609_680;
    reserve
}

// ---------------------------------------------------------------------------
// mul_div — the primitive everything else is built on
// ---------------------------------------------------------------------------

/// Floor never exceeds ceil, and they differ by at most one.
#[test]
fn mul_div_floor_and_ceil_bracket_the_true_value() {
    let mut checks = 0;

    for a in 0..=50u128 {
        for b in 0..=50u128 {
            for d in 1..=50u128 {
                let floor = mul_div_floor(a, b, d).unwrap();
                let ceil = mul_div_ceil(a, b, d).unwrap();

                assert!(
                    floor <= ceil,
                    "floor {floor} above ceil {ceil} for {a}*{b}/{d}"
                );
                assert!(
                    ceil - floor <= 1,
                    "floor and ceil differ by more than one unit for {a}*{b}/{d}"
                );

                // And each must be on the correct side of the exact value.
                let exact = a * b;
                assert!(
                    floor * d <= exact,
                    "floor is above the true value for {a}*{b}/{d}"
                );
                assert!(
                    ceil * d >= exact,
                    "ceil is below the true value for {a}*{b}/{d}"
                );

                // The dispatcher must agree with both.
                assert_eq!(mul_div(a, b, d, Rounding::Down).unwrap(), floor);
                assert_eq!(mul_div(a, b, d, Rounding::Up).unwrap(), ceil);
                checks += 4;
            }
        }
    }

    println!("mul_div assertions: {checks}");
    assert!(checks > 100_000);
}

/// Multiplying before dividing must not overflow at values the program reaches.
///
/// `borrowed_principal * borrow_index` is a u128 product of two large numbers,
/// and it is the one place a naive implementation runs out of room.
#[test]
fn mul_div_does_not_overflow_at_protocol_scale() {
    // A pool of a billion tokens at nine decimals, against a 1e18 index.
    let huge = 1_000_000_000u128 * 1_000_000_000;

    for index_multiple in [1u128, 2, 10, 100] {
        let index = FIXED_POINT_SCALE * index_multiple;
        let result = mul_div_floor(huge, index, FIXED_POINT_SCALE);
        assert!(
            result.is_ok(),
            "overflowed at {huge} * {index} / {FIXED_POINT_SCALE}"
        );
        assert_eq!(result.unwrap(), huge * index_multiple);
    }

    // And the failure, when it comes, must be an error rather than a wrap.
    let overflow = mul_div_floor(u128::MAX, 2, 1);
    assert!(
        overflow.is_err(),
        "a u128 overflow wrapped instead of erroring"
    );
}

// ---------------------------------------------------------------------------
// share maths — the 0..50 grid, directly
// ---------------------------------------------------------------------------

/// Converting liquidity to shares and back must never return more than went in.
///
/// The user is owed the round trip, so both directions must round against them.
#[test]
fn share_round_trip_never_favours_the_user() {
    let mut checks = 0;

    // Ratios where a floor and a ceil disagree: prime-ish pools against small
    // share supplies.
    for pool in [1u64, 2, 3, 7, 10, 33, 50, 1_000, 999_983] {
        for shares in [1u64, 2, 3, 7, 10, 33, 50, 1_000] {
            let reserve = reserve_with(pool, shares, 0, FIXED_POINT_SCALE);

            for amount in 0..=50u64 {
                let minted = reserve.liquidity_to_shares(amount, Rounding::Down).unwrap();
                let returned = reserve.shares_to_liquidity(minted, Rounding::Down).unwrap();

                assert!(
                    returned <= amount,
                    "pool={pool} shares={shares} amount={amount}: deposited {amount}, \
                     redeemed {returned} — value created by rounding"
                );
                checks += 1;
            }
        }
    }

    println!("share round-trip assertions: {checks}");
    assert!(checks >= 3_000);
}

/// Redeeming shares must never pay out more than the pool holds.
#[test]
fn shares_never_redeem_for_more_than_the_pool() {
    for pool in [1u64, 2, 7, 50, 1_000, 1_000_000] {
        for shares in [1u64, 2, 7, 50, 1_000] {
            let reserve = reserve_with(pool, shares, 0, FIXED_POINT_SCALE);
            let all = reserve.shares_to_liquidity(shares, Rounding::Down).unwrap();
            assert!(
                all <= pool,
                "pool={pool} shares={shares}: redeeming everything pays {all} from a pool of {pool}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// the borrow index
// ---------------------------------------------------------------------------

/// Accrual only ever raises the index, and never by wrapping.
///
/// Invariant 5. A saturating or wrapping index would silently forgive debt.
#[test]
fn the_index_only_rises() {
    let mut checks = 0;

    for borrowed in [0u128, 1, 1_000, 1_000_000_000_000_000] {
        for elapsed in [0u64, 1, 2, 1_000, 67_609_680, 676_096_800] {
            let mut reserve = reserve_with(
                1_000_000_000_000,
                1_000_000_000_000,
                borrowed,
                FIXED_POINT_SCALE,
            );
            let before = reserve.borrow_index;

            reserve
                .accrue_interest(elapsed)
                .expect("accrual must not error");

            assert!(
                reserve.borrow_index >= before,
                "borrowed={borrowed} elapsed={elapsed}: index fell from {before} to {}",
                reserve.borrow_index
            );

            // With nothing borrowed, or no time passed, it must not move at all.
            if borrowed == 0 || elapsed == 0 {
                assert_eq!(
                    reserve.borrow_index, before,
                    "borrowed={borrowed} elapsed={elapsed}: the index moved with nothing to accrue"
                );
            }
            checks += 1;
        }
    }

    println!("index assertions: {checks}");
}

/// Accruing in one step must not pay less than accruing in two.
///
/// A borrower who splits their accrual should not be able to pay less interest,
/// and a supplier should not earn less because a crank ran twice.
#[test]
fn splitting_accrual_does_not_forgive_interest() {
    for total in [2u64, 10, 1_000, 1_000_000] {
        let borrowed = 1_000_000_000_000u128;

        let mut once = reserve_with(
            1_000_000_000_000,
            1_000_000_000_000,
            borrowed,
            FIXED_POINT_SCALE,
        );
        once.accrue_interest(total).unwrap();

        let mut twice = reserve_with(
            1_000_000_000_000,
            1_000_000_000_000,
            borrowed,
            FIXED_POINT_SCALE,
        );
        twice.accrue_interest(total / 2).unwrap();
        twice.accrue_interest(total).unwrap();

        // Compounding across two accruals can only ever charge more, never less.
        assert!(
            twice.borrow_index >= once.borrow_index,
            "elapsed={total}: splitting accrual forgave interest, {} < {}",
            twice.borrow_index,
            once.borrow_index
        );
    }
}

// ---------------------------------------------------------------------------
// the reserve factor
// ---------------------------------------------------------------------------

/// The protocol's cut is taken once, floored, and never exceeds the interest.
#[test]
fn the_reserve_factor_is_taken_once_and_floors() {
    for elapsed in [1u64, 1_000, 67_609_680] {
        let borrowed = 1_000_000_000_000u128;
        let mut reserve = reserve_with(
            1_000_000_000_000,
            1_000_000_000_000,
            borrowed,
            FIXED_POINT_SCALE,
        );

        let debt_before = reserve.current_borrowed_amount().unwrap();
        reserve.accrue_interest(elapsed).unwrap();
        let debt_after = reserve.current_borrowed_amount().unwrap();

        let interest = debt_after - debt_before;
        let expected_cut = mul_div_floor(
            interest as u128,
            reserve.config.reserve_factor_bps as u128,
            BPS_DENOMINATOR,
        )
        .unwrap() as u64;

        assert_eq!(
            reserve.accrued_fees, expected_cut,
            "elapsed={elapsed}: fees {} but interest {interest} implies {expected_cut}",
            reserve.accrued_fees
        );
        assert!(
            reserve.accrued_fees <= interest,
            "elapsed={elapsed}: the protocol took more than the whole interest"
        );
    }
}

/// Fees are carved out of the pool, so they never lift the supplier rate.
#[test]
fn accrued_fees_do_not_inflate_the_share_rate() {
    let borrowed = 1_000_000_000_000u128;
    let mut reserve = reserve_with(
        1_000_000_000_000,
        1_000_000_000_000,
        borrowed,
        FIXED_POINT_SCALE,
    );

    let before = reserve.total_liquidity().unwrap();
    reserve.accrue_interest(67_609_680).unwrap();
    let after = reserve.total_liquidity().unwrap();

    let gross_growth =
        reserve.current_borrowed_amount().unwrap() as u128 + reserve.available_liquidity as u128;
    assert!(
        after < gross_growth,
        "total_liquidity {after} was not reduced by the {} of accrued fees",
        reserve.accrued_fees
    );
    assert!(
        after > before,
        "suppliers earned nothing from a year of interest"
    );
}
