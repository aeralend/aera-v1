//! Interest accrual, the rate curve, and the 10/5 fee split.

mod common;

use aera::constants::*;
use aera::state::ReserveConfig;
use common::*;

/// Put the COOK reserve at exactly the 60% kink: 1,000 supplied, 600 borrowed.
/// At the kink the borrow rate is exactly `optimal_borrow_rate_bps` (10%), so a
/// year of accrual is a number the test can state in closed form.
fn at_the_kink() -> (Env, ReserveHandle, ReserveHandle) {
    let (mut env, cook, bcook) = Env::core(1_000);

    let supplier = env.create_user();
    env.fund(&supplier, cook.mint, tokens(1_000));
    env.supply(&supplier, &cook, tokens(1_000));

    let borrower = env.create_user();
    env.fund(&borrower, bcook.mint, tokens(2_000));
    env.fund(&borrower, cook.mint, 0);
    let obligation = env.open_position(&borrower, &bcook, tokens(2_000));

    /*
     * No origination fee, so utilization lands exactly on the kink.
     *
     * Core charges 15 bps on a draw, which leaves the fee in the vault as
     * revenue rather than paying it out — so borrowing 600 against 1,000
     * supplied gives 59.94% utilization, not 60%. These tests are about the
     * interest curve, and a curve measured at 5,994 bps is measuring the fee.
     *
     * `origination_fee_is_fifteen_bps_by_default` covers the fee itself.
     */
    env.try_set_params(
        &cook,
        ReserveConfig {
            origination_fee_bps: 0,
            ..env.read_reserve(&cook).config
        },
    )
    .unwrap();

    env.try_borrow(&borrower, &cook, obligation, tokens(600), &[&cook, &bcook])
        .unwrap();

    (env, cook, bcook)
}

#[test]
fn rate_curve_matches_the_spec() {
    let (env, cook, _) = at_the_kink();
    let reserve = env.read_reserve(&cook);

    assert_eq!(reserve.utilization_bps().unwrap(), 6_000, "u = 600/1000");
    // At the kink: base 2% + slope1 8% = 10%.
    assert_eq!(reserve.borrow_rate_bps().unwrap(), 1_000);
    // r_supply = r_borrow * u * (1 - 0.15) = 0.10 * 0.60 * 0.85 = 5.1%
    assert_eq!(reserve.supply_rate_bps().unwrap(), 510);
}

/// The whole curve, checked at the points the spec names.
#[test]
fn rate_curve_endpoints() {
    let (mut env, cook, bcook) = Env::core(1_000);

    /*
     * No origination fee: the utilisation points below are exact fractions of
     * what was supplied, and a 15 bps fee leaves 0.15% of each draw in the
     * vault, shifting every one of them. The fee has its own tests.
     */
    env.try_set_params(
        &cook,
        ReserveConfig {
            origination_fee_bps: 0,
            ..env.read_reserve(&cook).config
        },
    )
    .unwrap();

    // Nothing borrowed: the base rate.
    let supplier = env.create_user();
    env.fund(&supplier, cook.mint, tokens(1_000));
    env.supply(&supplier, &cook, tokens(1_000));
    assert_eq!(env.read_reserve(&cook).borrow_rate_bps().unwrap(), 200);

    // 30% utilisation: halfway up slope1 -> 2% + 8%*(0.30/0.60) = 6%.
    let borrower = env.create_user();
    env.fund(&borrower, bcook.mint, tokens(5_000));
    env.fund(&borrower, cook.mint, 0);
    let obligation = env.open_position(&borrower, &bcook, tokens(5_000));
    env.try_borrow(&borrower, &cook, obligation, tokens(300), &[&cook, &bcook])
        .unwrap();
    assert_eq!(env.read_reserve(&cook).borrow_rate_bps().unwrap(), 600);

    // 80% utilisation: halfway up slope2 -> 10% + 80%*((0.80-0.60)/0.40) = 50%.
    env.try_borrow(&borrower, &cook, obligation, tokens(500), &[&cook, &bcook])
        .unwrap();
    assert_eq!(env.read_reserve(&cook).utilization_bps().unwrap(), 8_000);
    assert_eq!(env.read_reserve(&cook).borrow_rate_bps().unwrap(), 5_000);
}

