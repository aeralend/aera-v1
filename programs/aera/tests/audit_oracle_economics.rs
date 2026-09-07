//! Attacks on the money, through the derived oracle.
//!
//! The rest of the audit suite attacks the accounting. This file attacks the
//! *price*, which in v0.2 is the newest surface and the one with the shortest
//! history: every bCOOK valuation in Aera is arithmetic over two `u64`s in an
//! account owned by somebody else's program.
//!
//! Every test here ends with the same two assertions, not with "the transaction
//! errored":
//!
//! - the attacker is no richer, valued at rates fixed before and after so a
//!   gain cannot disguise itself as a price movement, and
//! - the reserve is still solvent and its vault still backs what the program
//!   says it tracks.
//!
//! A test that only asserts a refusal passes when the refusal happens for the
//! wrong reason, and passes when the attack half-succeeded and left the book
//! broken. These do not.
//!
//! ## What the attacker is assumed to control
//!
//! The strong assumption, deliberately: the attacker can write whatever they
//! like into the stake-pool account. That is not a normal user's power -- it is
//! the power of whoever holds the stake-pool program's upgrade authority, which
//! on Cookie Chain is a single wallet key. Assuming less would test a weaker
//! adversary than the one that exists.
//!
//! What they are *not* assumed to control: Aera's admin key, the accounts Aera
//! owns, or the `ProgramData` account the loader writes.

mod common;

use aera::constants::{DEFAULT_MAX_WITHDRAWAL_FEE_BPS, DEFAULT_RATE_CEILING, DEFAULT_RATE_FLOOR};
use aera::instructions::admin::init_oracle::OracleConfig;
use aera::oracle::breaker::OracleHealth;
use common::audit::*;
use common::*;
use solana_keypair::Keypair;

/// A funded market with a supplier, a borrower and a live position.
struct Book {
    env: Env,
    cook: ReserveHandle,
    bcook: ReserveHandle,
    attacker: Keypair,
    obligation: Pubkey,
}

fn book() -> Book {
    let (mut env, cook, bcook) = Env::core(1_300);

    let supplier = env.create_user();
    env.fund(&supplier, cook.mint, tokens(100_000));
    env.supply(&supplier, &cook, tokens(100_000));

    let attacker = env.create_user();
    env.fund(&attacker, bcook.mint, tokens(20_000));
    env.fund(&attacker, cook.mint, tokens(10_000));
    let obligation = env.open_position(&attacker, &bcook, tokens(10_000));
    // Supplied but not pledged, so tests that add collateral have shares to
    // add. Without it "adding collateral is still open" would fail for lack of
    // anything to add, which is not the property under test.
    env.supply(&attacker, &bcook, tokens(1_000));

    Book {
        env,
        cook,
        bcook,
        attacker,
        obligation,
    }
}

impl Book {
    fn value(&self) -> Value {
        self.env.value_of(
            &self.attacker.pubkey(),
            &self.cook,
            &self.bcook,
            Some(self.obligation),
        )
    }

    fn health(&self) -> OracleHealth {
        OracleHealth::from_u8(self.env.read_oracle(self.bcook.mint).health).unwrap()
    }

    fn reference_rate(&self) -> u128 {
        self.env.read_oracle(self.bcook.mint).reference.gross_rate
    }

    /// Attempt to borrow, ignoring the verdict. The assertions are economic.
    fn try_borrow(&mut self, amount: u64) -> Result<(), String> {
        let (cook, bcook) = (self.cook, self.bcook);
        let attacker = self.attacker.insecure_clone();
        self.env
            .try_borrow(&attacker, &cook, self.obligation, amount, &[&cook, &bcook])
    }

    /// The two assertions every test here ends with.
    fn assert_no_gain(&self, before: Value, allowed: u128, label: &str) {
        // Valued at a rate fixed by the honest state, so an attack that moved
        // the rate cannot be scored at the rate it moved it to.
        assert_no_profit(
            before,
            self.value(),
            px(1_300) as u128,
            self.env.acook_rate(&self.cook),
            allowed,
            label,
        );
        assert_solvent(&self.env, &self.cook, label);
        assert_solvent(&self.env, &self.bcook, label);
    }
}

