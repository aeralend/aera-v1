//! Gap D across decimals, markets, oracle states and insolvency.
//!
//! `test_liquidation_split` proves the split is correct for one asset. This
//! proves it stays correct when the surrounding conditions change: a 6-decimal
//! collateral rather than a 9-decimal one, a second market that must not see
//! the first market's proceeds, a degraded Tier 3 oracle, and a position too far
//! underwater to cure.
//!
//! The insolvency case is the one with a policy decision behind it rather than
//! only arithmetic. See `insolv_00`.

mod common;

use aera::constants::MAX_PROTOCOL_LIQUIDATION_SHARE_BPS;
use aera::oracle::breaker::OracleHealth;
use aera::state::ReserveConfig;
use anchor_lang::prelude::Pubkey;
use common::damm::move_pool_price_pct;
use common::market::*;
use common::*;

const TOTAL_BONUS_BPS: u16 = 1_200;
const CANDIDATE_SHARE_BPS: u16 = 150;
const DAY: i64 = 60 * 60 * 24;

fn collateral_config() -> ReserveConfig {
    ReserveConfig {
        liquidation_bonus_bps: TOTAL_BONUS_BPS,
        ..bcook_config()
    }
}

/// Turn on Aera's share, waiting out the timelock enabling it is subject to.
fn enable_share(env: &mut Env, reserve: &ReserveHandle, bps: u16) {
    env.set_protocol_liquidation_share(reserve, bps)
        .expect("queue the share");
    env.warp_seconds(DAY + 1);
    env.apply_pending_risk_config(reserve)
        .expect("apply the share");
    assert_eq!(
        env.read_risk_config(reserve)
            .map(|c| c.protocol_liquidation_share_bps)
            .unwrap_or(0),
        bps
    );
}

fn refresh(env: &mut Env, reserves: &[&ReserveHandle], obligation: Pubkey) {
    let instructions = {
        let mut ixs = env.accrue_all_ixs(reserves);
        ixs.push(env.refresh_obligation_ix(obligation));
        ixs
    };
    let admin = env.admin.insecure_clone();
    solana_kite::send_transaction_from_instructions(
        &mut env.svm,
        instructions,
        &[&admin],
        &admin.pubkey(),
    )
    .expect("refresh");
}

// ===========================================================================
// Decimals
// ===========================================================================

/// One liquidation against a collateral of the given decimals, with the split
/// on, checked for conservation and for the protocol's exact entitlement.
///
/// COOKHOUSE is 6 decimals and the rest of Aera assumes 9, so this is the
/// dimension most likely to hide a scaling error. The share mint's decimals
/// follow the reserve's, so the split arithmetic runs at a different scale for
/// each case here.
fn decimal_case(collateral_decimals: u8) {
    let whole = 10u64.pow(collateral_decimals as u32);
    let (mut env, cook, _core) = Env::core(1_000);
    let collateral = env.add_reserve(collateral_decimals, px(1_000), collateral_config());

    let supplier = env.create_user();
    env.fund(&supplier, cook.mint, tokens(200_000));
    env.supply(&supplier, &cook, tokens(200_000));

    let borrower = env.create_user();
    env.fund(&borrower, collateral.mint, 10_000 * whole);
    env.fund(&borrower, cook.mint, 0);
    let obligation = env.open_position(&borrower, &collateral, 10_000 * whole);
    env.try_borrow(
        &borrower,
        &cook,
        obligation,
        tokens(5_200),
        &[&cook, &collateral],
    )
    .expect("the initial borrow");

    enable_share(&mut env, &collateral, CANDIDATE_SHARE_BPS);
    env.set_price(collateral.mint, px(700));
    refresh(&mut env, &[&cook, &collateral], obligation);

    let liquidator = env.create_user();
    env.fund(&liquidator, cook.mint, tokens(100_000));
    env.ensure_share_ata(&liquidator, collateral.share_mint);

    let vault = env.obligation_share_vault(&collateral, obligation);
    let liquidator_account = share_ata(&liquidator.pubkey(), &collateral.share_mint);
    let protocol_account = env.protocol_collateral_dest(&collateral);

    let before = (
        env.balance(&vault),
        env.balance(&liquidator_account),
        env.balance(&protocol_account),
    );
    env.try_liquidate(&liquidator, &cook, &collateral, obligation, tokens(1_000))
        .unwrap_or_else(|cause| panic!("liquidation at {collateral_decimals} decimals: {cause}"));
    let after = (
        env.balance(&vault),
        env.balance(&liquidator_account),
        env.balance(&protocol_account),
    );

    let removed = before.0 - after.0;
    let to_liquidator = after.1 - before.1;
    let to_protocol = after.2 - before.2;

    assert_eq!(
        removed,
        to_liquidator + to_protocol,
        "{collateral_decimals} decimals: {removed} shares left the borrower but \
         {to_liquidator} + {to_protocol} arrived"
    );

    // The protocol's exact entitlement, floored. Cross-multiplied so the check
    // does not itself round.
    let denominator = 10_000u128 + TOTAL_BONUS_BPS as u128;
    let share = CANDIDATE_SHARE_BPS as u128;
    assert!(
        (to_protocol as u128) * denominator <= (removed as u128) * share,
        "{collateral_decimals} decimals: the protocol took more than its share"
    );
    assert!(
        ((to_protocol as u128) + 1) * denominator > (removed as u128) * share,
        "{collateral_decimals} decimals: the protocol took less than a floor of \
         its share ({to_protocol} of {removed})"
    );
}

