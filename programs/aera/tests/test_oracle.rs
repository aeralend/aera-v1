//! The v0.2 oracle, end to end.
//!
//! v0.1's version of this file tested a guardian median: quorum, freshness,
//! signatures. None of that exists. What is tested here instead is the
//! property that replaced it — that the rate is *derived*, that nobody can
//! choose it, and that the protocol refuses rather than guesses when the
//! derivation stops making sense.
//!
//! Every test drives a real stake-pool layout. There is no mock oracle: the
//! program always parses the same bytes it would parse on Cookie Chain, and the
//! test simply controls what those bytes say. That is the only way the parser,
//! the fee walk, the bounds and the breaker are all actually exercised.

mod common;

use aera::constants::{DEFAULT_RATE_CEILING, DEFAULT_RATE_FLOOR, FIXED_POINT_SCALE};
use aera::instructions::admin::init_oracle::OracleConfig;
use aera::oracle::breaker::{BreakerConfig, OracleHealth};
use common::*;
use solana_keypair::Keypair;

/// A funded wallet, as every other suite defines it.
fn actor(env: &mut Env, mint: Pubkey, amount: u64) -> Keypair {
    let user = env.create_user();
    env.fund(&user, mint, amount);
    user
}

/// A stake-pool account owned by `owner`, at `key`, saying whatever we like.
#[allow(clippy::too_many_arguments)] // every one of them is a thing a test varies
fn place_pool(
    env: &mut Env,
    key: Pubkey,
    owner: Pubkey,
    pool_mint: Pubkey,
    lamports: u64,
    shares: u64,
    fee_bps: u16,
    epoch: u64,
) {
    let data = stake_pool_bytes(pool_mint, lamports, shares, fee_bps, epoch);
    env.svm
        .set_account(
            key,
            solana_account::Account {
                lamports: 1_000_000_000,
                data,
                owner,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
}

fn health_of(env: &Env, mint: Pubkey) -> OracleHealth {
    OracleHealth::from_u8(env.read_oracle(mint).health).unwrap()
}

// ===========================================================================
// The happy path, and the arithmetic behind it
// ===========================================================================

/// A correctly formed pool produces the rate its own accounting implies.
#[test]
fn a_correct_pool_prices_at_lamports_over_shares() {
    let (mut env, _cook, bcook) = Env::core(1_000);

    // 1.3 COOK per bCOOK, the ratio observed on Cookie Chain.
    //
    // `set_price` rather than `move_rate`: this test is about the derivation,
    // not the breaker, and +30% in one step is exactly what the breaker exists
    // to refuse. Establishing the world is a re-anchor.
    env.set_price(bcook.mint, px(1_300));

    let oracle = env.read_oracle(bcook.mint);
    assert_eq!(
        oracle.reference.gross_rate,
        FIXED_POINT_SCALE * 13 / 10,
        "gross rate is not lamports/shares"
    );
    assert_eq!(health_of(&env, bcook.mint), OracleHealth::Healthy);
}

/// The two reductions compose in the documented order, and stay separate.
///
/// This is the arithmetic from the v0.2 design decision, asserted against a
/// real account rather than against the helper in isolation:
///
///   effective = gross x (1 - withdrawal_fee)
#[test]
fn the_withdrawal_fee_is_applied_by_the_oracle_not_the_haircut() {
    let (mut env, _cook, bcook) = Env::core(1_000);
    // +10% would sit exactly on the emergency bound, so re-anchor rather than
    // asking the breaker's permission -- the subject here is the fee, not the
    // movement.
    env.set_pool(
        bcook.mint,
        px(1_100) as u64,
        POOL_SHARES,
        LIVE_WITHDRAWAL_FEE_BPS,
        2,
    );
    env.reset_breaker(bcook.mint);

    let oracle = env.read_oracle(bcook.mint);
    assert_eq!(
        oracle.reference.gross_rate,
        FIXED_POINT_SCALE * 11 / 10,
        "gross 1.10"
    );
    assert_eq!(
        oracle.reference.effective_rate,
        FIXED_POINT_SCALE * 1078 / 1000,
        "1.10 x 0.98 = 1.078 -- the fee belongs to the oracle"
    );
    assert_eq!(
        oracle.reference.withdrawal_fee_bps, 200,
        "the fee is recorded, not folded away"
    );
    assert!(
        oracle.reference.gross_rate > oracle.reference.effective_rate,
        "gross must survive alongside effective, or a fee change is indistinguishable \
         from a backing change"
    );
}

/// A fee rise reduces what collateral is worth, without touching any Aera
/// parameter. This is the mechanism that stops the pool operator quietly
/// consuming Aera's risk margin.
#[test]
fn a_higher_pool_fee_lowers_collateral_value_immediately() {
    let (mut env, _cook, bcook) = Env::core(1_000);

    env.set_pool(
        bcook.mint,
        px(1_000) as u64,
        POOL_SHARES,
        LIVE_WITHDRAWAL_FEE_BPS,
        2,
    );
    env.refresh_oracle(bcook.mint);
    let cheap = env.read_oracle(bcook.mint).reference.effective_rate;

    env.set_pool(
        bcook.mint,
        px(1_000) as u64,
        POOL_SHARES,
        LIVE_WITHDRAWAL_FEE_BPS * 2,
        3,
    );
    env.refresh_oracle(bcook.mint);
    let dear = env.read_oracle(bcook.mint).reference.effective_rate;

    assert!(
        dear < cheap,
        "doubling the redemption fee must lower collateral value"
    );
    assert_eq!(
        env.read_oracle(bcook.mint).reference.gross_rate,
        FIXED_POINT_SCALE,
        "the gross rate did not change, and must not appear to have"
    );
}

// ===========================================================================
// Account validation
// ===========================================================================

/// The source must be owned by the configured program.
#[test]
fn a_pool_owned_by_the_wrong_program_is_refused() {
    let (mut env, _cook, bcook) = Env::core(1_000);
    let pool = env.stake_pool_address(bcook.mint);

    place_pool(
        &mut env,
        pool,
        Pubkey::new_unique(), // not TEST_STAKE_POOL_PROGRAM
        bcook.mint,
        px(9_000) as u64,
        POOL_SHARES,
        200,
        5,
    );

    let result = env.try_refresh_oracle(bcook.mint);
    assert!(
        result.is_err(),
        "a foreign-owned account was read as a stake pool"
    );
    assert!(
        result.unwrap_err().contains("OracleOwnerMismatch"),
        "wrong owner must be refused as an owner mismatch"
    );
}

/// A pool that issues a different mint prices a different asset.
#[test]
fn a_pool_for_another_mint_is_refused() {
    let (mut env, cook, bcook) = Env::core(1_000);
    let pool = env.stake_pool_address(bcook.mint);

    // Right address, right owner, right shape -- wrong asset.
    place_pool(
        &mut env,
        pool,
        TEST_STAKE_POOL_PROGRAM,
        cook.mint,
        px(9_000) as u64,
        POOL_SHARES,
        200,
        5,
    );

    env.refresh_oracle(bcook.mint);
    assert_eq!(
        health_of(&env, bcook.mint),
        OracleHealth::Emergency,
        "a pool issuing the wrong mint must not be believed"
    );
}

/// Malformed data must abort the read rather than being interpreted.
#[test]
fn a_truncated_account_is_refused() {
    let (mut env, _cook, bcook) = Env::core(1_000);
    let pool = env.stake_pool_address(bcook.mint);

    let mut data = stake_pool_bytes(bcook.mint, px(1_000) as u64, POOL_SHARES, 200, 5);
    data.truncate(400);
    env.svm
        .set_account(
            pool,
            solana_account::Account {
                lamports: 1_000_000_000,
                data,
                owner: TEST_STAKE_POOL_PROGRAM,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

    env.refresh_oracle(bcook.mint);
    assert_eq!(health_of(&env, bcook.mint), OracleHealth::Emergency);
}

/// A corrupt variable-width tag must stop the walk, not be skipped.
///
/// This is the check that protects the fee read: past `epoch_fee` the layout is
/// only navigable by trusting each tag, so a tag outside its range means the
/// remaining offsets are unknown and nothing beyond it may be believed.
#[test]
fn a_corrupt_layout_tag_is_refused() {
    let (mut env, _cook, bcook) = Env::core(1_000);
    let pool = env.stake_pool_address(bcook.mint);

    let mut data = stake_pool_bytes(bcook.mint, px(1_000) as u64, POOL_SHARES, 200, 5);
    data[346] = 7; // not a FutureEpoch variant
    env.svm
        .set_account(
            pool,
            solana_account::Account {
                lamports: 1_000_000_000,
                data,
                owner: TEST_STAKE_POOL_PROGRAM,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

    env.refresh_oracle(bcook.mint);
    assert_eq!(health_of(&env, bcook.mint), OracleHealth::Emergency);
}

/// Zero shares and zero backing both mean "there is no rate".
#[test]
fn zero_supply_and_zero_backing_are_refused() {
    for (lamports, shares, label) in [
        (px(1_000) as u64, 0u64, "no shares"),
        (0u64, POOL_SHARES, "no backing"),
    ] {
        let (mut env, _cook, bcook) = Env::core(1_000);
        env.set_pool(bcook.mint, lamports, shares, 200, 5);
        env.refresh_oracle(bcook.mint);
        assert_eq!(
            health_of(&env, bcook.mint),
            OracleHealth::Emergency,
            "{label} must not produce a price"
        );
    }
}

/// A redemption fee past the configured bound freezes rather than being absorbed.
#[test]
fn a_fee_above_the_bound_stops_the_protocol_lending() {
    let (mut env, _cook, bcook) = Env::core(1_000);

    // 90%, far past DEFAULT_MAX_WITHDRAWAL_FEE_BPS.
    env.set_pool(bcook.mint, px(1_000) as u64, POOL_SHARES, 9_000, 5);
    env.refresh_oracle(bcook.mint);

    assert_eq!(
        health_of(&env, bcook.mint),
        OracleHealth::Emergency,
        "a punitive redemption fee must stop new lending, not be silently absorbed"
    );
}

/// The absolute band catches a rate that cannot be a bCOOK rate at all.
#[test]
fn a_rate_outside_the_absolute_band_is_refused() {
    let (mut env, _cook, bcook) = Env::core(1_000);

    // Below parity: a staking receipt worth less than the asset it wraps.
    env.set_pool(bcook.mint, px(500) as u64, POOL_SHARES, 200, 5);
    env.refresh_oracle(bcook.mint);
    assert_eq!(
        health_of(&env, bcook.mint),
        OracleHealth::Emergency,
        "below the floor"
    );

    let (mut env, _cook, bcook) = Env::core(1_000);
    // Above the ceiling.
    env.set_pool(bcook.mint, px(50_000) as u64, POOL_SHARES, 200, 5);
    env.refresh_oracle(bcook.mint);
    assert_eq!(
        health_of(&env, bcook.mint),
        OracleHealth::Emergency,
        "above the ceiling"
    );
}

/// Enormous values must error rather than wrap.
#[test]
fn absurd_magnitudes_do_not_overflow_into_a_plausible_rate() {
    let (mut env, _cook, bcook) = Env::core(1_000);
    env.set_pool(bcook.mint, u64::MAX, 1, 200, 5);
    env.refresh_oracle(bcook.mint);

    // u64::MAX / 1 is astronomically past the ceiling, so it is refused -- and
    // the point is that it is refused rather than wrapping into something small
    // and believable.
    assert_eq!(health_of(&env, bcook.mint), OracleHealth::Emergency);
    let oracle = env.read_oracle(bcook.mint);
    assert!(
        oracle.reference.gross_rate < DEFAULT_RATE_CEILING,
        "an overflowing rate reached the reference"
    );
}

// ===========================================================================
// The circuit breaker
// ===========================================================================

/// A normal reward epoch is accepted.
#[test]
fn a_rate_rise_inside_the_allowance_is_accepted() {
    let (mut env, _cook, bcook) = Env::core(1_000);
    let before = env.read_oracle(bcook.mint).reference.gross_rate;

    // +0.2%, about what Cookie Chain actually produced in one epoch.
    env.move_rate(bcook.mint, 1_002, 2);

    let after = env.read_oracle(bcook.mint).reference.gross_rate;
    assert!(
        after > before,
        "a legitimate reward epoch must move the reference"
    );
    assert_eq!(health_of(&env, bcook.mint), OracleHealth::Healthy);
}

/// A rise past the allowance freezes borrowing and does NOT move the reference.
///
/// The second half is the important half: if the reference followed the
/// suspicious rate, the freeze would be cosmetic and the next observation would
/// be judged against the very number under suspicion.
#[test]
fn a_rate_rise_past_the_allowance_freezes_without_moving_the_reference() {
    let (mut env, _cook, bcook) = Env::core(1_000);
    let before = env.read_oracle(bcook.mint).reference.gross_rate;

    env.move_rate(bcook.mint, 1_050, 2); // +5% in one epoch

    let oracle = env.read_oracle(bcook.mint);
    assert_eq!(
        oracle.reference.gross_rate, before,
        "the reference followed a rate the breaker refused"
    );
    assert_eq!(health_of(&env, bcook.mint), OracleHealth::BorrowFrozen);
    assert!(
        oracle.last_moved_bps >= 500,
        "the movement should be recorded for an operator"
    );
}

/// A fall is held to a tighter bound and is treated as an incident.
#[test]
fn a_rate_fall_past_its_bound_is_an_emergency_not_a_warning() {
    let (mut env, _cook, bcook) = Env::core(1_000);

    env.move_rate(bcook.mint, 985, 2); // -1.5%, past the 1% down allowance

    assert_eq!(
        health_of(&env, bcook.mint),
        OracleHealth::Emergency,
        "a stake pool's rate does not fall as a matter of course"
    );
}

/// The absolute emergency bound ignores however many epochs have passed.
#[test]
fn staleness_cannot_accumulate_past_the_emergency_bound() {
    let (mut env, _cook, bcook) = Env::core(1_000);

    // 100 epochs would otherwise allow 100 x 2% of movement.
    env.move_rate(bcook.mint, 1_500, 101); // +50%

    assert_eq!(health_of(&env, bcook.mint), OracleHealth::Emergency);
}

/// Allowance genuinely accumulates across epochs, within its cap.
#[test]
fn several_epochs_of_yield_are_accepted_together() {
    let (mut env, _cook, bcook) = Env::core(1_000);
    // Three epochs at 2% each = 6% allowed. +5% fits.
    env.move_rate(bcook.mint, 1_050, 4);

    // Accepted is the assertion. The state is RateWarning rather than Healthy
    // because three epochs without a crank is itself worth flagging -- and
    // RateWarning permits everything, which is the point of it being separate
    // from BorrowFrozen.
    let oracle = env.read_oracle(bcook.mint);
    assert_eq!(
        oracle.reference.gross_rate,
        FIXED_POINT_SCALE * 105 / 100,
        "three epochs of legitimate yield must be accepted"
    );
    assert!(
        health_of(&env, bcook.mint).permits(aera::oracle::breaker::RiskAction::Borrow),
        "an accepted observation must not block borrowing"
    );
}

/// A freeze is recoverable, and only to the truth.
#[test]
fn resetting_re_anchors_to_the_chain_rather_than_to_a_chosen_number() {
    let (mut env, _cook, bcook) = Env::core(1_000);

    env.move_rate(bcook.mint, 1_050, 2);
    assert_eq!(health_of(&env, bcook.mint), OracleHealth::BorrowFrozen);

    env.reset_breaker(bcook.mint);

    let oracle = env.read_oracle(bcook.mint);
    assert_eq!(health_of(&env, bcook.mint), OracleHealth::Healthy);
    assert_eq!(
        oracle.reference.gross_rate,
        FIXED_POINT_SCALE * 105 / 100,
        "reset must anchor to what the pool says, not to anything chosen"
    );
}

// ===========================================================================
// INVARIANT 10 — nobody can choose a price
// ===========================================================================

/// There is no instruction that accepts a rate, and the admin cannot write one.
///
/// The strongest form available: the admin re-anchors the breaker while the
/// pool says 1.05, and gets 1.05. They cannot get 9.0 by any argument, because
/// no instruction takes one.
#[test]
fn invariant_10_an_admin_cannot_choose_the_rate() {
    let (mut env, _cook, bcook) = Env::core(1_000);

    // The admin sets the pool to a number they would love to be true...
    env.set_pool(bcook.mint, px(1_050) as u64, POOL_SHARES, 200, 2);
    env.reset_breaker(bcook.mint);
    assert_eq!(
        env.read_oracle(bcook.mint).reference.gross_rate,
        FIXED_POINT_SCALE * 105 / 100,
        "reset takes the chain's number"
    );

    // ...and the only lever they have is which account is configured, which is
    // itself validated. Pointing at a pool for a different mint does not work.
    let forged = Pubkey::new_unique();
    place_pool(
        &mut env,
        forged,
        TEST_STAKE_POOL_PROGRAM,
        Pubkey::new_unique(), // some other mint
        px(9_000) as u64,
        POOL_SHARES,
        200,
        3,
    );

    let config = OracleConfig::native(
        TEST_STAKE_POOL_PROGRAM,
        forged,
        500,
        DEFAULT_RATE_FLOOR,
        DEFAULT_RATE_CEILING,
        TEST_DEPLOY_SLOT,
        TEST_UPGRADE_AUTHORITY,
    );
    env.set_oracle_with(bcook.mint, config);

    // Reconfiguring cleared the reference, and the new source cannot be read
    // because it issues a different mint.
    let result = env.try_reset_breaker(bcook.mint);
    assert!(
        result.is_err(),
        "an admin repointed the oracle at a pool for a different mint and got a price"
    );
}

/// Reconfiguring the source clears the reference rather than inheriting it.
#[test]
fn changing_the_source_does_not_inherit_the_old_reference() {
    let (mut env, _cook, bcook) = Env::core(1_000);
    assert!(env.read_oracle(bcook.mint).reference.is_set());

    let elsewhere = Pubkey::new_unique();
    place_pool(
        &mut env,
        elsewhere,
        TEST_STAKE_POOL_PROGRAM,
        bcook.mint,
        px(1_200) as u64,
        POOL_SHARES,
        200,
        3,
    );
    env.set_oracle_with(
        bcook.mint,
        OracleConfig::native(
            TEST_STAKE_POOL_PROGRAM,
            elsewhere,
            500,
            DEFAULT_RATE_FLOOR,
            DEFAULT_RATE_CEILING,
            TEST_DEPLOY_SLOT,
            TEST_UPGRADE_AUTHORITY,
        ),
    );

    let oracle = env.read_oracle(bcook.mint);
    assert!(
        !oracle.reference.is_set(),
        "a new source inherited the old source's credibility"
    );
    assert_eq!(health_of(&env, bcook.mint), OracleHealth::BorrowFrozen);
}

// ===========================================================================
// Staleness
// ===========================================================================

/// An oracle not refreshed in this transaction cannot be used for new risk.
#[test]
fn an_unrefreshed_oracle_blocks_borrowing() {
    let (mut env, cook, bcook) = Env::core(1_000);
    let supplier = actor(&mut env, cook.mint, tokens(10_000));
    env.supply(&supplier, &cook, tokens(10_000));

    let borrower = actor(&mut env, bcook.mint, tokens(10_000));
    env.fund(&borrower, cook.mint, 0);
    let obligation = env.open_position(&borrower, &bcook, tokens(10_000));

    // Advance a slot so the last refresh is no longer current, then send the
    // borrow with the accrue prelude but WITHOUT the oracle refresh.
    env.warp_slots(1);

    let mut instructions = env.accrue_only_ixs(&[&cook, &bcook]);
    instructions.push(env.refresh_obligation_ix(obligation));

    let result = env.send_raw(instructions, &[&borrower]);
    assert!(
        result.is_err(),
        "an obligation refreshed against an un-refreshed oracle was accepted"
    );
    assert!(
        result.unwrap_err().contains("OracleStale"),
        "an un-refreshed oracle must be refused as stale"
    );
}

// ===========================================================================
// Configuration
// ===========================================================================

/// A breaker whose emergency bound undercuts its own allowance is refused.
#[test]
fn an_incoherent_breaker_config_is_refused() {
    let (mut env, _cook, bcook) = Env::core(1_000);

    let mut config = OracleConfig::native(
        TEST_STAKE_POOL_PROGRAM,
        env.stake_pool_address(bcook.mint),
        500,
        DEFAULT_RATE_FLOOR,
        DEFAULT_RATE_CEILING,
        TEST_DEPLOY_SLOT,
        TEST_UPGRADE_AUTHORITY,
    );
    config.breaker = BreakerConfig {
        max_up_bps_per_epoch: 500,
        max_down_bps_per_epoch: 100,
        emergency_deviation_bps: 200, // below the up-allowance
        max_epoch_allowance: 10,
    };

    assert!(
        env.try_set_oracle_with(bcook.mint, config).is_err(),
        "an emergency bound below the per-epoch allowance makes the breaker dead code"
    );
}

/// A native source must name a program and an account.
#[test]
fn a_native_source_without_an_account_is_refused() {
    let (mut env, _cook, bcook) = Env::core(1_000);
    let config = OracleConfig::native(
        Pubkey::default(),
        Pubkey::default(),
        500,
        DEFAULT_RATE_FLOOR,
        DEFAULT_RATE_CEILING,
        TEST_DEPLOY_SLOT,
        TEST_UPGRADE_AUTHORITY,
    );
    assert!(env.try_set_oracle_with(bcook.mint, config).is_err());
}

/// A unit-of-account source reads nothing and is always exactly 1.
#[test]
fn the_unit_of_account_source_is_exactly_one() {
    let (mut env, cook, _bcook) = Env::core(1_000);

    env.set_oracle_with(cook.mint, OracleConfig::unit_of_account());
    env.refresh_oracle(cook.mint);

    let oracle = env.read_oracle(cook.mint);
    assert_eq!(
        oracle.reference.gross_rate, FIXED_POINT_SCALE,
        "one COOK is one COOK"
    );
    assert_eq!(
        oracle.reference.effective_rate, FIXED_POINT_SCALE,
        "and carries no fee"
    );
    assert_eq!(health_of(&env, cook.mint), OracleHealth::Healthy);
}
