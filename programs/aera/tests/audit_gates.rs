//! STEAL-100..133, 140..153, 160..174 — health, liquidation, oracle, admin.
//!
//! Three families that share a shape: the program is asked to do something the
//! rules forbid, and must refuse for the documented reason rather than an
//! incidental one. A refusal with the wrong error is a finding too - it means
//! the check that fired is not the check we think protects us.
//!
//! The admin family is different in kind. Those tests measure a documented trust
//! assumption rather than proving it absent: a single admin key really can pause
//! and really can point the fee sink anywhere. The tests exist so the blast
//! radius is written down and bounded, not so we can claim it is zero.

mod common;

use aera::constants::*;
use aera::state::ReserveConfig;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use anchor_lang::{InstructionData, ToAccountMetas};
use common::audit::*;
use common::*;
use solana_keypair::Keypair;

fn actor(env: &mut Env, mint: Pubkey, amount: u64) -> Keypair {
    let user = env.create_user();
    env.fund(&user, mint, amount);
    user
}

/// A vault with liquidity and one borrower holding collateral.
fn book() -> (Env, ReserveHandle, ReserveHandle, Keypair, Pubkey) {
    let (mut env, cook, bcook) = Env::core(1_000);

    let supplier = actor(&mut env, cook.mint, tokens(100_000));
    env.supply(&supplier, &cook, tokens(100_000));

    let borrower = actor(&mut env, bcook.mint, tokens(10_000));
    env.fund(&borrower, cook.mint, 0);
    let obligation = env.open_position(&borrower, &bcook, tokens(10_000));

    (env, cook, bcook, borrower, obligation)
}

// ===========================================================================
// Health and borrowing
// ===========================================================================

/// STEAL-100 — borrowing with nothing posted must fail.
#[test]
fn steal_100_borrow_with_no_collateral() {
    let (mut env, cook, bcook) = Env::core(1_000);
    let supplier = actor(&mut env, cook.mint, tokens(10_000));
    env.supply(&supplier, &cook, tokens(10_000));

    let attacker = actor(&mut env, cook.mint, 0);
    let obligation = env.init_obligation(&attacker);

    let before = env.value_of(&attacker.pubkey(), &cook, &bcook, Some(obligation));
    let result = env.try_borrow(&attacker, &cook, obligation, tokens(1), &[&cook, &bcook]);
    assert!(result.is_err(), "borrowed against no collateral");

    let after = env.value_of(&attacker.pubkey(), &cook, &bcook, Some(obligation));
    assert_no_profit(before, after, FIXED_POINT, FIXED_POINT, 0, "STEAL-100");
    assert_solvent(&env, &cook, "STEAL-100");
}

/// STEAL-101/102 — the LTV boundary is exact.
///
/// 10,000 bCOOK at 1.0 is 10,000; after the 5% haircut, 9,500; at 55% LTV the
/// limit is 5,225. One unit past it must fail, and the limit itself must work.
#[test]
fn steal_101_the_ltv_limit_is_exact_to_one_unit() {
    let (mut env, cook, bcook, borrower, obligation) = book();

    let value = tokens(10_000) * (10_000 - u64::from(DEFAULT_COLLATERAL_HAIRCUT_BPS)) / 10_000;
    let limit = value * u64::from(DEFAULT_LTV_BPS) / 10_000;

    // One unit over must be refused.
    let over = env.try_borrow(&borrower, &cook, obligation, limit + 1, &[&cook, &bcook]);
    assert!(
        over.is_err(),
        "borrowed {} — one unit past the {limit} limit",
        limit + 1
    );

    // Exactly the limit must be allowed.
    env.try_borrow(&borrower, &cook, obligation, limit, &[&cook, &bcook])
        .unwrap_or_else(|e| panic!("borrow of exactly {limit} was refused: {e}"));

    assert_solvent(&env, &cook, "STEAL-101");
}

/// STEAL-104 — unlocking collateral that would push health under 1.
#[test]
fn steal_104_unlock_cannot_push_health_below_one() {
    let (mut env, cook, bcook, borrower, obligation) = book();

    env.try_borrow(
        &borrower,
        &cook,
        obligation,
        tokens(5_000),
        &[&cook, &bcook],
    )
    .expect("borrow inside the limit");

    let before = env.value_of(&borrower.pubkey(), &cook, &bcook, Some(obligation));

    // Taking back almost all the collateral would leave the debt unbacked.
    let result = env.try_withdraw_collateral(
        &borrower,
        &bcook,
        obligation,
        tokens(9_000),
        &[&cook, &bcook],
    );
    assert!(
        result.is_err(),
        "unlocked collateral out from under a live debt"
    );

    let after = env.value_of(&borrower.pubkey(), &cook, &bcook, Some(obligation));
    assert_no_profit(before, after, FIXED_POINT, FIXED_POINT, 0, "STEAL-104");
    assert_solvent(&env, &cook, "STEAL-104");
}

