//! Pauses, the timelock, the hard maxima, and the two rules that make Aera a
//! COOK bank rather than a general money market: aCOOK is not collateral, and
//! bCOOK is not borrowable.

mod common;

use aera::constants::*;
use common::*;

fn funded_borrower(
    env: &mut Env,
    cook: &ReserveHandle,
    bcook: &ReserveHandle,
) -> (solana_keypair::Keypair, Pubkey) {
    let supplier = env.create_user();
    env.fund(&supplier, cook.mint, tokens(5_000));
    env.supply(&supplier, cook, tokens(5_000));

    let borrower = env.create_user();
    env.fund(&borrower, bcook.mint, tokens(5_000));
    env.fund(&borrower, cook.mint, 0);
    let obligation = env.open_position(&borrower, bcook, tokens(5_000));
    (borrower, obligation)
}

// ---------------------------------------------------------------------------
// The two structural rules
// ---------------------------------------------------------------------------

/// aCOOK is not accepted as collateral. This is a program rule, not a UI
/// convention: the COOK reserve is configured `collateral_enabled = false`.
#[test]
fn acook_is_not_accepted_as_collateral() {
    let (mut env, cook, _) = Env::core(1_000);

    let user = env.create_user();
    env.fund(&user, cook.mint, tokens(1_000));
    let obligation = env.init_obligation(&user);
    let shares = env.supply(&user, &cook, tokens(1_000));
    let held = env.balance(&shares);

    assert_error(
        env.try_deposit_collateral(&user, &cook, obligation, held),
        "CollateralNotEnabled",
    );
}

/// bCOOK is collateral only. Nobody borrows the LST out of the vault.
#[test]
fn bcook_cannot_be_borrowed() {
    let (mut env, cook, bcook) = Env::core(1_000);
    let (borrower, obligation) = funded_borrower(&mut env, &cook, &bcook);

    assert_error(
        env.try_borrow(&borrower, &bcook, obligation, tokens(1), &[&cook, &bcook]),
        "BorrowNotEnabled",
    );
}

// ---------------------------------------------------------------------------
// Pauses
// ---------------------------------------------------------------------------

#[test]
fn pause_borrow_stops_borrowing_only() {
    let (mut env, cook, bcook) = Env::core(1_000);
    let (borrower, obligation) = funded_borrower(&mut env, &cook, &bcook);
    env.try_borrow(&borrower, &cook, obligation, tokens(100), &[&cook, &bcook])
        .unwrap();

    env.pause_borrow();

    assert_error(
        env.try_borrow(&borrower, &cook, obligation, tokens(1), &[&cook, &bcook]),
        "BorrowPaused",
    );

    // Supplying and repaying still work.
    let saver = env.create_user();
    env.fund(&saver, cook.mint, tokens(100));
    env.supply(&saver, &cook, tokens(100));
    env.try_repay(&borrower, &cook, obligation, tokens(50))
        .unwrap();

    env.unpause();
    // Bumped so this is a distinct transaction from the identical borrow that
    // was refused above, rather than a replay of it.
    env.bump_blockhash();
    env.try_borrow(&borrower, &cook, obligation, tokens(1), &[&cook, &bcook])
        .unwrap();
}

/// `pause_all` stops everything except repay. Refusing repayment while prices
/// move would manufacture liquidations the borrower could have avoided.
#[test]
fn pause_all_leaves_repay_open() {
    let (mut env, cook, bcook) = Env::core(1_000);
    let (borrower, obligation) = funded_borrower(&mut env, &cook, &bcook);
    env.try_borrow(&borrower, &cook, obligation, tokens(100), &[&cook, &bcook])
        .unwrap();

    env.pause_all();

    let saver = env.create_user();
    env.fund(&saver, cook.mint, tokens(100));
    assert_error(env.try_supply(&saver, &cook, tokens(100)), "ProtocolPaused");
    assert_error(
        env.try_borrow(&borrower, &cook, obligation, tokens(1), &[&cook, &bcook]),
        "ProtocolPaused",
    );

    // Repay is the exception.
    env.try_repay(&borrower, &cook, obligation, tokens(50))
        .unwrap();
}