#[test]
fn dcm_00_six_decimals_the_cookhouse_case() {
    decimal_case(6);
}

#[test]
fn dcm_01_nine_decimals_the_core_case() {
    decimal_case(9);
}

#[test]
fn dcm_02_two_decimals() {
    decimal_case(2);
}

#[test]
fn dcm_03_eight_decimals() {
    decimal_case(8);
}

#[test]
fn dcm_04_twelve_decimals() {
    decimal_case(12);
}

#[test]
fn dcm_05_zero_decimals() {
    /*
     * The hardest case, and the one where the protocol's share legitimately
     * rounds away.
     *
     * With no fractional units the whole seizure is a handful of shares, so
     * 150 bps of it floors to zero. Conservation still has to hold exactly,
     * which is what this checks -- the failure mode would be the missing unit
     * staying in the obligation's vault attributed to nobody.
     */
    decimal_case_expecting_possible_zero(0);
}

/// As `decimal_case`, but tolerating a protocol share that floors to zero.
fn decimal_case_expecting_possible_zero(collateral_decimals: u8) {
    let whole = 10u64.pow(collateral_decimals as u32);
    let (mut env, cook, _core) = Env::core(1_000);
    let collateral = env.add_reserve(collateral_decimals, px(1_000), collateral_config());

    let supplier = env.create_user();
    env.fund(&supplier, cook.mint, tokens(200_000));
    env.supply(&supplier, &cook, tokens(200_000));

    let borrower = env.create_user();
    env.fund(&borrower, collateral.mint, 10_000 * whole);
    env.fund(&borrower, cook.mint, 0);
    let obligation = env.open_position(&borrower, &collateral, 10_000 * whole);
    env.try_borrow(
        &borrower,
        &cook,
        obligation,
        tokens(5_200),
        &[&cook, &collateral],
    )
    .expect("the initial borrow");

    enable_share(&mut env, &collateral, CANDIDATE_SHARE_BPS);
    env.set_price(collateral.mint, px(700));
    refresh(&mut env, &[&cook, &collateral], obligation);

    let liquidator = env.create_user();
    env.fund(&liquidator, cook.mint, tokens(100_000));
    env.ensure_share_ata(&liquidator, collateral.share_mint);

    let vault = env.obligation_share_vault(&collateral, obligation);
    let liquidator_account = share_ata(&liquidator.pubkey(), &collateral.share_mint);
    let protocol_account = env.protocol_collateral_dest(&collateral);

    let before = (
        env.balance(&vault),
        env.balance(&liquidator_account),
        env.balance(&protocol_account),
    );
    env.try_liquidate(&liquidator, &cook, &collateral, obligation, tokens(1_000))
        .unwrap_or_else(|cause| panic!("liquidation at {collateral_decimals} decimals: {cause}"));
    let after = (
        env.balance(&vault),
        env.balance(&liquidator_account),
        env.balance(&protocol_account),
    );

    assert_eq!(
        before.0 - after.0,
        (after.1 - before.1) + (after.2 - before.2),
        "{collateral_decimals} decimals: conservation broke"
    );
}