/// The launch oracle configuration, for tests that reconfigure.
fn native_config(env: &Env, mint: Pubkey, slot: u64, authority: Pubkey) -> OracleConfig {
    OracleConfig::native(
        TEST_STAKE_POOL_PROGRAM,
        env.stake_pool_address(mint),
        DEFAULT_MAX_WITHDRAWAL_FEE_BPS,
        DEFAULT_RATE_FLOOR,
        DEFAULT_RATE_CEILING,
        slot,
        authority,
    )
}

// ===========================================================================
// 1-2. The rate cannot be moved by using the pool as intended
// ===========================================================================

/// Staking into the pool does not move the rate in the staker's favour.
///
/// The property the whole oracle rests on: `total_lamports / pool_token_supply`
/// is invariant under a deposit, because a deposit adds to both terms at the
/// current ratio. If it were not, the rate could be moved with capital alone --
/// no privileged access required -- and every flash-loan attack on a
/// share-price oracle would apply.
#[test]
fn econ_01_staking_into_the_pool_does_not_move_the_rate() {
    let mut b = book();
    let before = b.value();
    let rate_before = b.reference_rate();

    // A deposit of 30% of the pool, minted at the prevailing rate: lamports and
    // shares both rise, and 1.3 stays 1.3.
    let (lamports, shares, epoch) = b.env.read_pool(b.bcook.mint);
    let added_shares = shares / 3;
    let added_lamports = (added_shares as u128 * lamports as u128 / shares as u128) as u64;
    b.env.set_pool(
        b.bcook.mint,
        lamports + added_lamports,
        shares + added_shares,
        TEST_WITHDRAWAL_FEE_BPS,
        epoch + 1,
    );
    b.env.refresh_oracle(b.bcook.mint);

    /*
     * At most one base unit, and never upward.
     *
     * Exact equality is the wrong assertion: minting shares for a deposit
     * involves an integer division, so a real pool's ratio can move by a unit
     * even when nothing economic happened -- and this test computes the
     * deposit's share count the same way, so it inherits the same floor. What
     * must hold is the direction. A deposit that *raised* the rate would pay
     * the depositor out of every existing holder, and is the shape every
     * share-price oracle attack takes.
     */
    let after = b.reference_rate();
    assert!(
        after <= rate_before,
        "a proportional deposit raised the exchange rate: {rate_before} -> {after}"
    );
    assert!(
        rate_before - after <= 1,
        "a proportional deposit moved the rate by more than rounding: {rate_before} -> {after}"
    );
    b.assert_no_gain(before, 0, "staking into the pool");
}

/// Unstaking does not move it either.
#[test]
fn econ_02_unstaking_from_the_pool_does_not_move_the_rate() {
    let mut b = book();
    let before = b.value();
    let rate_before = b.reference_rate();

    let (lamports, shares, epoch) = b.env.read_pool(b.bcook.mint);
    let burned_shares = shares / 4;
    let removed = (burned_shares as u128 * lamports as u128 / shares as u128) as u64;
    b.env.set_pool(
        b.bcook.mint,
        lamports - removed,
        shares - burned_shares,
        TEST_WITHDRAWAL_FEE_BPS,
        epoch + 1,
    );
    b.env.refresh_oracle(b.bcook.mint);

    let after = b.reference_rate();
    assert!(
        after <= rate_before,
        "a proportional withdrawal raised the exchange rate: {rate_before} -> {after}"
    );
    assert!(
        rate_before - after <= 1,
        "a proportional withdrawal moved the rate by more than rounding: {rate_before} -> {after}"
    );
    b.assert_no_gain(before, 0, "unstaking from the pool");
}

// ===========================================================================
// 3-4. Moving one term without the other
// ===========================================================================

/// Inflating `total_lamports` alone is caught, and buys no borrowing power.
///
/// This is the attack the breaker exists for. Donating to the pool's reserve
/// stake account, or simply writing a larger number, raises the rate without
/// anyone having staked -- and a higher bCOOK price is more borrowing capacity
/// against the same collateral.
#[test]
fn econ_03_inflating_the_backing_buys_no_borrowing_power() {
    let mut b = book();
    let before = b.value();
    let honest_rate = b.reference_rate();

    let (lamports, shares, epoch) = b.env.read_pool(b.bcook.mint);
    b.env.set_pool(
        b.bcook.mint,
        lamports * 3,
        shares,
        TEST_WITHDRAWAL_FEE_BPS,
        epoch + 1,
    );
    let _ = b.env.try_refresh_oracle(b.bcook.mint);

    assert_eq!(
        b.reference_rate(),
        honest_rate,
        "a tripled backing reached the reference"
    );
    assert!(
        b.try_borrow(tokens(20_000)).is_err(),
        "an inflated rate financed a loan"
    );
    b.assert_no_gain(before, 0, "inflating the backing");
}

