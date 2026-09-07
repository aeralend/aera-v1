//! The two trust assumptions the oracle cannot derive away, and what bounds them.
//!
//! `test_oracle.rs` proves the rate is derived rather than chosen. That leaves
//! two things a derivation cannot settle by itself, both of which were found by
//! reading Cookie Chain rather than by reasoning about it:
//!
//! 1. **The source program can be replaced.** `GZgs5uREPp6BvDt8eysmhavQPAHBAtjePgV4zfhgd9pH`
//!    is owned by the upgradeable loader and its upgrade authority is
//!    `GSPUoahS7jSQUEAEkjaejsN9vo2w4B2NYHZ9oJSMm45p` -- a system-owned account
//!    with zero bytes of data, i.e. a single wallet key, not a multisig.
//!    Whoever holds that key can deploy new code behind the same program id.
//!    Owner validation would not notice: the account would still be owned by
//!    the same program id, the layout could be identical, and the meaning of
//!    the bytes could be anything.
//!
//!    Hashing the deployed ELF on chain is not available -- it is 427,112 bytes
//!    against a 1.4M compute budget -- so what is pinned instead is the
//!    deployment: the `ProgramData` slot and upgrade authority, which change on
//!    exactly the events that matter and cost one account read.
//!
//! 2. **The first observation has nothing to check itself against.** The
//!    movement breaker needs a reference. Before there is one, the only guards
//!    are the absolute floor and ceiling, which are deliberately wide. So the
//!    moment an oracle is configured -- a moment an operator picks -- is the
//!    moment an attacker would want to arrange the source. What that first
//!    reading is checked against instead is the pool's own published previous
//!    epoch, and the oracle is refused the right to open new risk until a later
//!    epoch of the source confirms it.
//!
//! Neither is eliminated. Both are bounded, and every bound below is a test.

mod common;

use aera::constants::{DEFAULT_MAX_WITHDRAWAL_FEE_BPS, DEFAULT_RATE_CEILING, DEFAULT_RATE_FLOOR};
use aera::instructions::admin::init_oracle::OracleConfig;
use aera::oracle::breaker::OracleHealth;
use aera::oracle::deployment::NO_UPGRADE_AUTHORITY;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use anchor_lang::{InstructionData, ToAccountMetas};
use common::*;
use solana_keypair::Keypair;

fn health_of(env: &Env, mint: Pubkey) -> OracleHealth {
    OracleHealth::from_u8(env.read_oracle(mint).health).unwrap()
}

/// The launch configuration for a mint's native oracle, pinned to `slot`.
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

/// Crank the oracle and assert the observation was refused.
///
/// A refused observation does **not** fail the transaction, and that is
/// deliberate: a crank that aborted could not record the freeze, so a broken
/// source would leave the oracle looking healthy until somebody noticed. The
/// refusal is recorded instead -- EMERGENCY, and the last trusted reference
/// left exactly where it was so liquidation still has a price to work from.
///
/// Which check fired is in the transaction log rather than in the state, since
/// all of them need the same response; `deployment.rs`'s unit tests pin the
/// exact error each one returns.
fn assert_observation_refused(env: &mut Env, mint: Pubkey, why: &str) {
    let before = env.read_oracle(mint).reference;

    env.try_refresh_oracle(mint)
        .expect("a refused observation must still record the freeze, not abort");

    let after = env.read_oracle(mint);
    assert_eq!(
        OracleHealth::from_u8(after.health).unwrap(),
        OracleHealth::Emergency,
        "{why}"
    );
    assert_eq!(
        after.reference, before,
        "a refused observation moved the reference: {why}"
    );
}