/// STEAL-106 — the bCOOK reserve is never borrowable.
#[test]
fn steal_106_bcook_cannot_be_borrowed() {
    let (mut env, cook, bcook, borrower, obligation) = book();

    let result = env.try_borrow(&borrower, &bcook, obligation, tokens(1), &[&cook, &bcook]);
    assert!(result.is_err(), "borrowed from the collateral-only reserve");
    if let Err(message) = result {
        assert!(
            message.contains("BorrowNotEnabled"),
            "bCOOK borrow refused for the wrong reason: {message}"
        );
    }

    // And its index must never have moved.
    let reserve = env.read_reserve(&bcook);
    assert_eq!(
        reserve.borrow_index, FIXED_POINT_SCALE,
        "the bCOOK index moved — something accrued on a reserve that is never borrowed"
    );
    assert_solvent(&env, &bcook, "STEAL-106");
}

// ===========================================================================
// Liquidation
// ===========================================================================

/// STEAL-120 — a healthy position cannot be liquidated.
#[test]
fn steal_120_a_healthy_obligation_cannot_be_liquidated() {
    let (mut env, cook, bcook, borrower, obligation) = book();
    env.try_borrow(
        &borrower,
        &cook,
        obligation,
        tokens(1_000),
        &[&cook, &bcook],
    )
    .expect("small borrow, very healthy");

    let liquidator = actor(&mut env, cook.mint, tokens(10_000));
    env.ensure_share_ata(&liquidator, bcook.share_mint);

    let before = env.value_of(&liquidator.pubkey(), &cook, &bcook, None);
    let result = env.try_liquidate(&liquidator, &cook, &bcook, obligation, tokens(500));
    assert!(result.is_err(), "liquidated a healthy obligation");
    if let Err(message) = result {
        assert!(
            message.contains("ObligationHealthy"),
            "healthy liquidation refused for the wrong reason: {message}"
        );
    }

    let after = env.value_of(&liquidator.pubkey(), &cook, &bcook, None);
    assert_no_profit(before, after, FIXED_POINT, FIXED_POINT, 0, "STEAL-120");
    assert_solvent(&env, &cook, "STEAL-120");
}

/// STEAL-130 — liquidating an obligation with no debt.
#[test]
fn steal_130_liquidating_an_empty_obligation() {
    let (mut env, cook, bcook, _borrower, obligation) = book();
    let liquidator = actor(&mut env, cook.mint, tokens(10_000));
    env.ensure_share_ata(&liquidator, bcook.share_mint);

    let before = env.value_of(&liquidator.pubkey(), &cook, &bcook, None);
    let result = env.try_liquidate(&liquidator, &cook, &bcook, obligation, tokens(100));
    assert!(result.is_err(), "liquidated an obligation with no debt");

    let after = env.value_of(&liquidator.pubkey(), &cook, &bcook, None);
    assert_no_profit(before, after, FIXED_POINT, FIXED_POINT, 0, "STEAL-130");
    assert_solvent(&env, &cook, "STEAL-130");
}

// ===========================================================================
// Oracle
// ===========================================================================