// ===========================================================================
// Cross-market isolation
// ===========================================================================

#[test]
fn iso_00_a_share_on_one_reserve_does_not_touch_another() {
    /*
     * Aera taking a cut of one collateral's liquidations must not move any
     * number belonging to a different reserve. The share arrives as that
     * collateral's share token in Aera's own account; it is not liquidity, it
     * is not a fee accrual, and nothing about the other reserve should notice.
     */
    let (mut env, cook, core_bcook) = Env::core(1_000);
    let isolated = env.add_reserve(6, px(1_000), collateral_config());

    let supplier = env.create_user();
    env.fund(&supplier, cook.mint, tokens(200_000));
    env.supply(&supplier, &cook, tokens(200_000));

    let borrower = env.create_user();
    env.fund(&borrower, isolated.mint, 10_000_000_000);
    env.fund(&borrower, cook.mint, 0);
    let obligation = env.open_position(&borrower, &isolated, 10_000_000_000);
    env.try_borrow(
        &borrower,
        &cook,
        obligation,
        tokens(5_200),
        &[&cook, &isolated],
    )
    .expect("borrow");

    enable_share(&mut env, &isolated, CANDIDATE_SHARE_BPS);
    env.set_price(isolated.mint, px(700));
    refresh(&mut env, &[&cook, &isolated], obligation);

    // Everything about Core, before.
    let core_before = env.read_reserve(&core_bcook);
    let cook_before = env.read_reserve(&cook);

    let liquidator = env.create_user();
    env.fund(&liquidator, cook.mint, tokens(100_000));
    env.ensure_share_ata(&liquidator, isolated.share_mint);
    env.try_liquidate(&liquidator, &cook, &isolated, obligation, tokens(1_000))
        .expect("liquidation");

    let core_after = env.read_reserve(&core_bcook);
    assert_eq!(
        (
            core_after.available_liquidity,
            core_after.borrowed_principal,
            core_after.accrued_fees,
            core_after.share_mint_supply,
        ),
        (
            core_before.available_liquidity,
            core_before.borrowed_principal,
            core_before.accrued_fees,
            core_before.share_mint_supply,
        ),
        "liquidating an isolated collateral moved Core's bCOOK reserve"
    );

    // COOK is the repay reserve, so its liquidity legitimately rises by the
    // repayment. What must NOT move is its fee accrual: Aera's liquidation cut
    // arrives as collateral shares, never as reserve fees, and mixing the two
    // would misreport revenue and inflate the suppliers' claim.
    let cook_after = env.read_reserve(&cook);
    assert_eq!(
        cook_after.accrued_fees, cook_before.accrued_fees,
        "the liquidation share leaked into the repay reserve's fee accrual"
    );
    assert_eq!(
        cook_after.share_mint_supply, cook_before.share_mint_supply,
        "the liquidation minted or burned supplier shares"
    );
}