#[test]
fn only_the_admin_may_pause() {
    let (mut env, _, _) = Env::core(1_000);
    let stranger = env.create_user();

    let instruction = anchor_lang::solana_program::instruction::Instruction {
        program_id: aera::id(),
        accounts: anchor_lang::ToAccountMetas::to_account_metas(
            &aera::accounts::PauseControl {
                global: env.global,
                admin: stranger.pubkey(),
            },
            None,
        ),
        data: anchor_lang::InstructionData::data(&aera::instruction::PauseAll {}),
    };
    let result = solana_kite::send_transaction_from_instructions(
        &mut env.svm,
        vec![instruction],
        &[&stranger],
        &stranger.pubkey(),
    )
    .map(|_| ())
    .map_err(|e| format!("{e:?}"));

    assert_error(result, "NotAdmin");
    assert!(!env.read_global().paused);
}

// ---------------------------------------------------------------------------
// Hard maxima — no admin, timelock or not, may cross these
// ---------------------------------------------------------------------------

#[test]
fn ltv_above_seventy_five_percent_is_refused() {
    let (mut env, _, bcook) = Env::core(1_000);
    let mut config = env.read_reserve(&bcook).config;
    config.loan_to_value_bps = MAX_ADMIN_LTV_BPS + 1;
    config.liquidation_threshold_bps = 9_000;
    assert_error(env.try_set_params(&bcook, config), "LtvAboveHardMax");
}

#[test]
fn liquidation_bonus_above_fifteen_percent_is_refused() {
    let (mut env, _, bcook) = Env::core(1_000);
    let mut config = env.read_reserve(&bcook).config;
    config.liquidation_bonus_bps = MAX_ADMIN_LIQUIDATION_BONUS_BPS + 1;
    assert_error(env.try_set_params(&bcook, config), "BonusAboveHardMax");
}

#[test]
fn reserve_factor_above_thirty_percent_is_refused() {
    let (mut env, cook, _) = Env::core(1_000);
    let mut config = env.read_reserve(&cook).config;
    config.reserve_factor_bps = MAX_ADMIN_RESERVE_FACTOR_BPS + 1;
    assert_error(
        env.try_set_params(&cook, config),
        "ReserveFactorAboveHardMax",
    );
}

#[test]
fn origination_fee_above_fifty_bps_is_refused() {
    let (mut env, cook, _) = Env::core(1_000);
    let mut config = env.read_reserve(&cook).config;
    config.origination_fee_bps = MAX_ADMIN_ORIGINATION_FEE_BPS + 1;
    assert_error(
        env.try_set_params(&cook, config),
        "OriginationFeeAboveHardMax",
    );
}

/// Changing the reserve factor at all is a loosening, so it waits — a raise
/// cannot be slipped in as if it were a risk reduction.
#[test]
fn changing_the_reserve_factor_waits_for_the_timelock() {
    let (mut env, cook, _) = Env::core(1_000);
    let original = env.read_reserve(&cook).config.reserve_factor_bps;

    let mut config = env.read_reserve(&cook).config;
    config.reserve_factor_bps = 2_000;
    env.try_set_params(&cook, config).unwrap();

    assert_eq!(
        env.read_reserve(&cook).config.reserve_factor_bps,
        original,
        "a fee change must not apply immediately"
    );

    env.warp_seconds(DEFAULT_PARAM_TIMELOCK_SECONDS);
    env.try_apply_pending(&cook).unwrap();
    assert_eq!(env.read_reserve(&cook).config.reserve_factor_bps, 2_000);
}

/// You may not be allowed to borrow past the point you would be liquidated.
#[test]
fn ltv_above_the_liquidation_threshold_is_refused() {
    let (mut env, _, bcook) = Env::core(1_000);
    let mut config = env.read_reserve(&bcook).config;
    config.loan_to_value_bps = 7_000;
    config.liquidation_threshold_bps = 6_500;
    assert_error(env.try_set_params(&bcook, config), "InvalidConfig");
}

// ---------------------------------------------------------------------------
// Timelock: tightening is instant, loosening waits
// ---------------------------------------------------------------------------