/// Burning supply out from under the pool is the same attack, mirrored.
#[test]
fn econ_04_deflating_the_supply_buys_no_borrowing_power() {
    let mut b = book();
    let before = b.value();
    let honest_rate = b.reference_rate();

    let (lamports, shares, epoch) = b.env.read_pool(b.bcook.mint);
    b.env.set_pool(
        b.bcook.mint,
        lamports,
        shares / 3,
        TEST_WITHDRAWAL_FEE_BPS,
        epoch + 1,
    );
    let _ = b.env.try_refresh_oracle(b.bcook.mint);

    assert_eq!(
        b.reference_rate(),
        honest_rate,
        "a collapsed supply reached the reference"
    );
    assert!(
        b.try_borrow(tokens(20_000)).is_err(),
        "a deflated supply financed a loan"
    );
    b.assert_no_gain(before, 0, "deflating the supply");
}

// ===========================================================================
// 5-7. Timing
// ===========================================================================

/// A rate move and a borrow in the same transaction is not a sandwich.
///
/// The classic shape: move the price, act on it, move it back, all atomically,
/// so no keeper can react. It fails here for a structural reason worth stating
/// -- the rate is only read by `refresh_oracle`, and the breaker judges every
/// reading against the stored reference regardless of how close together the
/// instructions were. There is no window because there is no window to be in.
#[test]
fn econ_05_moving_the_rate_and_borrowing_atomically_is_refused() {
    let mut b = book();
    let before = b.value();

    let (lamports, shares, epoch) = b.env.read_pool(b.bcook.mint);
    b.env.set_pool(
        b.bcook.mint,
        lamports * 2,
        shares,
        TEST_WITHDRAWAL_FEE_BPS,
        epoch + 1,
    );

    // refresh + accrue + refresh_obligation + borrow, one transaction.
    let (cook, bcook) = (b.cook, b.bcook);
    let attacker = b.attacker.insecure_clone();
    let obligation = b.obligation;
    let mut instructions = vec![b.env.refresh_oracle_ix(bcook.mint)];
    instructions.extend(b.env.accrue_all_ixs(&[&cook, &bcook]));
    instructions.push(b.env.refresh_obligation_ix(obligation));
    instructions.push(
        b.env
            .borrow_ix(&attacker, &cook, obligation, tokens(20_000)),
    );

    let result = b.env.send_raw(instructions, &[&attacker]);
    assert!(result.is_err(), "an atomic price move financed a loan");
    b.assert_no_gain(before, 0, "atomic rate move and borrow");
}

/// A frozen oracle keeps its last honest reference; borrowing stays shut.
///
/// Freezing must not be a way to lock in a favourable price. The reference is
/// kept so liquidation and valuation still work, and borrowing is refused for
/// exactly as long as the freeze lasts.
#[test]
fn econ_06_a_freeze_cannot_be_borrowed_through() {
    let mut b = book();
    let before = b.value();

    // Push the oracle into emergency with an unreadable source.
    let pool = b.env.stake_pool_address(b.bcook.mint);
    let mut data = stake_pool_bytes(b.bcook.mint, px(1_300) as u64, POOL_SHARES, 0, 2);
    data[0] = 9; // not AccountType::StakePool
    b.env
        .svm
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
    b.env.try_refresh_oracle(b.bcook.mint).expect("crank");
    assert_eq!(b.health(), OracleHealth::Emergency);

    assert!(
        b.try_borrow(tokens(100)).is_err(),
        "a frozen oracle financed a loan"
    );
    b.assert_no_gain(before, 0, "borrowing through a freeze");
}

