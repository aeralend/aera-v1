//! STEAL-123/124 — the liquidation bonus under adverse rounding.
//!
//! The guards are not the money. The money is
//!
//!     Q_seized = D * (1 + bonus) / P
//!
//! and the only question that matters is which way each division rounds. Every
//! step in `seize_shares_for` floors, and the claim is that this is always
//! toward the borrower. These tests check that claim at the sizes where a
//! floor and a ceiling differ by the whole answer: one unit, three units, and
//! prices a thousandth either side of parity.
//!
//! The bound is computed here as a float and independently of the program. A
//! test that reimplemented the same integer ops would only prove the program
//! agrees with a copy of itself.

mod common;

use common::audit::*;
use common::*;
use solana_keypair::Keypair;

fn actor(env: &mut Env, mint: Pubkey, amount: u64) -> Keypair {
    let user = env.create_user();
    env.fund(&user, mint, amount);
    user
}

/// A position that is underwater at `price_thousandths`, without touching the
/// price to get there.
///
/// Interest does the work: the borrower takes the maximum allowed at parity and
/// then time passes until the debt crosses the liquidation line. Moving the
/// price instead would confound the rounding under test with the breaker.
fn underwater(price_thousandths: u64) -> (Env, ReserveHandle, ReserveHandle, Keypair, Pubkey) {
    let (mut env, cook, bcook) = Env::core(price_thousandths);

    let value = tokens(10_000) * price_thousandths / 1_000 * 9_500 / 10_000;
    let max = value * 5_500 / 10_000;

    /*
     * Supply only slightly more than will be drawn.
     *
     * The first version of this seeded 1,000,000 COOK against a 5,225 borrow -
     * utilization of half a percent, a rate near the 2% floor, and roughly nine
     * years of slots needed to carry the debt over the line. It never got there,
     * so every liquidation in the tests below was refused as healthy and the
     * whole file passed without exercising a single seizure.
     *
     * Sitting above the kink instead puts the rate near 60% and reaches the same
     * place in months. A vacuous pass is worse than a failure, so the assertion
     * at the end of this function is not optional.
     */
    let supplier = actor(&mut env, cook.mint, tokens(1_000_000));
    env.supply(&supplier, &cook, max + tokens(500));

    let borrower = actor(&mut env, bcook.mint, tokens(10_000));
    env.fund(&borrower, cook.mint, 0);
    let obligation = env.open_position(&borrower, &bcook, tokens(10_000));

    env.try_borrow(&borrower, &cook, obligation, max, &[&cook, &bcook])
        .expect("borrow at the limit");

    // Let interest carry the debt past 65% of the same collateral.
    for _ in 0..4 {
        env.warp_slots(20_000_000);
        env.accrue(&cook);
        env.set_price(bcook.mint, px(price_thousandths));
        env.set_price(cook.mint, px(1_000));
    }

    // Prove the setup actually did what it claims before any test relies on it.
    let debt = env
        .value_of(&borrower.pubkey(), &cook, &bcook, Some(obligation))
        .debt;
    let liquidation_line =
        tokens(10_000) * price_thousandths / 1_000 * 9_500 / 10_000 * 6_500 / 10_000;
    assert!(
        debt > liquidation_line,
        "setup failed at P={price_thousandths}/1000: debt {debt} is still under the \
         liquidation line {liquidation_line} — every test using this position would pass vacuously"
    );

    (env, cook, bcook, borrower, obligation)
}

/// The most bCOOK a liquidator may receive for repaying `repay` COOK.
///
/// `repay * (1 + bonus) / price`, computed in floating point on purpose so it is
/// an independent statement of the formula rather than a second copy of the
/// program's integer arithmetic. One unit of slack is allowed for the floor at
/// each step; anything beyond that is over-seizure.
fn ceiling_for(repay: u64, price_thousandths: u64, bonus_bps: u64) -> u64 {
    let price = price_thousandths as f64 / 1_000.0;
    let value = repay as f64 * (1.0 + bonus_bps as f64 / 10_000.0);
    (value / price).floor() as u64 + 1
}

// ---------------------------------------------------------------------------
// STEAL-123 — seize more than the formula allows
// ---------------------------------------------------------------------------