/// A borrower with bCOOK collateral and a little debt, in a funded market.
fn market_with_borrower() -> (Env, ReserveHandle, ReserveHandle, Keypair, Pubkey) {
    let (mut env, cook, bcook) = Env::core(1_300);

    let supplier = env.create_user();
    env.fund(&supplier, cook.mint, tokens(50_000));
    env.supply(&supplier, &cook, tokens(50_000));

    let borrower = env.create_user();
    // A thousand bCOOK more than the position needs, supplied but not pledged,
    // so the tests below have shares in hand to add as collateral.
    env.fund(&borrower, bcook.mint, tokens(11_000));
    env.fund(&borrower, cook.mint, tokens(1_000));
    let obligation = env.open_position(&borrower, &bcook, tokens(10_000));
    env.supply(&borrower, &bcook, tokens(1_000));
    env.try_borrow(
        &borrower,
        &cook,
        obligation,
        tokens(1_000),
        &[&cook, &bcook],
    )
    .expect("setup borrow");

    (env, cook, bcook, borrower, obligation)
}

// ===========================================================================
// 1. The deployment pin
// ===========================================================================

/// The baseline: an unchanged deployment is invisible.
///
/// Worth asserting on its own. A check that refuses everything is not a check,
/// and every test below is only meaningful because this one passes.
#[test]
fn pin_00_an_unchanged_deployment_is_not_noticed() {
    let (mut env, _cook, bcook) = Env::core(1_300);

    env.move_rate(bcook.mint, 1_310, 2);

    assert_eq!(health_of(&env, bcook.mint), OracleHealth::Healthy);
    assert_eq!(
        env.read_oracle(bcook.mint).reference.gross_rate,
        px(1_310) as u128
    );
}

/// A redeploy stops every observation, immediately.
///
/// This is the whole point. The bytes at the pool address are unchanged and
/// still owned by the same program id -- only the code behind that id has been
/// replaced. Owner validation cannot see that. The pin can.
#[test]
fn pin_01_a_redeploy_refuses_every_observation() {
    let (mut env, _cook, bcook) = Env::core(1_300);

    env.set_program_data(TEST_DEPLOY_SLOT + 1, Some(TEST_UPGRADE_AUTHORITY));
    env.set_rate(bcook.mint, 1_310, 2);

    assert_observation_refused(
        &mut env,
        bcook.mint,
        "a redeployed source program was still read",
    );
    assert_eq!(
        env.read_oracle(bcook.mint).reference.gross_rate,
        px(1_300) as u128,
        "the post-redeploy rate reached the reference"
    );
}

/// A transferred upgrade authority is refused, even at the same slot.
///
/// The code has not changed yet. Somebody else can now change it, which is a
/// different fact about the same program and worth stopping for on its own --
/// waiting for the redeploy would mean acting after the fact.
#[test]
fn pin_02_a_transferred_authority_is_refused() {
    let (mut env, _cook, bcook) = Env::core(1_300);

    env.set_program_data(TEST_DEPLOY_SLOT, Some(Pubkey::new_unique()));
    env.set_rate(bcook.mint, 1_310, 2);

    assert_observation_refused(
        &mut env,
        bcook.mint,
        "the source program changed hands unnoticed",
    );
}

/// Revoking the authority is allowed. It can only reduce the risk.
///
/// An immutable program is strictly safer than the one that was reviewed: the
/// same code, and nobody able to replace it. Refusing this would punish the
/// operator for doing the one thing that removes the assumption entirely.
#[test]
fn pin_03_revoking_the_authority_is_allowed() {
    let (mut env, _cook, bcook) = Env::core(1_300);

    env.set_program_data(TEST_DEPLOY_SLOT, None);
    env.move_rate(bcook.mint, 1_310, 2);

    assert_eq!(
        health_of(&env, bcook.mint),
        OracleHealth::Healthy,
        "making the source immutable must not freeze the market"
    );
}