/// STEAL-140 - a caller-supplied source account cannot price the book.
///
/// v0.1 tested that two of five guardians could not reach quorum. There is no
/// quorum in v0.2 and nobody publishes anything, so the property worth testing
/// is the one that replaced it: the rate comes from an account named in
/// configuration, and passing a different one -- however well-formed -- is
/// refused rather than believed.
///
/// This is INVARIANT 9, and it is why the oracle validates an address *and* an
/// owner before it parses a single byte.
#[test]
fn steal_140_a_substituted_source_cannot_price_the_book() {
    let (mut env, cook, bcook) = Env::core(1_000);
    let supplier = actor(&mut env, cook.mint, tokens(10_000));
    env.supply(&supplier, &cook, tokens(10_000));

    // A pool the attacker controls, reporting a rate far above the truth. It is
    // a perfectly valid stake pool -- right length, right discriminant, right
    // mint -- and it is simply not the configured one.
    let forged = Pubkey::new_unique();
    let data = stake_pool_bytes(bcook.mint, px(9_000) as u64, POOL_SHARES, 200, 9);
    env.svm
        .set_account(
            forged,
            solana_account::Account {
                lamports: 1_000_000_000,
                data,
                owner: TEST_STAKE_POOL_PROGRAM,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

    let mut accounts = aera::accounts::RefreshOracle {
        oracle: env.oracle_address(bcook.mint),
    }
    .to_account_metas(None);
    accounts.push(AccountMeta::new_readonly(forged, false));
    // The honest ProgramData, so this test still turns on the substituted
    // *source account* and cannot pass merely because an account was missing.
    accounts.push(AccountMeta::new_readonly(
        env.stake_pool_program_data(),
        false,
    ));

    let admin = env.admin.insecure_clone();
    let result = env.send_raw(
        vec![Instruction {
            program_id: aera::id(),
            accounts,
            data: aera::instruction::RefreshOracle {}.data(),
        }],
        &[&admin],
    );

    assert!(
        result.is_err(),
        "a forged stake pool was accepted as the source"
    );
    if let Err(message) = result {
        assert!(
            message.contains("OracleAccountMismatch"),
            "forged source refused for the wrong reason: {message}"
        );
    }

    // And the oracle still holds the honest rate.
    let oracle = env.read_oracle(bcook.mint);
    assert!(
        oracle.reference.gross_rate < px(2_000) as u128,
        "the forged rate reached the reference: {}",
        oracle.reference.gross_rate
    );
    assert_solvent(&env, &cook, "STEAL-140");
}

/// STEAL-148/149 — repay is never blocked. This is the property that matters.
///
/// A borrower who cannot repay while prices are down can only be liquidated, so
/// repay must survive every state the protocol can enter. In v0.2 those states
/// are the oracle's own: a breaker freeze, a full emergency, and a pause. Each
/// is a different code path and each is tested separately.
#[test]
fn steal_149_repay_survives_frozen_emergency_and_pause() {
    for condition in ["frozen", "emergency", "pause"] {
        let (mut env, cook, bcook, borrower, obligation) = book();
        env.try_borrow(
            &borrower,
            &cook,
            obligation,
            tokens(1_000),
            &[&cook, &bcook],
        )
        .expect("borrow while healthy");

        let debt_before = env
            .value_of(&borrower.pubkey(), &cook, &bcook, Some(obligation))
            .debt;
        assert!(debt_before > 0);

        match condition {
            // A rise past the per-epoch allowance but under the emergency
            // bound: BORROW_FROZEN.
            "frozen" => env.move_rate(bcook.mint, 1_050, 2),
            // A redemption fee past the bound Aera accepts makes the source
            // unreadable: EMERGENCY.
            "emergency" => {
                env.set_pool(bcook.mint, px(1_000) as u64, POOL_SHARES, 9_000, 2);
                env.refresh_oracle(bcook.mint);
            }
            "pause" => env.pause_all(),
            _ => unreachable!(),
        }

        let result = env.try_repay(&borrower, &cook, obligation, tokens(500));
        assert!(
            result.is_ok(),
            "REPAY BLOCKED under {condition} — a borrower can only be liquidated: {result:?}"
        );

        let debt_after = env
            .value_of(&borrower.pubkey(), &cook, &bcook, Some(obligation))
            .debt;
        assert!(
            debt_after < debt_before,
            "repay under {condition} succeeded but the debt did not fall"
        );
        assert_solvent(&env, &cook, &format!("STEAL-149 {condition}"));
    }
}

/// STEAL-152 - COOK is the quote currency and prices at exactly 1.
///
/// It must also carry no redemption fee. COOK denominates debt, so pricing it
/// even slightly below 1 would understate what every borrower owes and make
/// them look healthier than they are -- wrong in the unsafe direction. That is
/// why the quote asset uses the unit-of-account source rather than a stake
/// pool, and why this asserts the effective rate, not just the gross one.
#[test]
fn steal_152_cook_prices_at_exactly_one() {
    let (env, cook, _bcook) = Env::core(1_000);
    let oracle = env.read_oracle(cook.mint);

    assert_eq!(
        oracle.reference.gross_rate, FIXED_POINT_SCALE,
        "COOK was priced at something other than 1"
    );
    assert_eq!(
        oracle.reference.effective_rate, FIXED_POINT_SCALE,
        "the quote asset must not carry a redemption fee"
    );
    assert_eq!(
        oracle.reference.withdrawal_fee_bps, 0,
        "the unit of account has nothing to redeem through"
    );
}

// ===========================================================================
// Admin — measuring a documented trust, not disproving it
// ===========================================================================

/// STEAL-161..165 — the hard maxima hold against the admin.
#[test]
fn steal_161_admin_cannot_cross_the_hard_maxima() {
    let (mut env, cook, _bcook) = Env::core(1_000);
    let base = env.read_reserve(&cook).config;

    let cases: Vec<(&str, ReserveConfig)> = vec![
        (
            "LTV above 75%",
            ReserveConfig {
                loan_to_value_bps: MAX_ADMIN_LTV_BPS + 1,
                liquidation_threshold_bps: 9_999,
                ..base
            },
        ),
        (
            "bonus above 15%",
            ReserveConfig {
                liquidation_bonus_bps: MAX_ADMIN_LIQUIDATION_BONUS_BPS + 1,
                ..base
            },
        ),
        (
            "reserve factor above 30%",
            ReserveConfig {
                reserve_factor_bps: MAX_ADMIN_RESERVE_FACTOR_BPS + 1,
                ..base
            },
        ),
        (
            "origination above 50bps",
            ReserveConfig {
                origination_fee_bps: MAX_ADMIN_ORIGINATION_FEE_BPS + 1,
                ..base
            },
        ),
        (
            "LTV above LT",
            ReserveConfig {
                loan_to_value_bps: 7_000,
                liquidation_threshold_bps: 6_000,
                ..base
            },
        ),
        (
            "borrow cap above supply cap",
            ReserveConfig {
                supply_cap: tokens(1_000),
                borrow_cap: tokens(2_000),
                ..base
            },
        ),
    ];

    for (label, config) in cases {
        let result = env.try_set_params(&cook, config);
        assert!(
            result.is_err(),
            "ADMIN CROSSED A HARD MAXIMUM — {label} was accepted"
        );
    }
}

/// STEAL-166/167 — cutting a cap lands now; raising one waits.
#[test]
fn steal_166_raising_a_cap_waits_and_cutting_one_does_not() {
    let (mut env, cook, _bcook) = Env::core(1_000);
    let base = env.read_reserve(&cook).config;

    // Both caps move together: halving the supply cap alone would leave the
    // borrow cap above it, which `validate` refuses outright. That refusal is
    // itself invariant 12 holding, and is asserted directly in
    // `steal_161_admin_cannot_cross_the_hard_maxima`.
    let cut = ReserveConfig {
        supply_cap: base.supply_cap / 2,
        borrow_cap: base.borrow_cap / 2,
        ..base
    };
    env.try_set_params(&cook, cut)
        .expect("a cap cut must be accepted");
    assert_eq!(
        env.read_reserve(&cook).config.supply_cap,
        base.supply_cap / 2,
        "a cap cut did not land immediately"
    );

    // A raise is a loosening and must queue behind the timelock.
    let raise = ReserveConfig {
        supply_cap: base.supply_cap,
        ..cut
    };
    env.try_set_params(&cook, raise)
        .expect("queuing a raise is allowed");
    assert_eq!(
        env.read_reserve(&cook).config.supply_cap,
        base.supply_cap / 2,
        "A CAP RAISE LANDED INSTANTLY — the timelock does not hold"
    );

    // And it must not apply before the timelock elapses.
    let early = env.try_apply_pending(&cook);
    assert!(early.is_err(), "a queued raise applied before its timelock");

    env.warp_seconds(DEFAULT_PARAM_TIMELOCK_SECONDS + 1);
    env.try_apply_pending(&cook)
        .expect("applies after the timelock");
    assert_eq!(env.read_reserve(&cook).config.supply_cap, base.supply_cap);
}

/// STEAL-160 — a full pause must not trap a borrower.
///
/// Covered by STEAL-149 as well; kept separate because "admin pauses and nobody
/// can repay" is the single worst outcome in the admin family and deserves to
/// fail loudly on its own.
#[test]
fn steal_160_a_paused_protocol_still_lets_borrowers_out() {
    let (mut env, cook, bcook, borrower, obligation) = book();
    env.try_borrow(
        &borrower,
        &cook,
        obligation,
        tokens(1_000),
        &[&cook, &bcook],
    )
    .expect("borrow while healthy");

    env.pause_all();

    /*
     * Repay what they actually hold, which is the draw less the origination
     * fee.
     *
     * Core charges 15 bps on a draw, so a borrower who requests 1,000 receives
     * 998.5 and owes 1,000. Repaying the full 1,000 would fail for want of
     * balance rather than for want of permission, and would test the wrong
     * thing — the property here is that a paused protocol does not trap
     * borrowers, not that they can conjure the fee back.
     */
    let held = env.balance(&ata(&borrower.pubkey(), &cook.mint));
    assert!(held > 0, "the borrower must hold something to repay with");

    assert!(
        env.try_repay(&borrower, &cook, obligation, held).is_ok(),
        "CRITICAL: admin pause traps borrowers — they can only be liquidated"
    );
    assert_solvent(&env, &cook, "STEAL-160");
}