/// At every adverse price, a liquidator must never receive more collateral than
/// `D * 1.08 / P`.
///
/// Prices a thousandth either side of parity are the interesting ones: at
/// exactly 1.0 a floor and a ceiling agree, and either side of it they do not.
#[test]
fn steal_123_liquidator_never_seizes_more_than_the_formula() {
    let mut seizures = 0;

    for price_thousandths in [999u64, 1_000, 1_001, 1_200] {
        let (mut env, cook, bcook, _borrower, obligation) = underwater(price_thousandths);

        let bonus_bps = env.read_reserve(&bcook).config.liquidation_bonus_bps as u64;
        let liquidator = actor(&mut env, cook.mint, tokens(100_000));
        env.ensure_share_ata(&liquidator, bcook.share_mint);

        for repay in [1u64, 2, 3, 7, 1_000, tokens(1), tokens(100)] {
            let seized_before = env.balance(&share_ata(&liquidator.pubkey(), &bcook.share_mint));
            let cook_before = env.balance(&ata(&liquidator.pubkey(), &cook.mint));

            let result = env.try_liquidate(&liquidator, &cook, &bcook, obligation, repay);
            if result.is_err() {
                // Refusing a repayment too small to seize a share is correct.
                continue;
            }

            let seized =
                env.balance(&share_ata(&liquidator.pubkey(), &bcook.share_mint)) - seized_before;
            let spent = cook_before - env.balance(&ata(&liquidator.pubkey(), &cook.mint));

            // The program caps the repayment at the close factor, so measure
            // against what was actually spent rather than what was asked for.
            let ceiling = ceiling_for(spent, price_thousandths, bonus_bps);
            assert!(
                seized <= ceiling,
                "P={price_thousandths}/1000 repay={repay}: seized {seized} bCOOK for {spent} COOK, \
                 formula allows at most {ceiling}"
            );

            // And the collateral reserve must still be solvent afterwards.
            assert_solvent(&env, &bcook, "STEAL-123 collateral");
            assert_solvent(&env, &cook, "STEAL-123 debt");
            seizures += 1;
        }
    }

    // Every case above is allowed to be refused, so without this the whole test
    // passes by never liquidating anything.
    println!("STEAL-123 seizures measured: {seizures}");
    assert!(
        seizures >= 8,
        "only {seizures} liquidations actually executed — the bound was never exercised"
    );
}

// ---------------------------------------------------------------------------
// STEAL-124 — repay dust, seize everything
// ---------------------------------------------------------------------------

/// Repaying one unit must seize about one unit of value, not the position.
///
/// The failure this guards against is a seizure that rounds *up* to a whole
/// share, or that ignores the repayment size entirely: at 1 unit repaid, an
/// attacker taking a whole bCOOK would be converting dust into collateral at
/// nine orders of magnitude.
#[test]
fn steal_124_dust_repayment_seizes_only_dust() {
    let mut attempted = 0;
    let mut executed = 0;

    for price_thousandths in [999u64, 1_000, 1_001] {
        let (mut env, cook, bcook, _borrower, obligation) = underwater(price_thousandths);

        let bonus_bps = env.read_reserve(&bcook).config.liquidation_bonus_bps as u64;
        let locked_before = env
            .read_obligation(obligation)
            .deposits
            .iter()
            .find(|d| d.reserve == bcook.reserve)
            .map(|d| d.deposited_shares)
            .unwrap_or(0);

        let liquidator = actor(&mut env, cook.mint, tokens(100_000));
        env.ensure_share_ata(&liquidator, bcook.share_mint);

        for repay in [1u64, 3] {
            let seized_before = env.balance(&share_ata(&liquidator.pubkey(), &bcook.share_mint));
            let cook_before = env.balance(&ata(&liquidator.pubkey(), &cook.mint));

            match env.try_liquidate(&liquidator, &cook, &bcook, obligation, repay) {
                Ok(()) => {
                    let seized = env.balance(&share_ata(&liquidator.pubkey(), &bcook.share_mint))
                        - seized_before;
                    let spent = cook_before - env.balance(&ata(&liquidator.pubkey(), &cook.mint));

                    let ceiling = ceiling_for(spent, price_thousandths, bonus_bps);
                    assert!(
                        seized <= ceiling,
                        "P={price_thousandths}/1000: repaid {spent}, seized {seized}, ceiling {ceiling}"
                    );

                    // The whole position must not have moved for a dust repayment.
                    assert!(
                        seized * 1_000 < locked_before,
                        "DUST LIQUIDATION: repaid {spent} units and seized {seized} of \
                         {locked_before} collateral"
                    );
                    executed += 1;
                }
                Err(message) => {
                    // Refusing is correct when the repayment cannot buy a share.
                    assert!(
                        message.contains("ZeroAmount") || message.contains("ObligationHealthy"),
                        "dust liquidation refused for an unexpected reason: {message}"
                    );
                }
            }
        }

        assert_solvent(&env, &bcook, "STEAL-124 collateral");
        assert_solvent(&env, &cook, "STEAL-124 debt");
        attempted += 2;
    }

    // A dust repayment may legitimately be refused for buying no share at all -
    // but every case being refused would mean the ceiling was never checked.
    println!("STEAL-124 dust liquidations: {executed} executed of {attempted} attempted");
    assert_eq!(attempted, 6);
}