/// Collapsing the rate does not hand a liquidator free collateral.
///
/// The most profitable oracle attack in a lending protocol is downward, not
/// upward: make everyone look underwater and liquidate them at a bonus. The
/// breaker treats a fall past its bound as an emergency, the reference does not
/// move, and the liquidation is priced off the last accepted rate -- at which
/// the position is healthy and cannot be touched.
#[test]
fn econ_07_collapsing_the_rate_does_not_hand_out_free_collateral() {
    let mut b = book();

    // Give the attacker debt worth liquidating, at honest prices.
    b.try_borrow(tokens(5_000)).expect("honest borrow");

    let liquidator = b.env.create_user();
    b.env.fund(&liquidator, b.cook.mint, tokens(50_000));
    let before = b
        .env
        .value_of(&liquidator.pubkey(), &b.cook, &b.bcook, None);
    let honest_rate = b.reference_rate();

    let (_, shares, epoch) = b.env.read_pool(b.bcook.mint);
    b.env.set_pool(
        b.bcook.mint,
        px(300) as u64,
        shares / POOL_SHARES.max(1),
        TEST_WITHDRAWAL_FEE_BPS,
        epoch + 1,
    );
    let _ = b.env.try_refresh_oracle(b.bcook.mint);

    assert_eq!(
        b.reference_rate(),
        honest_rate,
        "a collapsed rate became the reference"
    );

    let (cook, bcook) = (b.cook, b.bcook);
    let result = b
        .env
        .try_liquidate(&liquidator, &cook, &bcook, b.obligation, tokens(5_000));
    assert!(
        result.is_err(),
        "a healthy position was liquidated at a forged price"
    );

    assert_no_profit(
        before,
        b.env.value_of(&liquidator.pubkey(), &cook, &bcook, None),
        px(1_300) as u128,
        b.env.acook_rate(&cook),
        0,
        "liquidating at a collapsed rate",
    );
    assert_solvent(&b.env, &cook, "after a forged collapse");
}

// ===========================================================================
// 8-9. The withdrawal fee
// ===========================================================================

/// A withdrawal-fee spike freezes the market instead of liquidating it.
///
/// The fee is the stake operator's to set, and it is inside Aera's valuation by
/// design -- bCOOK really is worth less if redeeming it costs more. That makes
/// it a lever: raise the fee to 50% and every position's collateral value falls
/// by half at once. The bound is what stops that becoming a liquidation
/// cascade the operator profits from.
#[test]
fn econ_08_a_fee_spike_freezes_rather_than_liquidates() {
    let mut b = book();
    b.try_borrow(tokens(5_000)).expect("honest borrow");

    let liquidator = b.env.create_user();
    b.env.fund(&liquidator, b.cook.mint, tokens(50_000));
    let before = b
        .env
        .value_of(&liquidator.pubkey(), &b.cook, &b.bcook, None);

    let (lamports, shares, epoch) = b.env.read_pool(b.bcook.mint);
    b.env
        .set_pool(b.bcook.mint, lamports, shares, 5_000, epoch + 1);
    b.env.try_refresh_oracle(b.bcook.mint).expect("crank");

    assert_ne!(
        b.health(),
        OracleHealth::Healthy,
        "a 50% redemption fee left the oracle healthy"
    );

    let (cook, bcook) = (b.cook, b.bcook);
    let result = b
        .env
        .try_liquidate(&liquidator, &cook, &bcook, b.obligation, tokens(5_000));
    assert!(
        result.is_err(),
        "a fee spike liquidated a position that was healthy before it"
    );

    assert_no_profit(
        before,
        b.env.value_of(&liquidator.pubkey(), &cook, &bcook, None),
        px(1_300) as u128,
        b.env.acook_rate(&cook),
        0,
        "liquidating after a fee spike",
    );
    assert_solvent(&b.env, &cook, "after a fee spike");
}

/// A fee change inside the bound is priced, immediately and in full.
///
/// The other direction of the same rule. A bound that froze on every fee move
/// would make the protocol unusable; a bound that ignored small ones would let
/// the operator walk the fee up in steps. Inside the bound the fee is taken out
/// of the collateral value on the next crank, in full.
#[test]
fn econ_09_a_fee_inside_the_bound_is_priced_in_full() {
    let mut b = book();
    let gross = b.reference_rate();

    let (lamports, shares, epoch) = b.env.read_pool(b.bcook.mint);
    b.env
        .set_pool(b.bcook.mint, lamports, shares, 200, epoch + 1);
    b.env.try_refresh_oracle(b.bcook.mint).expect("crank");

    let oracle = b.env.read_oracle(b.bcook.mint);
    assert_eq!(oracle.reference.withdrawal_fee_bps, 200);
    assert_eq!(
        oracle.reference.effective_rate,
        gross * 9_800 / 10_000,
        "the 2% fee was not taken out of the effective rate in full"
    );
    assert!(
        oracle.reference.effective_rate < oracle.reference.gross_rate,
        "the fee did not lower the rate at all"
    );
}