/// A pin recorded against an already-immutable program keeps working.
///
/// The zero pubkey is the sentinel for "no authority". This checks the sentinel
/// round-trips, rather than accidentally matching a real key of all zeros or
/// failing to match `None`.
#[test]
fn pin_04_an_immutable_program_can_be_pinned_from_the_start() {
    let (mut env, _cook, bcook) = Env::core(1_300);

    env.set_program_data(TEST_DEPLOY_SLOT, None);
    env.set_oracle_with(
        bcook.mint,
        native_config(&env, bcook.mint, TEST_DEPLOY_SLOT, NO_UPGRADE_AUTHORITY),
    );
    env.set_pool(
        bcook.mint,
        px(1_300) as u64,
        POOL_SHARES,
        TEST_WITHDRAWAL_FEE_BPS,
        2,
    );
    env.refresh_oracle(bcook.mint);

    assert!(
        env.read_oracle(bcook.mint).reference.is_set(),
        "an immutable source could not be observed at all"
    );
    env.confirm_bootstrap(bcook.mint);
    assert_eq!(
        health_of(&env, bcook.mint),
        OracleHealth::Healthy,
        "an immutable source must reach a working market"
    );
}

/// Redeploying and *then* revoking does not launder the redeploy.
///
/// The most tempting way around the pin: end in the state the revocation rule
/// permits. It does not work, because the two facts are checked separately and
/// the slot has moved. If this passed, an attacker could deploy anything and
/// then throw the key away to make it permanent.
#[test]
fn pin_05_redeploying_then_revoking_is_still_refused() {
    let (mut env, _cook, bcook) = Env::core(1_300);

    env.set_program_data(TEST_DEPLOY_SLOT + 1, None);
    env.set_rate(bcook.mint, 1_310, 2);

    assert_observation_refused(
        &mut env,
        bcook.mint,
        "a redeploy was laundered through a revocation",
    );
}

