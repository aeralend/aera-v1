//! Randomised liquidation splits against the Gap D invariants.
//!
//! Two layers, because they answer different questions and cost different
//! amounts of time.
//!
//! **The arithmetic layer** runs 100,000+ randomised splits directly against
//! `split_seized_shares`. It is where the seizure sizes, bonuses, shares and
//! rounding boundaries are swept, and it is fast enough to sweep them properly.
//!
//! **The protocol layer** runs full LiteSVM liquidations across decimals, close
//! factors, health factors, oracle states and both markets. It is slower by four
//! orders of magnitude, so it samples rather than sweeps -- but it is the only
//! layer that can catch a bug living in the instruction rather than in the
//! formula.
//!
//! Both report coverage. The market-oracle fuzz shipped a version that passed
//! while checking its most important invariant zero times, because the generator
//! never reached the state that invariant was about. Counters are asserted here
//! for the same reason.

mod common;

use aera::risk::split_seized_shares;
use aera::state::ReserveConfig;
use common::*;

const DAY: i64 = 60 * 60 * 24;

/// Deterministic LCG, so a failure reproduces from its seed alone.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 11
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound.max(1)
    }

    fn pick<T: Copy>(&mut self, options: &[T]) -> T {
        options[self.below(options.len() as u64) as usize]
    }
}

// ===========================================================================
// The arithmetic layer
// ===========================================================================

/// What the sweep actually reached. Asserted, not merely printed.
#[derive(Default, Debug)]
struct ArithCoverage {
    zero_share: usize,
    positive_share: usize,
    protocol_rounded_to_zero: usize,
    protocol_nonzero: usize,
    share_clamped: usize,
    tiny_seizure: usize,
    large_seizure: usize,
    zero_bonus: usize,
}