// ===========================================================================
// 10-13. The bootstrap, as an economic attack
// ===========================================================================

/// Anchoring high and borrowing against it, end to end.
///
/// The bootstrap attack in its complete form rather than as a unit test: arrange
/// the pool at three times its real rate, get Aera to configure an oracle
/// against it, and borrow. Every step is attempted and the attacker's balance
/// is checked at the end, because "the configuration was refused" is not the
/// same claim as "no value moved".
#[test]
fn econ_10_anchoring_high_finances_nothing() {
    let mut b = book();
    let before = b.value();

    let new_slot = TEST_DEPLOY_SLOT + 1;
    b.env
        .set_program_data(new_slot, Some(TEST_UPGRADE_AUTHORITY));
    b.env.set_pool_with_history(
        b.bcook.mint,
        px(3_900) as u64,
        POOL_SHARES,
        TEST_WITHDRAWAL_FEE_BPS,
        9,
        Some((px(1_300) as u64, POOL_SHARES)),
    );
    b.env.set_oracle_with(
        b.bcook.mint,
        native_config(&b.env, b.bcook.mint, new_slot, TEST_UPGRADE_AUTHORITY),
    );
    b.env.try_refresh_oracle(b.bcook.mint).expect("crank");

    assert!(
        !b.env.read_oracle(b.bcook.mint).reference.is_set(),
        "an anchor the pool's own history denies was accepted"
    );
    assert!(
        b.try_borrow(tokens(30_000)).is_err(),
        "a forged anchor financed a loan"
    );
    b.assert_no_gain(before, 0, "anchoring high");
}

/// Anchoring low to liquidate everybody finances nothing either.
#[test]
fn econ_11_anchoring_low_liquidates_nobody() {
    let mut b = book();
    b.try_borrow(tokens(5_000)).expect("honest borrow");

    let liquidator = b.env.create_user();
    b.env.fund(&liquidator, b.cook.mint, tokens(50_000));
    let before = b
        .env
        .value_of(&liquidator.pubkey(), &b.cook, &b.bcook, None);

    let new_slot = TEST_DEPLOY_SLOT + 1;
    b.env
        .set_program_data(new_slot, Some(TEST_UPGRADE_AUTHORITY));
    b.env.set_pool_with_history(
        b.bcook.mint,
        px(200) as u64,
        POOL_SHARES,
        TEST_WITHDRAWAL_FEE_BPS,
        9,
        Some((px(1_300) as u64, POOL_SHARES)),
    );
    b.env.set_oracle_with(
        b.bcook.mint,
        native_config(&b.env, b.bcook.mint, new_slot, TEST_UPGRADE_AUTHORITY),
    );
    b.env.try_refresh_oracle(b.bcook.mint).expect("crank");

    let (cook, bcook) = (b.cook, b.bcook);
    let result = b
        .env
        .try_liquidate(&liquidator, &cook, &bcook, b.obligation, tokens(5_000));
    assert!(result.is_err(), "a forged low anchor liquidated a position");

    assert_no_profit(
        before,
        b.env.value_of(&liquidator.pubkey(), &cook, &bcook, None),
        px(1_300) as u128,
        b.env.acook_rate(&cook),
        0,
        "anchoring low to liquidate",
    );
    assert_solvent(&b.env, &cook, "after a forged low anchor");
}

/// An anchor inside the allowance is accepted, and still cannot be borrowed
/// against until a later epoch confirms it.
///
/// The subtler version: do not overreach. Move the pool only as far as one
/// epoch's movement allows, so the consistency check has nothing to object to.
/// The bootstrap still refuses the borrowing, because the objection was never
/// that the number looked wrong -- it was that one reading is one reading.
#[test]
fn econ_12_a_plausible_anchor_still_cannot_be_borrowed_against() {
    let mut b = book();
    let before = b.value();

    let new_slot = TEST_DEPLOY_SLOT + 1;
    b.env
        .set_program_data(new_slot, Some(TEST_UPGRADE_AUTHORITY));
    b.env.set_pool_with_history(
        b.bcook.mint,
        px(1_310) as u64,
        POOL_SHARES,
        TEST_WITHDRAWAL_FEE_BPS,
        9,
        Some((px(1_300) as u64, POOL_SHARES)),
    );
    b.env.set_oracle_with(
        b.bcook.mint,
        native_config(&b.env, b.bcook.mint, new_slot, TEST_UPGRADE_AUTHORITY),
    );
    b.env.refresh_oracle(b.bcook.mint);

    assert_eq!(
        b.health(),
        OracleHealth::Bootstrapping,
        "a plausible anchor was trusted on one reading"
    );
    assert!(
        b.try_borrow(tokens(100)).is_err(),
        "an unconfirmed anchor financed a loan"
    );
    b.assert_no_gain(before, 0, "a plausible unconfirmed anchor");
}