/// A year at the kink adds ~10% to the debt, and the protocol keeps 15% of it —
/// all of which accrues to the single fee destination.
#[test]
fn a_year_of_interest_accrues_fifteen_percent() {
    let (mut env, cook, _) = at_the_kink();

    let before = env.read_reserve(&cook).current_borrowed_amount().unwrap();
    assert_eq!(before, tokens(600));

    env.warp_slots(DEFAULT_SLOTS_PER_YEAR);
    env.accrue(&cook);

    let reserve = env.read_reserve(&cook);
    let after = reserve.current_borrowed_amount().unwrap();
    let interest = after - before;

    // 10% of 600 COOK, allowing for per-slot flooring.
    let expected = tokens(60);
    let drift = expected.abs_diff(interest);
    assert!(
        drift < tokens(1),
        "expected ~{expected} interest, got {interest}"
    );

    // The cut is exact regardless of the rounding above.
    assert_eq!(
        reserve.accrued_fees,
        interest * DEFAULT_RESERVE_FACTOR_BPS as u64 / 10_000,
        "the protocol keeps the whole 15% reserve factor"
    );
}

/// The other 85% is not paid out anywhere — it lifts what a supplier's aCOOK
/// redeems for. That is the only place supplier yield comes from.
#[test]
fn suppliers_earn_through_the_exchange_rate() {
    let (mut env, cook, _) = at_the_kink();

    let before = env.read_reserve(&cook);
    let shares = tokens(100);
    let redeems_before = before
        .shares_to_liquidity(shares, aera::math::Rounding::Down)
        .unwrap();
    assert_eq!(redeems_before, tokens(100), "1:1 before any interest");

    env.warp_slots(DEFAULT_SLOTS_PER_YEAR);
    env.accrue(&cook);

    let after = env.read_reserve(&cook);
    let redeems_after = after
        .shares_to_liquidity(shares, aera::math::Rounding::Down)
        .unwrap();
    assert!(
        redeems_after > redeems_before,
        "100 aCOOK should redeem for more than 100 COOK after a year"
    );

    // 85% of 60 COOK of interest spread over 1,000 shares = ~5.1 COOK per 100.
    let gain = redeems_after - redeems_before;
    assert!(
        gain > tokens(5) && gain < tokens(6),
        "expected ~5.1 COOK of gain per 100 aCOOK, got {gain}"
    );
}

/// Fees are paid to the one configured destination and nowhere else.
#[test]
fn collect_fees_pays_the_fee_destination() {
    let (mut env, cook, _) = at_the_kink();

    env.warp_slots(DEFAULT_SLOTS_PER_YEAR);
    env.accrue(&cook);

    let owed = env.read_reserve(&cook).accrued_fees;
    assert!(owed > 0);

    let fee_wallet = env.fee_wallet.insecure_clone();
    let fee_token = env.fund(&fee_wallet, cook.mint, 0);

    env.try_collect_fees(&cook).unwrap();

    assert_eq!(env.balance(&fee_token), owed);
    assert_eq!(env.read_reserve(&cook).accrued_fees, 0);

    // Nothing left to collect. The blockhash is bumped so this is a distinct
    // transaction rather than a replay of the one above.
    env.bump_blockhash();
    assert_error(env.try_collect_fees(&cook), "NothingToCollect");
}

/// The origination fee ships off, so a borrower receives exactly what they
/// asked for.
#[test]
fn origination_fee_is_fifteen_bps_by_default() {
    /*
     * Aera V1 charges 15 bps when liquidity is drawn, and nothing for routine
     * account management.
     *
     * This test asserted the fee shipped OFF, which was the v0.2 policy. The
     * behaviour it pins is what matters and is unchanged: the borrower owes the
     * full requested amount, receives it less the fee, and the fee lands in
     * `accrued_fees` rather than coming out of the pool.
     */
    let (mut env, cook, bcook) = Env::core(1_000);

    let supplier = env.create_user();
    env.fund(&supplier, cook.mint, tokens(1_000));
    env.supply(&supplier, &cook, tokens(1_000));

    let borrower = env.create_user();
    env.fund(&borrower, bcook.mint, tokens(2_000));
    let wallet = env.fund(&borrower, cook.mint, 0);
    let obligation = env.open_position(&borrower, &bcook, tokens(2_000));

    assert_eq!(
        env.read_reserve(&cook).config.origination_fee_bps,
        aera::constants::DEFAULT_ORIGINATION_FEE_BPS,
    );
    assert_eq!(aera::constants::DEFAULT_ORIGINATION_FEE_BPS, 15);

    let before = env.read_reserve(&cook);
    env.try_borrow(&borrower, &cook, obligation, tokens(500), &[&cook, &bcook])
        .unwrap();

    // 500 COOK at 15 bps is 0.75 COOK.
    let fee = tokens(500) * 15 / 10_000;
    assert_eq!(
        env.balance(&wallet),
        tokens(500) - fee,
        "the borrower receives the draw less the fee",
    );

    let after = env.read_reserve(&cook);
    assert_eq!(after.accrued_fees - before.accrued_fees, fee);

    /*
     * The debt is the full amount, not the amount received. Anything else would
     * let a wallet cross its borrow cap by exactly the fee.
     */
    let debt = env.read_obligation(obligation).borrows[0].borrowed_principal;
    assert_eq!(debt, tokens(500) as u128, "debt is the requested amount");

    /*
     * And the suppliers are untouched: available falls by what was paid out,
     * accrued_fees rises by the fee, so the pool the share price is computed
     * from nets to where it was.
     */
    assert_eq!(
        before.available_liquidity - after.available_liquidity,
        tokens(500) - fee,
        "the vault releases only what was paid out; the fee never leaves it",
    );

    /*
     * And the pool the share price is computed from is unchanged.
     *
     * `total_liquidity()` is available + borrowed - fees. Available fell by the
     * payout, borrowed rose by the full draw, and fees rose by the difference —
     * so suppliers' claim is exactly where it was. The fee came from the
     * borrower, not from them.
     */
    assert_eq!(
        after.total_liquidity().unwrap(),
        before.total_liquidity().unwrap(),
        "suppliers' claim on the pool must not move when a borrower pays a fee",
    );
}