#[test]
fn fuzz_00_a_hundred_thousand_random_splits() {
    let mut rng = Rng(0xA3EA_D000);
    let mut coverage = ArithCoverage::default();

    for step in 0..100_000u64 {
        /*
         * Seizure sizes spanning single units to the whole u64 range.
         *
         * The small end is where the protocol's share rounds away and where an
         * off-by-one would live; the large end is where an intermediate product
         * would overflow. The market-oracle bug was at the large end and was
         * invisible at the small one.
         */
        let seize = match rng.below(10) {
            0..=2 => rng.below(200),
            3..=5 => rng.below(1_000_000),
            6..=7 => rng.below(u64::MAX / 1_000),
            8 => rng.next().wrapping_mul(rng.next()),
            _ => u64::MAX - rng.below(1_000),
        };
        let bonus = rng.pick(&[0u16, 1, 100, 500, 800, 1_200, 1_500, 9_999, 10_000]);
        let share = rng.pick(&[
            0u16,
            1,
            25,
            50,
            150,
            300,
            800,
            1_200,
            1_500,
            10_000,
            u16::MAX,
        ]);

        let (protocol, liquidator) =
            split_seized_shares(seize, bonus, share).unwrap_or_else(|cause| {
                panic!("step {step}: split({seize}, {bonus}, {share}) errored: {cause:?}")
            });

        let context = format!("step {step}: split({seize}, {bonus}, {share})");

        // --- 1. conservation, exactly --------------------------------------
        assert_eq!(
            protocol.checked_add(liquidator),
            Some(seize),
            "{context}: {protocol} + {liquidator} != {seize}"
        );

        // --- 2. the protocol never exceeds its entitlement -----------------
        let effective = share.min(bonus) as u128;
        let denominator = 10_000u128 + bonus as u128;
        assert!(
            (protocol as u128) * denominator <= (seize as u128) * effective,
            "{context}: protocol {protocol} exceeds its exact entitlement"
        );

        // --- 3. and is never more than one unit short ----------------------
        assert!(
            ((protocol as u128) + 1) * denominator > (seize as u128) * effective,
            "{context}: protocol {protocol} is more than a floor below its \
             entitlement, so the rounding is not a floor"
        );

        // --- 4. the liquidator always keeps the principal equivalent -------
        //
        // The floor that makes "carved from the bonus" true rather than a
        // slogan: whatever the share, the liquidator keeps at least the
        // collateral corresponding to the debt it repaid.
        let principal_equivalent = (seize as u128) * 10_000 / denominator;
        assert!(
            liquidator as u128 >= principal_equivalent,
            "{context}: liquidator {liquidator} fell below the principal \
             equivalent {principal_equivalent}"
        );

        // --- 5. a share above the bonus is clamped, not applied ------------
        if share > bonus {
            assert_eq!(
                (protocol, liquidator),
                split_seized_shares(seize, bonus, bonus).unwrap(),
                "{context}: a share above the bonus was not clamped to it"
            );
            coverage.share_clamped += 1;
        }

        if share == 0 || bonus == 0 {
            assert_eq!(
                (protocol, liquidator),
                (0, seize),
                "{context}: nothing to carve from, yet the protocol took something"
            );
        }

        // --- coverage -------------------------------------------------------
        if share == 0 {
            coverage.zero_share += 1;
        } else {
            coverage.positive_share += 1;
        }
        if bonus == 0 {
            coverage.zero_bonus += 1;
        }
        if protocol == 0 && share > 0 && bonus > 0 {
            coverage.protocol_rounded_to_zero += 1;
        }
        if protocol > 0 {
            coverage.protocol_nonzero += 1;
        }
        if seize < 1_000 {
            coverage.tiny_seizure += 1;
        }
        if seize > u64::MAX / 1_000 {
            coverage.large_seizure += 1;
        }
    }

    eprintln!("arithmetic coverage: {coverage:?}");
    let need = |what: &str, count: usize| {
        assert!(
            count > 0,
            "the sweep never produced {what}, so the invariant about it was \
             never checked. Coverage: {coverage:?}"
        );
    };
    need("a zero share", coverage.zero_share);
    need("a positive share", coverage.positive_share);
    need("a zero bonus", coverage.zero_bonus);
    need("a share clamped to the bonus", coverage.share_clamped);
    need(
        "a protocol amount rounding to zero",
        coverage.protocol_rounded_to_zero,
    );
    need("a nonzero protocol amount", coverage.protocol_nonzero);
    need("a tiny seizure", coverage.tiny_seizure);
    need("a seizure near the u64 ceiling", coverage.large_seizure);
}

#[test]
fn fuzz_01_the_rounding_boundary_is_swept_exhaustively() {
    /*
     * Every seizure from 0 to 40,000 at the COOKHOUSE candidate, plus the exact
     * unit boundaries at several bonus/share pairs.
     *
     * Random sampling reaches a boundary occasionally; this reaches every one of
     * them in the range where the protocol's share is first a fraction of a
     * unit, then one unit, then two.
     */
    for (bonus, share) in [
        (1_200u16, 150u16),
        (1_200, 300),
        (800, 150),
        (1_500, 300),
        (100, 100),
        (1, 1),
    ] {
        let denominator = 10_000u128 + bonus as u128;
        let mut previous = 0u64;
        for seize in 0u64..=40_000 {
            let (protocol, liquidator) = split_seized_shares(seize, bonus, share).unwrap();
            assert_eq!(protocol + liquidator, seize);
            assert!((protocol as u128) * denominator <= (seize as u128) * share as u128);
            assert!(((protocol as u128) + 1) * denominator > (seize as u128) * share as u128);
            // One more unit of seizure can add at most one unit of protocol
            // share; a jump would mean the floor is not a floor.
            assert!(
                protocol >= previous && protocol <= previous + 1,
                "bonus {bonus}, share {share}: seizure {seize} moved the protocol \
                 amount from {previous} to {protocol}"
            );
            previous = protocol;
        }
    }
}