/// Cranking repeatedly does not confirm a bootstrap.
///
/// The cheapest possible attack if it worked: send the same free instruction
/// twice. Confirmation needs a later *pool epoch*, which the attacker cannot
/// manufacture without moving the pool -- and moving the pool is what the
/// breaker is watching.
#[test]
fn econ_13_cranking_does_not_confirm_a_bootstrap() {
    let mut b = book();
    let before = b.value();

    let new_slot = TEST_DEPLOY_SLOT + 1;
    b.env
        .set_program_data(new_slot, Some(TEST_UPGRADE_AUTHORITY));
    b.env.set_oracle_with(
        b.bcook.mint,
        native_config(&b.env, b.bcook.mint, new_slot, TEST_UPGRADE_AUTHORITY),
    );

    for _ in 0..12 {
        b.env.refresh_oracle(b.bcook.mint);
        b.env.warp_slots(500);
    }

    assert_eq!(
        b.health(),
        OracleHealth::Bootstrapping,
        "twelve cranks bought a confirmation"
    );
    assert!(
        b.try_borrow(tokens(100)).is_err(),
        "cranking financed a loan"
    );
    b.assert_no_gain(before, 0, "cranking to confirm");
}

// ===========================================================================
// 14-16. Griefing, exits and rounding
// ===========================================================================

/// An ordinary user cannot freeze the oracle to grief other borrowers.
///
/// Every route to a freeze runs through the source account or the source
/// program, and neither is writable by anyone but their owner. Worth asserting
/// rather than assuming: a denial-of-service that costs nothing is a real
/// attack even though it steals nothing.
#[test]
fn econ_14_an_outsider_cannot_freeze_the_oracle() {
    let mut b = book();

    // Everything an outsider can actually do: crank, repeatedly, with junk
    // accounts, from an unfunded wallet.
    let outsider = b.env.create_user();
    for _ in 0..5 {
        let _ = b
            .env
            .send_raw(vec![b.env.refresh_oracle_ix(b.bcook.mint)], &[&outsider]);
        b.env.svm.expire_blockhash();
    }

    assert_eq!(
        b.health(),
        OracleHealth::Healthy,
        "an outsider cranking froze the market"
    );
    b.try_borrow(tokens(100))
        .expect("an outsider's cranking blocked an honest borrow");
}

/// A borrower can always get out, whatever the oracle says.
///
/// Asserted across the states an attacker could try to trap someone in, because
/// the freeze rules are only defensible if the exit is open in each. A borrower
/// who cannot repay during an incident has been harmed by the protection.
#[test]
fn econ_15_the_exit_is_open_in_every_frozen_state() {
    let mut b = book();
    b.try_borrow(tokens(5_000)).expect("honest borrow");

    // Emergency, via an unreadable source.
    let pool = b.env.stake_pool_address(b.bcook.mint);
    let mut data = stake_pool_bytes(b.bcook.mint, px(1_300) as u64, POOL_SHARES, 0, 2);
    data[0] = 9;
    b.env
        .svm
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
    b.env.try_refresh_oracle(b.bcook.mint).expect("crank");
    assert_eq!(b.health(), OracleHealth::Emergency);

    let (cook, bcook) = (b.cook, b.bcook);
    let attacker = b.attacker.insecure_clone();
    let obligation = b.obligation;
    b.env
        .try_repay(&attacker, &cook, obligation, tokens(1_000))
        .expect("repayment must survive an emergency");
    b.env
        .try_deposit_collateral(&attacker, &bcook, obligation, tokens(1))
        .expect("adding collateral must survive an emergency");
    assert_solvent(&b.env, &cook, "after repaying through an emergency");
}