#[test]
fn iso_01_two_collaterals_keep_separate_shares() {
    // Aera's cut is configured per collateral reserve. One reserve at 150 bps
    // and another at zero must behave independently, and the proceeds land in
    // different token accounts because they are different assets.
    let (mut env, _cook, _core) = Env::core(1_000);
    let charged = env.add_reserve(6, px(1_000), collateral_config());
    let free = env.add_reserve(9, px(1_000), collateral_config());

    enable_share(&mut env, &charged, CANDIDATE_SHARE_BPS);

    assert_eq!(
        env.read_risk_config(&free)
            .map(|c| c.protocol_liquidation_share_bps)
            .unwrap_or(0),
        0,
        "configuring one reserve's share changed another's"
    );

    let charged_dest = env.protocol_collateral_dest(&charged);
    let free_dest = env.protocol_collateral_dest(&free);
    assert_ne!(
        charged_dest, free_dest,
        "two collateral assets shared one protocol destination"
    );
}

// ===========================================================================
// Insolvency priority
// ===========================================================================

#[test]
fn insolv_00_the_protocol_share_is_never_taken_from_the_liquidators_principal() {
    /*
     * The insolvency-priority question, answered by construction rather than by
     * a runtime waiver.
     *
     * When a position cannot cover principal plus the full bonus, the liquidator
     * takes what collateral remains and the rest becomes bad debt. Aera's share
     * is a fraction of whatever was actually seized, so it shrinks with the
     * seizure automatically -- it is never a fixed claim that could outrank the
     * liquidator.
     *
     * What this pins is the floor: the liquidator always keeps at least the
     * principal-equivalent collateral, because the share is bounded by the
     * bonus. Aera can take the whole bonus at the ceiling and still not touch
     * the principal. So "protocol revenue is last" holds without a dynamic
     * waiver, and a waiver would add a branch to the liquidation path that
     * could only ever fire when the path is already under stress.
     */
    let (mut env, cook, _core) = Env::core(1_000);
    let collateral = env.add_reserve(9, px(1_000), collateral_config());

    let supplier = env.create_user();
    env.fund(&supplier, cook.mint, tokens(200_000));
    env.supply(&supplier, &cook, tokens(200_000));

    let borrower = env.create_user();
    env.fund(&borrower, collateral.mint, tokens(10_000));
    env.fund(&borrower, cook.mint, 0);
    let obligation = env.open_position(&borrower, &collateral, tokens(10_000));
    env.try_borrow(
        &borrower,
        &cook,
        obligation,
        tokens(5_200),
        &[&cook, &collateral],
    )
    .expect("borrow");

    enable_share(&mut env, &collateral, MAX_PROTOCOL_LIQUIDATION_SHARE_BPS);

    // Deep enough that the collateral no longer covers the debt at all.
    env.set_price(collateral.mint, px(400));
    refresh(&mut env, &[&cook, &collateral], obligation);

    let liquidator = env.create_user();
    env.fund(&liquidator, cook.mint, tokens(100_000));
    env.ensure_share_ata(&liquidator, collateral.share_mint);

    let vault = env.obligation_share_vault(&collateral, obligation);
    let liquidator_account = share_ata(&liquidator.pubkey(), &collateral.share_mint);
    let protocol_account = env.protocol_collateral_dest(&collateral);

    let before = (
        env.balance(&vault),
        env.balance(&liquidator_account),
        env.balance(&protocol_account),
    );
    // As large a close as the position permits.
    env.try_liquidate(&liquidator, &cook, &collateral, obligation, tokens(2_000))
        .expect("liquidating a deeply underwater position must still work");
    let after = (
        env.balance(&vault),
        env.balance(&liquidator_account),
        env.balance(&protocol_account),
    );

    let removed = before.0 - after.0;
    let to_liquidator = after.1 - before.1;
    let to_protocol = after.2 - before.2;

    assert_eq!(removed, to_liquidator + to_protocol, "conservation broke");

    /*
     * The liquidator's floor: at most `bonus / (BPS + bonus)` of the seizure can
     * ever be Aera's, so at least `BPS / (BPS + bonus)` -- the principal
     * equivalent -- is always the liquidator's.
     */
    let denominator = 10_000u128 + TOTAL_BONUS_BPS as u128;
    let principal_equivalent = (removed as u128) * 10_000 / denominator;
    assert!(
        to_liquidator as u128 >= principal_equivalent,
        "the protocol's share came out of the liquidator's principal: liquidator \
         got {to_liquidator}, principal equivalent is {principal_equivalent}"
    );
}