/// A `ProgramData` account at the wrong address is refused.
///
/// The address is a PDA of the source program id under the loader, so it cannot
/// be substituted. Deriving it in the program rather than trusting the caller
/// is what makes the pin unforgeable by whoever builds the transaction.
#[test]
fn pin_06_a_substituted_program_data_account_is_refused() {
    let (mut env, _cook, bcook) = Env::core(1_300);

    // A perfectly well-formed ProgramData, saying exactly what the pin expects
    // -- at an address the loader would never put it.
    let impostor = Pubkey::new_unique();
    env.svm
        .set_account(
            impostor,
            solana_account::Account {
                lamports: 1_000_000_000,
                data: program_data_bytes(TEST_DEPLOY_SLOT, Some(TEST_UPGRADE_AUTHORITY)),
                owner: BPF_LOADER_UPGRADEABLE,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();

    // And the real one moved, so passing the impostor is the only way through.
    env.set_program_data(TEST_DEPLOY_SLOT + 1, Some(TEST_UPGRADE_AUTHORITY));
    env.set_rate(bcook.mint, 1_310, 2);

    let mut accounts = aera::accounts::RefreshOracle {
        oracle: env.oracle_address(bcook.mint),
    }
    .to_account_metas(None);
    accounts.push(AccountMeta::new_readonly(
        env.stake_pool_address(bcook.mint),
        false,
    ));
    accounts.push(AccountMeta::new_readonly(impostor, false));

    let admin = env.admin.insecure_clone();
    let before = env.read_oracle(bcook.mint).reference;
    env.send_raw(
        vec![Instruction {
            program_id: aera::id(),
            accounts,
            data: aera::instruction::RefreshOracle {}.data(),
        }],
        &[&admin],
    )
    .expect("the crank must record the refusal rather than abort");

    let after = env.read_oracle(bcook.mint);
    assert_eq!(
        OracleHealth::from_u8(after.health).unwrap(),
        OracleHealth::Emergency,
        "a forged ProgramData account was accepted"
    );
    assert_eq!(
        after.reference, before,
        "a forged ProgramData let a new rate through"
    );
}

/// A `ProgramData` account not owned by the loader is refused.
#[test]
fn pin_07_program_data_must_be_owned_by_the_loader() {
    let (mut env, _cook, bcook) = Env::core(1_300);

    let key = env.stake_pool_program_data();
    env.svm
        .set_account(
            key,
            solana_account::Account {
                lamports: 1_000_000_000,
                data: program_data_bytes(TEST_DEPLOY_SLOT, Some(TEST_UPGRADE_AUTHORITY)),
                owner: Pubkey::new_unique(), // not the upgradeable loader
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
    env.set_rate(bcook.mint, 1_310, 2);

    assert_observation_refused(
        &mut env,
        bcook.mint,
        "a ProgramData owned by a stranger was accepted",
    );
}

/// A `Program` account passed where `ProgramData` belongs is refused.
///
/// Both are owned by the loader and both parse as the same bincode enum, so the
/// discriminant is the only thing separating them. Reading a `Program` account
/// as `ProgramData` would take its 32-byte pointer as a slot and an authority.
#[test]
fn pin_08_the_wrong_loader_variant_is_refused() {
    let (mut env, _cook, bcook) = Env::core(1_300);

    let key = env.stake_pool_program_data();
    // UpgradeableLoaderState::Program { programdata_address }, variant 2.
    let mut data = vec![0u8; 36];
    data[0..4].copy_from_slice(&2u32.to_le_bytes());
    data[4..36].copy_from_slice(key.as_ref());
    env.svm
        .set_account(
            key,
            solana_account::Account {
                lamports: 1_000_000_000,
                data,
                owner: BPF_LOADER_UPGRADEABLE,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
    env.set_rate(bcook.mint, 1_310, 2);

    assert_observation_refused(
        &mut env,
        bcook.mint,
        "a Program account was read as ProgramData",
    );
}

/// A truncated `ProgramData` account is refused rather than read short.
#[test]
fn pin_09_a_truncated_program_data_is_refused() {
    let (mut env, _cook, bcook) = Env::core(1_300);

    let key = env.stake_pool_program_data();
    let mut data = program_data_bytes(TEST_DEPLOY_SLOT, Some(TEST_UPGRADE_AUTHORITY));
    data.truncate(20); // past the slot, into the authority
    env.svm
        .set_account(
            key,
            solana_account::Account {
                lamports: 1_000_000_000,
                data,
                owner: BPF_LOADER_UPGRADEABLE,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
    env.set_rate(bcook.mint, 1_310, 2);

    assert_observation_refused(
        &mut env,
        bcook.mint,
        "a truncated ProgramData was read anyway",
    );
}

/// A redeploy freezes new borrowing but leaves every exit open.
///
/// The rule that stops the market is only defensible if it does not trap
/// anybody inside it. A borrower who cannot repay during a freeze has been made
/// worse off by the protection.
#[test]
fn pin_10_a_redeploy_freezes_borrowing_but_not_repayment() {
    let (mut env, cook, bcook, borrower, obligation) = market_with_borrower();

    env.set_program_data(TEST_DEPLOY_SLOT + 1, Some(TEST_UPGRADE_AUTHORITY));

    // The observation is refused, so the oracle keeps its last good reading.
    // Everything below is priced against that -- stale, but honest and bounded.
    assert_observation_refused(&mut env, bcook.mint, "the redeploy went unnoticed");

    let borrowed = env.try_borrow(&borrower, &cook, obligation, tokens(100), &[&cook, &bcook]);
    assert!(
        borrowed.is_err(),
        "new debt was opened against a redeployed source"
    );

    env.try_repay(&borrower, &cook, obligation, tokens(100))
        .expect("repayment must survive a redeploy");
    env.try_deposit_collateral(&borrower, &bcook, obligation, tokens(1))
        .expect("adding collateral must survive a redeploy");
}

/// A reset cannot wave a redeployed program through.
///
/// `reset_oracle_breaker` is the admin's tool for clearing a freeze, and it
/// re-anchors from a live reading -- so if it skipped the pin it would be a
/// one-instruction bypass of everything above. Clearing a redeploy has to be a
/// deliberate reconfiguration, not a reset.
#[test]
fn pin_11_a_breaker_reset_cannot_clear_a_redeploy() {
    let (mut env, _cook, bcook) = Env::core(1_300);

    env.set_program_data(TEST_DEPLOY_SLOT + 1, Some(TEST_UPGRADE_AUTHORITY));
    env.set_rate(bcook.mint, 1_310, 2);

    let message = env
        .try_reset_breaker(bcook.mint)
        .expect_err("a reset cleared a redeploy");
    assert!(
        message.contains("OracleProgramUpgraded"),
        "the reset must still enforce the pin: {message}"
    );
}

/// Re-authorising the new deployment restores service -- and starts over.
///
/// This is the intended recovery: a human reads the new code, decides it is
/// still the program Aera thought it was reading, and records the new slot.
/// Because that is a fresh configuration, the oracle bootstraps again rather
/// than resuming on the old reference's authority.
#[test]
fn pin_12_reauthorising_the_new_deployment_restores_service() {
    let (mut env, _cook, bcook) = Env::core(1_300);

    let new_slot = TEST_DEPLOY_SLOT + 1;
    env.set_program_data(new_slot, Some(TEST_UPGRADE_AUTHORITY));
    env.set_rate(bcook.mint, 1_310, 2);
    assert_observation_refused(&mut env, bcook.mint, "the redeploy went unnoticed");

    env.set_oracle_with(
        bcook.mint,
        native_config(&env, bcook.mint, new_slot, TEST_UPGRADE_AUTHORITY),
    );
    env.refresh_oracle(bcook.mint);

    assert_eq!(
        health_of(&env, bcook.mint),
        OracleHealth::Bootstrapping,
        "a re-authorised deployment must earn trust again, not inherit it"
    );

    env.confirm_bootstrap(bcook.mint);
    assert_eq!(health_of(&env, bcook.mint), OracleHealth::Healthy);
}

/// The unit-of-account oracle reads no program, so it has nothing to pin.
///
/// COOK's rate is the constant 1, fixed in this program's own code. Requiring a
/// deployment pin for it would mean inventing a program for it to point at.
#[test]
fn pin_13_the_unit_oracle_needs_no_deployment() {
    let (mut env, cook, _bcook) = Env::core(1_300);

    env.set_program_data(TEST_DEPLOY_SLOT + 999, Some(Pubkey::new_unique()));
    env.svm.expire_blockhash();
    env.refresh_oracle(cook.mint);

    assert_eq!(
        health_of(&env, cook.mint),
        OracleHealth::Healthy,
        "the quote asset must not be frozen by somebody else's redeploy"
    );
}

// ===========================================================================
// 2. The bootstrap
// ===========================================================================

/// A brand-new native oracle is provisional, not trusted.
#[test]
fn boot_00_a_first_observation_leaves_the_oracle_bootstrapping() {
    let mut env = Env::new();
    let mint = env.create_mint(DECIMALS);

    env.set_oracle_with(
        mint,
        native_config(&env, mint, TEST_DEPLOY_SLOT, TEST_UPGRADE_AUTHORITY),
    );
    env.set_pool(
        mint,
        px(1_300) as u64,
        POOL_SHARES,
        TEST_WITHDRAWAL_FEE_BPS,
        7,
    );
    env.refresh_oracle(mint);

    let oracle = env.read_oracle(mint);
    assert!(
        oracle.reference.is_set(),
        "the first reading must still anchor -- existing positions need a price"
    );
    assert_eq!(
        OracleHealth::from_u8(oracle.health).unwrap(),
        OracleHealth::Bootstrapping
    );
}

/// A second reading in the *same* epoch confirms nothing.
///
/// Within one epoch a stake pool's figures do not move, so a second reading
/// says only that the bytes have not changed in the last few seconds. If a
/// later slot were enough, an attacker who arranged the pool could confirm
/// their own anchor in the next transaction.
#[test]
fn boot_01_a_second_reading_in_the_same_epoch_does_not_confirm() {
    let (mut env, _cook, bcook) = fresh_bootstrapping_market();

    env.refresh_oracle(bcook.mint);
    env.refresh_oracle(bcook.mint);

    assert_eq!(
        health_of(&env, bcook.mint),
        OracleHealth::Bootstrapping,
        "cranking twice in one epoch must not buy trust"
    );
}

/// A reading in a later epoch confirms the anchor.
#[test]
fn boot_02_a_later_epoch_confirms_the_anchor() {
    let (mut env, _cook, bcook) = fresh_bootstrapping_market();

    env.confirm_bootstrap(bcook.mint);

    assert_eq!(health_of(&env, bcook.mint), OracleHealth::Healthy);
}

/// While bootstrapping, no new debt and no collateral leaves the protocol.
///
/// Those are the two actions that increase risk against a price. Everything
/// else is either risk-reducing or risk-neutral, and stays open.
#[test]
fn boot_03_bootstrapping_blocks_the_risk_increasing_actions() {
    let (mut env, cook, bcook, borrower, obligation) = market_with_borrower();

    // Re-authorise bCOOK's source under a new deployment, which is what puts
    // the oracle back to a first observation it has not yet earned trust in.
    rebootstrap(&mut env, bcook.mint);
    assert_eq!(health_of(&env, bcook.mint), OracleHealth::Bootstrapping);

    let borrowed = env.try_borrow(&borrower, &cook, obligation, tokens(100), &[&cook, &bcook]);
    assert!(
        borrowed
            .as_ref()
            .err()
            .is_some_and(|m| m.contains("OracleBorrowFrozen")),
        "borrowing was permitted against an unconfirmed anchor: {borrowed:?}"
    );

    let withdrawn =
        env.try_withdraw_collateral(&borrower, &bcook, obligation, tokens(1), &[&cook, &bcook]);
    assert!(
        withdrawn.is_err(),
        "collateral left the protocol against an unconfirmed anchor"
    );
}

/// While bootstrapping, every exit stays open.
#[test]
fn boot_04_bootstrapping_leaves_the_exits_open() {
    let (mut env, cook, bcook, borrower, obligation) = market_with_borrower();

    rebootstrap(&mut env, bcook.mint);
    assert_eq!(health_of(&env, bcook.mint), OracleHealth::Bootstrapping);

    env.try_repay(&borrower, &cook, obligation, tokens(100))
        .expect("repayment must stay open while bootstrapping");
    env.try_deposit_collateral(&borrower, &bcook, obligation, tokens(1))
        .expect("adding collateral must stay open while bootstrapping");

    let supplier = env.create_user();
    env.fund(&supplier, cook.mint, tokens(100));
    env.try_supply(&supplier, &cook, tokens(100))
        .expect("supplying must stay open while bootstrapping");
}

/// A first reading the pool's own history cannot explain is refused outright.
///
/// This is the attack the bootstrap exists for: arrange the pool at the moment
/// the oracle is configured, and the reference becomes whatever you chose. The
/// pool publishes `last_epoch_total_lamports` and `last_epoch_pool_token_supply`
/// alongside the current figures, so a rate far from its own previous epoch is
/// self-contradictory -- and the contradiction is in bytes the same attacker
/// would have to forge consistently.
#[test]
fn boot_05_an_anchor_the_pools_own_history_denies_is_refused() {
    let mut env = Env::new();
    let mint = env.create_mint(DECIMALS);

    env.set_oracle_with(
        mint,
        native_config(&env, mint, TEST_DEPLOY_SLOT, TEST_UPGRADE_AUTHORITY),
    );

    // The pool says it is worth 3.0 now, and says it was worth 1.3 an epoch
    // ago. No stake pool moves 130% in an epoch.
    env.set_pool_with_history(
        mint,
        px(3_000) as u64,
        POOL_SHARES,
        TEST_WITHDRAWAL_FEE_BPS,
        7,
        Some((px(1_300) as u64, POOL_SHARES)),
    );

    env.try_refresh_oracle(mint)
        .expect("the crank must record the refusal rather than abort");
    let oracle = env.read_oracle(mint);
    assert_eq!(
        OracleHealth::from_u8(oracle.health).unwrap(),
        OracleHealth::Emergency,
        "an anchor contradicted by the pool's own history was accepted"
    );
    assert!(
        !oracle.reference.is_set(),
        "a refused bootstrap must leave no reference behind"
    );
}

/// The same check works downwards.
///
/// Understating bCOOK is not harmless during a bootstrap: it is how you would
/// set up a market in which everyone is instantly liquidatable.
#[test]
fn boot_06_a_collapsed_anchor_is_refused_too() {
    let mut env = Env::new();
    let mint = env.create_mint(DECIMALS);

    env.set_oracle_with(
        mint,
        native_config(&env, mint, TEST_DEPLOY_SLOT, TEST_UPGRADE_AUTHORITY),
    );
    env.set_pool_with_history(
        mint,
        px(1_100) as u64,
        POOL_SHARES,
        TEST_WITHDRAWAL_FEE_BPS,
        7,
        Some((px(2_000) as u64, POOL_SHARES)),
    );

    env.try_refresh_oracle(mint)
        .expect("the crank must record the refusal rather than abort");
    let oracle = env.read_oracle(mint);
    assert_eq!(
        OracleHealth::from_u8(oracle.health).unwrap(),
        OracleHealth::Emergency,
        "a collapsed anchor was accepted"
    );
    assert!(
        !oracle.reference.is_set(),
        "a refused bootstrap must leave no reference behind"
    );
}

/// An ordinary epoch's movement is accepted, so the check is not merely strict.
///
/// The live pool moved 0.33% over its last epoch, measured on Cookie Chain. A
/// bound that rejected that would have frozen the real market at launch.
#[test]
fn boot_07_a_normal_epochs_movement_bootstraps_cleanly() {
    let mut env = Env::new();
    let mint = env.create_mint(DECIMALS);

    env.set_oracle_with(
        mint,
        native_config(&env, mint, TEST_DEPLOY_SLOT, TEST_UPGRADE_AUTHORITY),
    );
    // 1.3000 now, 1.29571 an epoch ago: 0.33%, the figure the live pool showed.
    env.set_pool_with_history(
        mint,
        px(1_300) as u64,
        POOL_SHARES,
        TEST_WITHDRAWAL_FEE_BPS,
        7,
        Some((1_295_710_000_000_000_000, POOL_SHARES)),
    );

    env.refresh_oracle(mint);
    assert_eq!(
        OracleHealth::from_u8(env.read_oracle(mint).health).unwrap(),
        OracleHealth::Bootstrapping,
        "a realistic epoch of movement must be accepted, if unconfirmed"
    );
    assert_eq!(
        env.read_oracle(mint).reference.gross_rate,
        px(1_300) as u128
    );
}

/// A pool with no history bootstraps, but stays unconfirmed.
///
/// A pool that has never completed an epoch has genuinely nothing to check
/// itself against. That is not a reason to refuse -- it is exactly what a newly
/// launched stake pool looks like -- but it is a reason not to trust it yet,
/// which is what `Bootstrapping` already means.
#[test]
fn boot_08_a_pool_with_no_history_still_has_to_wait() {
    let mut env = Env::new();
    let mint = env.create_mint(DECIMALS);

    env.set_oracle_with(
        mint,
        native_config(&env, mint, TEST_DEPLOY_SLOT, TEST_UPGRADE_AUTHORITY),
    );
    env.set_pool_with_history(
        mint,
        px(1_300) as u64,
        POOL_SHARES,
        TEST_WITHDRAWAL_FEE_BPS,
        7,
        None,
    );

    env.refresh_oracle(mint);
    assert_eq!(
        OracleHealth::from_u8(env.read_oracle(mint).health).unwrap(),
        OracleHealth::Bootstrapping,
        "a pool with no history must not be trusted on its own word"
    );
}

/// Reconfiguring an oracle drops it back to bootstrapping.
///
/// Otherwise the whole mechanism is one `set_oracle` away from being skipped:
/// point the oracle at a pool you control, inherit the old health, and borrow.
#[test]
fn boot_09_reconfiguring_an_oracle_starts_the_bootstrap_over() {
    let (mut env, _cook, bcook) = Env::core(1_300);
    assert_eq!(health_of(&env, bcook.mint), OracleHealth::Healthy);

    rebootstrap(&mut env, bcook.mint);

    assert_eq!(
        health_of(&env, bcook.mint),
        OracleHealth::Bootstrapping,
        "a re-authorised deployment inherited trust it had not earned"
    );

    // And an identical reconfiguration -- same source, same pin -- keeps the
    // reference, because nothing it was a statement about has changed.
    let (mut env, _cook, bcook) = Env::core(1_300);
    env.set_oracle_with(
        bcook.mint,
        native_config(&env, bcook.mint, TEST_DEPLOY_SLOT, TEST_UPGRADE_AUTHORITY),
    );
    env.refresh_oracle(bcook.mint);
    assert_eq!(
        health_of(&env, bcook.mint),
        OracleHealth::Healthy,
        "re-stating the same configuration must not cost an epoch of borrowing"
    );
}

/// A confirmed oracle does not fall back to bootstrapping on a later move.
///
/// `Bootstrapping` is a starting state, not a recurring one. Once confirmed,
/// later readings are the movement breaker's business, and a move large enough
/// to matter must be reported as what it is rather than as a fresh start.
#[test]
fn boot_10_confirmation_is_not_undone_by_a_later_move() {
    let (mut env, _cook, bcook) = Env::core(1_300);

    env.move_rate(bcook.mint, 1_310, 2);
    assert_eq!(health_of(&env, bcook.mint), OracleHealth::Healthy);

    // A move large enough to trip the breaker: an emergency, not a bootstrap.
    env.set_rate(bcook.mint, 3_000, 3);
    let _ = env.try_refresh_oracle(bcook.mint);
    assert_ne!(
        health_of(&env, bcook.mint),
        OracleHealth::Bootstrapping,
        "a confirmed oracle must not be able to re-enter bootstrapping"
    );
}

/// A market can be launched, confirmed, and used -- the whole intended path.
///
/// Every test above is a refusal. This one is the sequence a real operator
/// runs, so that "nothing works" cannot pass for "everything is safe".
#[test]
fn boot_11_the_intended_launch_sequence_works_end_to_end() {
    let (mut env, cook, bcook) = fresh_bootstrapping_market();

    let supplier = env.create_user();
    env.fund(&supplier, cook.mint, tokens(50_000));
    env.supply(&supplier, &cook, tokens(50_000));

    let borrower = env.create_user();
    env.fund(&borrower, bcook.mint, tokens(10_000));
    // A COOK balance, so the borrow has somewhere to land.
    env.fund(&borrower, cook.mint, tokens(1));
    let obligation = env.open_position(&borrower, &bcook, tokens(10_000));

    let early = env.try_borrow(&borrower, &cook, obligation, tokens(100), &[&cook, &bcook]);
    assert!(early.is_err(), "borrowed before the anchor was confirmed");

    env.confirm_bootstrap(bcook.mint);

    env.try_borrow(
        &borrower,
        &cook,
        obligation,
        tokens(1_000),
        &[&cook, &bcook],
    )
    .expect("the market must open once the anchor is confirmed");
}

/// Put a mint's oracle back to a first observation, as a real reconfiguration
/// would.
///
/// Re-authorising the source under a new deployment pin is the honest way to
/// get there: `set_oracle` keeps the reference when nothing about the source
/// changed, so passing an identical configuration would leave the old anchor in
/// place and prove nothing.
fn rebootstrap(env: &mut Env, mint: Pubkey) {
    let new_slot = TEST_DEPLOY_SLOT + 1;
    env.set_program_data(new_slot, Some(TEST_UPGRADE_AUTHORITY));
    env.set_oracle_with(
        mint,
        native_config(env, mint, new_slot, TEST_UPGRADE_AUTHORITY),
    );
    env.refresh_oracle(mint);
}

/// A market whose bCOOK oracle has been reconfigured and not yet confirmed.
fn fresh_bootstrapping_market() -> (Env, ReserveHandle, ReserveHandle) {
    let (mut env, cook, bcook) = Env::core(1_300);
    rebootstrap(&mut env, bcook.mint);
    assert_eq!(
        health_of(&env, bcook.mint),
        OracleHealth::Bootstrapping,
        "the fixture did not produce a bootstrapping oracle"
    );
    (env, cook, bcook)
}