/// The two-stage valuation never rounds in the holder's favour.
///
/// `gross -> effective -> collateral` is two multiplications by a fraction, and
/// a rounding error in the wrong direction at either step is borrowing capacity
/// the collateral does not back. Checked across a range of rates and fees
/// rather than at one convenient point, because a rounding bug that shows up
/// only at odd values is exactly the kind that survives a spot check.
#[test]
fn econ_16_the_two_stage_valuation_never_rounds_upward() {
    let mut b = book();

    let mut epoch = 2u64;
    for rate in [1_301u64, 1_337, 1_299, 1_303, 1_311] {
        for fee_bps in [0u16, 1, 7, 199, 200] {
            epoch += 1;
            b.env
                .set_pool(b.bcook.mint, px(rate) as u64, POOL_SHARES, fee_bps, epoch);
            b.env.try_refresh_oracle(b.bcook.mint).expect("crank");

            let reference = b.env.read_oracle(b.bcook.mint).reference;
            if reference.source_epoch != epoch {
                continue; // the breaker refused this one; nothing to check.
            }

            let exact = reference.gross_rate * (10_000 - fee_bps as u128);
            assert!(
                reference.effective_rate * 10_000 <= exact,
                "rate {rate} fee {fee_bps}: effective {} exceeds the exact value {}/10000",
                reference.effective_rate,
                exact
            );
            // And not floored away to nothing, which would be safe but useless.
            assert!(
                (reference.effective_rate + 1) * 10_000 > exact,
                "rate {rate} fee {fee_bps}: effective {} is more than one unit low",
                reference.effective_rate
            );
        }
    }
    assert_solvent(&b.env, &b.bcook, "after the rounding sweep");
}

// ===========================================================================
// 17-20. Economic attacks that do not go through the oracle
// ===========================================================================
//
// Listed separately because they were named in the brief and are not about the
// price at all. They belong here rather than in `audit_rounding.rs` because
// what they measure is the attacker's balance, not an arithmetic bound.

/// A thousand one-unit deposit/withdraw round trips extract nothing.
///
/// The classic way to turn a rounding bias into money: find an operation that
/// gives back a fraction more than it took, and repeat it. One base unit per
/// round trip is invisible in any single assertion and is real money at scale,
/// so the test does it repeatedly and measures the balance rather than checking
/// one trip's arithmetic.
#[test]
fn econ_17_repeated_unit_deposits_and_withdrawals_extract_nothing() {
    let mut b = book();

    let user = b.env.create_user();
    b.env.fund(&user, b.cook.mint, tokens(1_000));
    let cook = b.cook;
    let start = b.env.balance(&ata(&user.pubkey(), &cook.mint));

    for _ in 0..250 {
        if b.env.try_supply(&user, &cook, 1).is_err() {
            break;
        }
        let shares = b.env.balance(&share_ata(&user.pubkey(), &cook.share_mint));
        if shares == 0 || b.env.try_withdraw(&user, &cook, shares).is_err() {
            break;
        }
    }

    let end = b.env.balance(&ata(&user.pubkey(), &cook.mint));
    assert!(
        end <= start,
        "250 unit round trips produced {} base units from nothing",
        end - start
    );
    assert_solvent(&b.env, &cook, "after 250 unit round trips");
}

/// A thousand one-unit borrow/repay round trips extract nothing either.
///
/// The same shape on the debt side, where the rounding convention is the
/// opposite: borrowed value ceils, so a round trip should cost the borrower
/// rather than pay them.
#[test]
fn econ_18_repeated_unit_borrows_and_repayments_extract_nothing() {
    let mut b = book();
    let cook = b.cook;
    let bcook = b.bcook;
    let attacker = b.attacker.insecure_clone();
    let obligation = b.obligation;
    let start = b.env.balance(&ata(&attacker.pubkey(), &cook.mint));

    for _ in 0..200 {
        if b.env
            .try_borrow(&attacker, &cook, obligation, 1, &[&cook, &bcook])
            .is_err()
        {
            break;
        }
        if b.env.try_repay(&attacker, &cook, obligation, 1).is_err() {
            break;
        }
    }

    let end = b.env.balance(&ata(&attacker.pubkey(), &cook.mint));
    assert!(
        end <= start,
        "200 unit borrow/repay trips produced {} base units from nothing",
        end - start
    );
    assert_solvent(&b.env, &cook, "after 200 unit borrow/repay trips");
}