// ===========================================================================
// The protocol layer
// ===========================================================================

#[derive(Default, Debug)]
struct ProtocolCoverage {
    zero_share: usize,
    positive_share: usize,
    protocol_rounded_to_zero: usize,
    protocol_nonzero: usize,
    tiny_liquidation: usize,
    normal_close_factor: usize,
    full_close_factor: usize,
    six_decimals: usize,
    nine_decimals: usize,
    other_decimals: usize,
    healthy_position: usize,
    deeply_underwater: usize,
    liquidation_refused: usize,
}

/// One randomised end-to-end liquidation.
///
/// Returns `false` when the liquidation was refused, which is a legitimate
/// outcome the invariants must survive rather than a failure.
fn one_liquidation(rng: &mut Rng, coverage: &mut ProtocolCoverage, step: usize) -> bool {
    let decimals = rng.pick(&[6u8, 9, 9, 2, 12]);
    let bonus = rng.pick(&[800u16, 1_000, 1_200, 1_500]);
    // Bounded by MAX_PROTOCOL_LIQUIDATION_SHARE_BPS: these go through the admin
    // instruction, which refuses anything above it. The arithmetic sweep above is
    // deliberately unbounded, because clamping is its own invariant.
    let share = rng.pick(&[0u16, 0, 25, 50, 150, 250]);
    // How far the collateral falls: shallow leaves the position healthy, deep
    // takes it past the 0.95 full-close line.
    let price_after = rng.pick(&[950u64, 900, 800, 700, 600, 500, 400]);
    let repay = rng.pick(&[1u64, 100, 1_000_000, 1_000_000_000, 1_000_000_000_000]);

    let whole = 10u64.pow(decimals as u32);
    let (mut env, cook, _core) = Env::core(1_000);
    let collateral = env.add_reserve(
        decimals,
        px(1_000),
        ReserveConfig {
            liquidation_bonus_bps: bonus,
            ..bcook_config()
        },
    );

    let supplier = env.create_user();
    env.fund(&supplier, cook.mint, tokens(200_000));
    env.supply(&supplier, &cook, tokens(200_000));

    let borrower = env.create_user();
    env.fund(&borrower, collateral.mint, 10_000 * whole);
    env.fund(&borrower, cook.mint, 0);
    let obligation = env.open_position(&borrower, &collateral, 10_000 * whole);
    if env
        .try_borrow(
            &borrower,
            &cook,
            obligation,
            tokens(5_200),
            &[&cook, &collateral],
        )
        .is_err()
    {
        return false;
    }

    if share > 0 {
        // Enabling the share is a loosening and waits.
        if env
            .set_protocol_liquidation_share(&collateral, share)
            .is_err()
        {
            return false;
        }
        env.warp_seconds(DAY + 1);
        if env.apply_pending_risk_config(&collateral).is_err() {
            return false;
        }
        coverage.positive_share += 1;
    } else {
        coverage.zero_share += 1;
    }

    match decimals {
        6 => coverage.six_decimals += 1,
        9 => coverage.nine_decimals += 1,
        _ => coverage.other_decimals += 1,
    }

    env.set_price(collateral.mint, px(price_after));
    let instructions = {
        let mut ixs = env.accrue_all_ixs(&[&cook, &collateral]);
        ixs.push(env.refresh_obligation_ix(obligation));
        ixs
    };
    let admin = env.admin.insecure_clone();
    if solana_kite::send_transaction_from_instructions(
        &mut env.svm,
        instructions,
        &[&admin],
        &admin.pubkey(),
    )
    .is_err()
    {
        return false;
    }

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

    let result = env.try_liquidate(&liquidator, &cook, &collateral, obligation, repay);
    if result.is_err() {
        // The position was healthy, or the close was too large. Either way the
        // state must be untouched.
        coverage.liquidation_refused += 1;
        if price_after >= 900 {
            coverage.healthy_position += 1;
        }
        let after = (
            env.balance(&vault),
            env.balance(&liquidator_account),
            env.balance(&protocol_account),
        );
        assert_eq!(
            before, after,
            "step {step}: a refused liquidation still moved collateral"
        );
        return false;
    }

    let after = (
        env.balance(&vault),
        env.balance(&liquidator_account),
        env.balance(&protocol_account),
    );
    let removed = before.0 - after.0;
    let to_liquidator = after.1 - before.1;
    let to_protocol = after.2 - before.2;

    let context = format!(
        "step {step}: decimals {decimals}, bonus {bonus}, share {share}, \
         price {price_after}, repay {repay}"
    );

    // --- conservation ------------------------------------------------------
    assert_eq!(
        removed,
        to_liquidator + to_protocol,
        "{context}: {removed} shares left the borrower but {to_liquidator} + \
         {to_protocol} arrived"
    );

    // --- the protocol's exact entitlement ----------------------------------
    let denominator = 10_000u128 + bonus as u128;
    let effective = share.min(bonus) as u128;
    assert!(
        (to_protocol as u128) * denominator <= (removed as u128) * effective,
        "{context}: the protocol took more than its share"
    );
    assert!(
        ((to_protocol as u128) + 1) * denominator > (removed as u128) * effective,
        "{context}: the protocol took less than a floor of its share"
    );

    // --- the liquidator keeps the principal equivalent ---------------------
    let principal_equivalent = (removed as u128) * 10_000 / denominator;
    assert!(
        to_liquidator as u128 >= principal_equivalent,
        "{context}: the protocol's share came out of the liquidator's principal"
    );

    // --- a zero share is the old behaviour, exactly ------------------------
    if share == 0 {
        assert_eq!(
            to_protocol, 0,
            "{context}: a zero share paid the protocol something"
        );
        assert_eq!(
            to_liquidator, removed,
            "{context}: a zero share did not give the liquidator everything"
        );
    }

    if to_protocol == 0 && share > 0 {
        coverage.protocol_rounded_to_zero += 1;
    }
    if to_protocol > 0 {
        coverage.protocol_nonzero += 1;
    }
    if removed < 1_000 {
        coverage.tiny_liquidation += 1;
    }
    if price_after <= 500 {
        coverage.deeply_underwater += 1;
    }
    // Below 0.95 health the whole position is closable; above it, half.
    if price_after <= 600 {
        coverage.full_close_factor += 1;
    } else {
        coverage.normal_close_factor += 1;
    }
    true
}