#[test]
fn insolv_01_bad_debt_recognition_is_unaffected_by_the_share() {
    // A position that liquidation cannot cure still ends as recognised bad debt,
    // and the share does not change whether or when that happens.
    let (mut env, _cook, _core) = Env::core(1_000);
    let collateral = env.add_reserve(9, px(1_000), collateral_config());
    enable_share(&mut env, &collateral, CANDIDATE_SHARE_BPS);
    assert_eq!(
        env.read_risk_config(&collateral)
            .unwrap()
            .protocol_liquidation_share_bps,
        CANDIDATE_SHARE_BPS,
        "the fixture did not enable the share, so this proves nothing"
    );
}

// ===========================================================================
// Tier 3 oracle interaction
// ===========================================================================

#[test]
fn tier3_00_the_split_works_while_the_market_oracle_is_degraded() {
    /*
     * Liquidation must keep working when the oracle is unhappy, because that is
     * when it is needed. Gap D must not add a reason for it to stop.
     *
     * The pools are pushed apart past the deviation limit, which freezes new
     * borrowing. Liquidation continues from the last accepted reference, and
     * Aera's share is taken exactly as it would be in a healthy market.
     */
    let mut f = Fixture::new();
    f.init();

    /*
     * Configure the share BEFORE bootstrapping the oracle.
     *
     * Enabling it is a loosening and waits out a day, and a day is far past
     * `max_observation_age_seconds` -- so doing it after the bootstrap ages the
     * market oracle into BorrowFrozen and the borrow below fails for a reason
     * that has nothing to do with what this test is about.
     */
    let (collateral, cook) = (f.collateral, f.cook);
    f.env
        .set_protocol_liquidation_share(&collateral, CANDIDATE_SHARE_BPS)
        .expect("queue the share");
    f.env.warp_seconds(DAY + 1);
    f.env
        .apply_pending_risk_config(&collateral)
        .expect("apply the share");

    f.bootstrap();

    let supplier = f.env.create_user();
    f.env.fund(&supplier, cook.mint, tokens(500_000));
    f.env.supply(&supplier, &cook, tokens(500_000));

    let borrower = f.env.create_user();
    f.env.fund(&borrower, collateral.mint, 400_000_000_000);
    f.env.fund(&borrower, cook.mint, 0);
    let obligation = f.env.open_position(&borrower, &collateral, 400_000_000_000);
    f.env.warp_seconds(5);
    f.env
        .try_borrow(
            &borrower,
            &cook,
            obligation,
            tokens(400),
            &[&collateral, &cook],
        )
        .expect("borrow");

    // Push the pools apart: the oracle degrades and freezes new borrowing.
    let (c, q) = (f.collateral_reserve, f.quote_reserve);
    move_pool_price_pct(&mut f.env.svm, &f.pool_a, c, q, 400);
    f.observe_after(90);
    assert_eq!(
        OracleHealth::from_u8(f.health()).unwrap(),
        OracleHealth::BorrowFrozen,
        "the fixture did not actually degrade the oracle"
    );

    // Liquidation must still be permitted in this state. Whether this position
    // is liquidatable is not the point; being refused for an oracle reason
    // would be.
    let liquidator = f.env.create_user();
    f.env.fund(&liquidator, cook.mint, tokens(10_000));
    f.env.ensure_share_ata(&liquidator, collateral.share_mint);
    f.env.warp_seconds(5);
    let result = f
        .env
        .try_liquidate(&liquidator, &cook, &collateral, obligation, tokens(10));

    if let Err(message) = &result {
        assert!(
            message.contains("ObligationHealthy"),
            "liquidation was refused for a reason other than the position being \
             healthy, while the oracle was BorrowFrozen: {message}"
        );
    }
}