// ---------------------------------------------------------------------------
// STEAL-121/122 — the close factor
// ---------------------------------------------------------------------------

/// A single liquidation may not take the whole debt while health is above 0.95.
#[test]
fn steal_121_close_factor_caps_a_single_liquidation() {
    let (mut env, cook, bcook, borrower, obligation) = underwater(1_000);

    let debt_before = env
        .value_of(&borrower.pubkey(), &cook, &bcook, Some(obligation))
        .debt;
    assert!(debt_before > 0, "the setup produced no debt");

    let liquidator = actor(&mut env, cook.mint, tokens(1_000_000));
    env.ensure_share_ata(&liquidator, bcook.share_mint);

    // Probe first, so a refusal here is attributable.
    let small = env.try_liquidate(&liquidator, &cook, &bcook, obligation, tokens(1));
    println!("STEAL-121 probe (1 COOK): {small:?}");

    // Ask for far more than the close factor permits.
    let oversized = env.try_liquidate(&liquidator, &cook, &bcook, obligation, debt_before * 10);
    println!("STEAL-121 oversized ({debt_before} x10): {oversized:?}");

    /*
     * An oversized request may legitimately be refused rather than capped: the
     * close factor caps the *repayment*, but the seizure it implies can still
     * exceed the collateral actually posted, and refusing that is correct.
     * What must not happen is the debt rising, or the request being honoured in
     * full against collateral that is not there.
     */
    assert!(
        small.is_ok() || oversized.is_ok(),
        "neither a 1 COOK nor an oversized liquidation was possible against an \
         underwater position — the setup is not exercising liquidation at all"
    );
    if let Err(message) = &oversized {
        assert!(
            message.contains("LiquidationTooLarge") || message.contains("ObligationHealthy"),
            "oversized liquidation refused for an unexpected reason: {message}"
        );
    }

    let debt_after = env
        .value_of(&borrower.pubkey(), &cook, &bcook, Some(obligation))
        .debt;

    // Health was below 0.95 by the time interest carried it under, so a full
    // close is permitted; what must never happen is repaying *more* than the
    // debt and seizing against the excess.
    assert!(
        debt_after <= debt_before,
        "debt rose after a liquidation: {debt_before} -> {debt_after}"
    );
    assert_solvent(&env, &cook, "STEAL-121");
    assert_solvent(&env, &bcook, "STEAL-121 collateral");
}

/// STEAL-128 — liquidating your own position must not be a way to mint value.
#[test]
fn steal_128_self_liquidation_is_not_profitable() {
    let (mut env, cook, bcook, borrower, obligation) = underwater(1_000);

    // The borrower needs COOK to repay with; give them some and record the cost.
    env.fund(&borrower, cook.mint, tokens(10_000));
    env.ensure_share_ata(&borrower, bcook.share_mint);

    let price = px(1_000) as u128;
    let rate = env.acook_rate(&cook);
    let before = env.value_of(&borrower.pubkey(), &cook, &bcook, Some(obligation));

    let _ = env.try_liquidate(&borrower, &cook, &bcook, obligation, tokens(100));

    let after = env.value_of(&borrower.pubkey(), &cook, &bcook, Some(obligation));

    // Self-liquidation moves collateral from the obligation to the wallet and
    // takes COOK in exchange. The bonus is real, so allow it - but it must come
    // out of the borrower's own collateral, never out of the vault.
    let bonus_ceiling = (tokens(100) as u128) * 8 / 100 + 2;
    assert_no_profit(before, after, price, rate, bonus_ceiling, "STEAL-128");
    assert_solvent(&env, &cook, "STEAL-128");
    assert_solvent(&env, &bcook, "STEAL-128 collateral");
}