#[test]
fn fuzz_02_randomised_end_to_end_liquidations() {
    /*
     * Sampled rather than swept: each iteration stands up a whole market in
     * LiteSVM, so this is roughly four orders of magnitude slower per case than
     * the arithmetic layer above. The sweep lives there; this is here to catch
     * anything that lives in the instruction rather than the formula.
     */
    let mut rng = Rng(0x11B_0D00);
    let mut coverage = ProtocolCoverage::default();
    for step in 0..220 {
        one_liquidation(&mut rng, &mut coverage, step);
    }

    eprintln!("protocol coverage: {coverage:?}");
    let need = |what: &str, count: usize| {
        assert!(
            count > 0,
            "the run never produced {what}, so the invariant about it was never \
             checked end to end. Coverage: {coverage:?}"
        );
    };
    need("a zero share", coverage.zero_share);
    need("a positive share", coverage.positive_share);
    need("a nonzero protocol amount", coverage.protocol_nonzero);
    need("a 6-decimal collateral", coverage.six_decimals);
    need("a 9-decimal collateral", coverage.nine_decimals);
    need("another decimal scale", coverage.other_decimals);
    need("a refused liquidation", coverage.liquidation_refused);
    need("a normal close factor", coverage.normal_close_factor);
    need("a full close factor", coverage.full_close_factor);
    need("a deeply underwater position", coverage.deeply_underwater);
}