#[test]
fn lowering_ltv_is_instant() {
    let (mut env, _, bcook) = Env::core(1_000);
    let mut config = env.read_reserve(&bcook).config;
    config.loan_to_value_bps = 4_000;
    env.try_set_params(&bcook, config).unwrap();

    assert_eq!(env.read_reserve(&bcook).config.loan_to_value_bps, 4_000);
    assert_eq!(env.read_reserve(&bcook).pending.eta, 0, "nothing queued");
}

#[test]
fn raising_ltv_waits_for_the_timelock() {
    let (mut env, _, bcook) = Env::core(1_000);
    let original = env.read_reserve(&bcook).config.loan_to_value_bps;

    let mut config = env.read_reserve(&bcook).config;
    config.loan_to_value_bps = 7_000;
    // The liquidation threshold has to move with it: LTV may never exceed the
    // line it would be liquidated at, and `validate` enforces that on the
    // queued config at apply time as well as here.
    config.liquidation_threshold_bps = 7_500;
    env.try_set_params(&bcook, config).unwrap();

    // Not applied — queued.
    let reserve = env.read_reserve(&bcook);
    assert_eq!(
        reserve.config.loan_to_value_bps, original,
        "must not apply yet"
    );
    assert!(reserve.pending.eta > 0, "must be queued");
    assert_eq!(reserve.pending.config.loan_to_value_bps, 7_000);

    // Too early.
    assert_error(env.try_apply_pending(&bcook), "TimelockNotElapsed");

    // After 24h it lands.
    env.warp_seconds(DEFAULT_PARAM_TIMELOCK_SECONDS);
    env.try_apply_pending(&bcook).unwrap();

    let reserve = env.read_reserve(&bcook);
    assert_eq!(reserve.config.loan_to_value_bps, 7_000);
    assert_eq!(reserve.pending.eta, 0, "queue cleared after applying");
}

/// A queued change is re-validated when it lands, so a raise past the hard
/// maximum cannot be smuggled through by queueing it first.
#[test]
fn raising_a_cap_waits_but_cutting_one_does_not() {
    let (mut env, cook, _) = Env::core(1_000);

    // Cut: instant.
    let mut config = env.read_reserve(&cook).config;
    config.supply_cap = tokens(1_000);
    config.borrow_cap = tokens(1_000);
    env.try_set_params(&cook, config).unwrap();
    assert_eq!(env.read_reserve(&cook).config.supply_cap, tokens(1_000));

    // Raise: queued.
    let mut config = env.read_reserve(&cook).config;
    config.supply_cap = tokens(9_000);
    env.try_set_params(&cook, config).unwrap();
    assert_eq!(
        env.read_reserve(&cook).config.supply_cap,
        tokens(1_000),
        "a raise must not apply immediately"
    );

    env.warp_seconds(DEFAULT_PARAM_TIMELOCK_SECONDS);
    env.try_apply_pending(&cook).unwrap();
    assert_eq!(env.read_reserve(&cook).config.supply_cap, tokens(9_000));
}

#[test]
fn applying_with_nothing_queued_is_refused() {
    let (mut env, _, bcook) = Env::core(1_000);
    assert_error(env.try_apply_pending(&bcook), "NoPendingConfig");
}

/// Turning a switch off is a tightening; turning it back on is not.
#[test]
fn disabling_borrowing_is_instant_and_re_enabling_waits() {
    let (mut env, cook, _) = Env::core(1_000);

    let mut config = env.read_reserve(&cook).config;
    config.borrow_enabled = false;
    env.try_set_params(&cook, config).unwrap();
    assert!(!env.read_reserve(&cook).config.borrow_enabled);

    let mut config = env.read_reserve(&cook).config;
    config.borrow_enabled = true;
    env.try_set_params(&cook, config).unwrap();
    assert!(
        !env.read_reserve(&cook).config.borrow_enabled,
        "re-enabling must wait"
    );

    env.warp_seconds(DEFAULT_PARAM_TIMELOCK_SECONDS);
    env.try_apply_pending(&cook).unwrap();
    assert!(env.read_reserve(&cook).config.borrow_enabled);
}