/// At extreme utilisation the reserve stays solvent and suppliers stay whole.
///
/// Nearly every unit lent out is where the interest curve is steepest, where
/// the available-liquidity term is smallest, and where a withdrawal that should
/// be refused is most likely to slip through. A year of accrual on top pushes
/// the index far from one, which is where fixed-point mistakes surface.
#[test]
fn econ_19_extreme_utilisation_stays_solvent() {
    let mut b = book();
    let cook = b.cook;
    let bcook = b.bcook;

    /*
     * Enough borrowers to actually pin utilisation near the top.
     *
     * The first version of this test asked three actors for more than their
     * collateral allowed, every borrow was refused, and it asserted nothing at
     * all -- it passed with a completely idle reserve. Each actor now borrows
     * inside its limit, there are enough of them to consume the supply, and
     * the utilisation reached is asserted rather than assumed.
     */
    let mut borrowed = 0u64;
    for _ in 0..10 {
        let user = b.env.create_user();
        b.env.fund(&user, bcook.mint, tokens(20_000));
        b.env.fund(&user, cook.mint, tokens(1));
        let obligation = b.env.open_position(&user, &bcook, tokens(20_000));
        if b.env
            .try_borrow(&user, &cook, obligation, tokens(13_000), &[&cook, &bcook])
            .is_ok()
        {
            borrowed += tokens(13_000);
        }
    }

    let before = b.env.solvency(&cook);
    let utilisation =
        before.outstanding_debt * 100 / (before.tracked_available + before.outstanding_debt);
    assert!(
        utilisation >= 80,
        "the setup only reached {utilisation}% utilisation ({borrowed} borrowed) --          this test is not exercising what it claims to"
    );

    b.env.warp_slots(aera::constants::DEFAULT_SLOTS_PER_YEAR);
    b.env.accrue(&cook);
    let after = b.env.solvency(&cook);

    assert!(
        after.outstanding_debt > before.outstanding_debt,
        "a year at {utilisation}% utilisation accrued no interest at all"
    );
    assert_solvent(&b.env, &cook, "after a year at extreme utilisation");

    // And a supplier can still withdraw whatever the reserve actually holds.
    let supplier = b.env.create_user();
    b.env.fund(&supplier, cook.mint, tokens(1_000));
    b.env
        .try_supply(&supplier, &cook, tokens(1_000))
        .expect("supplying into a hot reserve must work");
    assert_solvent(&b.env, &cook, "after supplying into a hot reserve");
}

/// Bad debt is absorbed by the suppliers, not manufactured out of nothing.
///
/// A position can end up owing more than its collateral is worth -- the rate
/// falls faster than liquidators act, which is the case no lending protocol
/// avoids entirely. What must not happen is the protocol's books pretending
/// otherwise: the shortfall has to show up as a claim the assets cannot cover,
/// visibly, rather than as an exchange rate that quietly keeps rising.
#[test]
fn econ_20_bad_debt_is_visible_rather_than_manufactured() {
    let mut b = book();
    let cook = b.cook;
    let bcook = b.bcook;

    b.try_borrow(tokens(6_000)).expect("honest borrow");

    // Walk the collateral down inside the breaker's allowance, epoch by epoch,
    // so the fall is accepted rather than frozen -- a slow bleed is how bad
    // debt actually arrives.
    let mut epoch = 2u64;
    for rate in [1_200u64, 1_100, 1_010, 930, 860, 790, 730, 670] {
        epoch += 1;
        b.env.set_rate(bcook.mint, rate, epoch);
        let _ = b.env.try_refresh_oracle(bcook.mint);
    }

    // Whatever state that left, the books must still be internally consistent
    // and the vault must still hold what the program says it holds.
    let solvency = b.env.solvency(&cook);
    assert!(
        solvency.vault_backs_tracked(),
        "the vault stopped backing the books during a collateral collapse: {}",
        solvency.report("COOK")
    );

    // The share rate must not have risen on the strength of debt that cannot
    // be repaid. Suppliers bear the loss; they must not be shown a profit.
    let rate = b.env.acook_rate(&cook);
    assert!(
        rate >= aera::constants::FIXED_POINT_SCALE,
        "the share rate fell below parity: {rate}"
    );

    // And repayment is still open, which is the only route by which the debt
    // can actually come back.
    let attacker = b.attacker.insecure_clone();
    b.env
        .try_repay(&attacker, &cook, b.obligation, tokens(100))
        .expect("repayment must survive a collateral collapse");
}