/// Turned on, it is deducted from the payout while the borrower still owes the
/// full amount — and suppliers' claim on the pool is untouched, because the fee
/// comes from the borrower.
#[test]
fn origination_fee_is_charged_when_enabled() {
    let (mut env, cook, bcook) = Env::core(1_000);

    // 50 bps is the hard maximum. Raising the fee is a loosening, so it waits.
    let mut config = env.read_reserve(&cook).config;
    config.origination_fee_bps = 50;
    env.try_set_params(&cook, config).unwrap();
    assert_eq!(
        env.read_reserve(&cook).config.origination_fee_bps,
        DEFAULT_ORIGINATION_FEE_BPS,
        "a fee increase must not apply immediately; the launch value stands"
    );
    env.warp_seconds(DEFAULT_PARAM_TIMELOCK_SECONDS);
    env.try_apply_pending(&cook).unwrap();
    assert_eq!(env.read_reserve(&cook).config.origination_fee_bps, 50);

    let supplier = env.create_user();
    env.fund(&supplier, cook.mint, tokens(1_000));
    env.supply(&supplier, &cook, tokens(1_000));
    let pool_before = env.read_reserve(&cook).total_liquidity().unwrap();

    let borrower = env.create_user();
    env.fund(&borrower, bcook.mint, tokens(2_000));
    let wallet = env.fund(&borrower, cook.mint, 0);
    let obligation = env.open_position(&borrower, &bcook, tokens(2_000));

    env.set_price(cook.mint, px(1_000));
    env.set_price(bcook.mint, px(1_000));
    env.try_borrow(&borrower, &cook, obligation, tokens(500), &[&cook, &bcook])
        .unwrap();

    // 0.50% of 500 COOK is 2.5 COOK.
    let fee = tokens(500) * 50 / 10_000;
    assert_eq!(
        env.balance(&wallet),
        tokens(500) - fee,
        "payout is net of fee"
    );

    let reserve = env.read_reserve(&cook);
    assert_eq!(reserve.accrued_fees, fee, "the fee is protocol revenue");
    assert_eq!(
        reserve.current_borrowed_amount().unwrap(),
        tokens(500),
        "the borrower owes the full amount"
    );
    assert_eq!(
        reserve.total_liquidity().unwrap(),
        pool_before,
        "suppliers' claim is unchanged: the fee came from the borrower"
    );
}

/// Interest only accrues against real debt. An idle pool costs nobody anything.
#[test]
fn no_debt_means_no_interest() {
    let (mut env, cook, _) = Env::core(1_000);

    let supplier = env.create_user();
    env.fund(&supplier, cook.mint, tokens(1_000));
    env.supply(&supplier, &cook, tokens(1_000));

    let index_before = env.read_reserve(&cook).borrow_index;
    env.warp_slots(DEFAULT_SLOTS_PER_YEAR);
    env.accrue(&cook);

    let reserve = env.read_reserve(&cook);
    assert_eq!(reserve.borrow_index, index_before, "index must not move");
    assert_eq!(reserve.accrued_fees, 0);
}
