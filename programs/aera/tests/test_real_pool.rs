//! The oracle, against the real account.
//!
//! Every other oracle test drives a *synthetic* stake pool: the harness writes
//! bytes in the layout it believes the pool uses, and the program reads them
//! back. That proves the program agrees with the harness. It does not prove
//! either of them agrees with Cookie Chain -- and if both held the same wrong
//! offset, the entire suite would pass while the protocol mispriced every
//! position on the live network.
//!
//! `fixtures/bcook_stake_pool.bin` is the actual account, fetched from
//! `rpc.cookiescan.io` and committed verbatim:
//!
//! ```text
//!   address   GxbNKNYdtNXQkhDkpHdLDAMX64GxaECgANqdfp6cUGH4
//!   owner     GZgs5uREPp6BvDt8eysmhavQPAHBAtjePgV4zfhgd9pH
//!   slot      22380121
//!   length    611 bytes
//! ```
//!
//! `fixtures/bcook_stake_pool.json` records where it came from. The point of a
//! captured account rather than a live fetch is that these tests must be
//! deterministic and offline; the point of capturing it at all is that nothing
//! else in the suite is anchored to reality.
//!
//! What this file asserts is therefore narrow and load-bearing: that the
//! program's parser, run over bytes nobody in this repository wrote, produces
//! the figures independently measured from that account.

mod common;

use aera::oracle::native_bcook;
use common::*;

/// The account as Cookie Chain served it.
const REAL_POOL: &[u8] = include_bytes!("../../../fixtures/bcook_stake_pool.bin");

/// bCOOK's mint, which the pool names as its `pool_mint`.
const BCOOK_MINT: Pubkey = anchor_lang::pubkey!("EkPafx58mgwkEnGwo62jXhXDAdJ37Z8G8MFBRPsr9uhz");

/// The stake-pool program that owns it.
const STAKE_POOL_PROGRAM: Pubkey =
    anchor_lang::pubkey!("GZgs5uREPp6BvDt8eysmhavQPAHBAtjePgV4zfhgd9pH");

/*
 * Measured from the account at slot 22380121 by `scripts/discover-bcook-oracle.ts`,
 * which derives every offset by walking the declared layout rather than by
 * guessing, and corroborates `pool_token_supply` against the bCOOK mint's own
 * supply. Written out here so a change in either the parser or the fixture has
 * to disagree with a number a human wrote down.
 */
const MEASURED_TOTAL_LAMPORTS: u64 = 126_916_639_823_291_785;
const MEASURED_POOL_TOKEN_SUPPLY: u64 = 97_414_250_137_692_269;
const MEASURED_LAST_UPDATE_EPOCH: u64 = 51;
const MEASURED_WITHDRAWAL_FEE_BPS: u16 = 200;

/// The program parses the real account into the measured figures.
///
/// The single most important test in the oracle suite. Everything else assumes
/// the layout; this is the only thing that checks it.
#[test]
fn the_program_parses_the_real_account() {
    assert_eq!(
        REAL_POOL.len(),
        native_bcook::STAKE_POOL_LEN,
        "the captured account is not the length the parser requires"
    );

    let view = native_bcook::parse_stake_pool(REAL_POOL).expect("the real account must parse");

    assert_eq!(view.pool_mint, BCOOK_MINT, "pool_mint is not bCOOK");
    assert_eq!(view.total_lamports, MEASURED_TOTAL_LAMPORTS);
    assert_eq!(view.pool_token_supply, MEASURED_POOL_TOKEN_SUPPLY);
    assert_eq!(view.last_update_epoch, MEASURED_LAST_UPDATE_EPOCH);
    assert_eq!(
        view.stake_withdrawal_fee_bps, MEASURED_WITHDRAWAL_FEE_BPS,
        "the redemption fee is not the 2.00% measured on chain"
    );
}

/// The rate the protocol would use for the live pool is the ratio, less the fee.
///
/// Stated in decimal as well as in fixed point, because a fixed-point number is
/// easy to get wrong by three orders of magnitude and have it still look right.
#[test]
fn the_real_account_gives_a_sane_rate() {
    let view = native_bcook::parse_stake_pool(REAL_POOL).unwrap();

    let gross = (view.total_lamports as u128) * aera::constants::FIXED_POINT_SCALE
        / (view.pool_token_supply as u128);

    // 1.30 COOK per bCOOK, give or take. A staking receipt that had drifted
    // outside this range would mean the parse was wrong, not that the pool had
    // moved: bCOOK has been accruing since genesis and does not jump.
    assert!(
        gross > 1_200_000_000_000_000_000 && gross < 1_500_000_000_000_000_000,
        "the live rate is outside anything plausible: {gross}"
    );

    // A staking receipt cannot be worth less than the asset it wraps.
    assert!(
        gross >= aera::constants::FIXED_POINT_SCALE,
        "the live rate is below parity: {gross}"
    );

    // And the 2% redemption fee comes out on top of that.
    let effective = aera::oracle::apply_withdrawal_fee(gross, MEASURED_WITHDRAWAL_FEE_BPS).unwrap();
    assert_eq!(
        effective,
        gross * 9_800 / 10_000,
        "the live fee is not being taken out in full"
    );
    assert!(
        effective < gross,
        "the redemption fee did not lower the rate"
    );
}

/// The live rate sits inside the absolute band the launch configuration sets.
///
/// A floor or ceiling that excluded the real pool would freeze the market on
/// its first refresh, which is the kind of thing that is obvious in hindsight
/// and invisible in a synthetic test where the test picked the rate.
#[test]
fn the_live_rate_is_inside_the_configured_band() {
    let view = native_bcook::parse_stake_pool(REAL_POOL).unwrap();
    let gross = (view.total_lamports as u128) * aera::constants::FIXED_POINT_SCALE
        / (view.pool_token_supply as u128);

    assert!(
        gross >= aera::constants::DEFAULT_RATE_FLOOR,
        "the live rate {gross} is below the configured floor {}",
        aera::constants::DEFAULT_RATE_FLOOR
    );
    assert!(
        gross <= aera::constants::DEFAULT_RATE_CEILING,
        "the live rate {gross} is above the configured ceiling {}",
        aera::constants::DEFAULT_RATE_CEILING
    );
    // The fee bound is checked at compile time, below: both sides are
    // constants, so a configuration that would freeze the live market on its
    // first refresh should fail to build rather than fail to run.
}

/*
 * The live redemption fee must fit inside the bound Aera launches with.
 *
 * Both are constants, so this is a compile-time check: lowering
 * DEFAULT_MAX_WITHDRAWAL_FEE_BPS below the 2.00% the deployed pool actually
 * charges would freeze the market on its first refresh, and that should stop
 * the build rather than wait for a test run.
 */
const _: () = assert!(
    MEASURED_WITHDRAWAL_FEE_BPS <= aera::constants::DEFAULT_MAX_WITHDRAWAL_FEE_BPS,
    "the live redemption fee exceeds DEFAULT_MAX_WITHDRAWAL_FEE_BPS -- the market \
     would freeze on its first refresh"
);

/// The pool's published previous epoch is present and consistent.
///
/// The bootstrap check depends on these two fields being real rather than
/// zero-filled padding. They sit at the end of a variable-width tail, so this
/// is also the strongest available evidence that the tail walk lands where it
/// should on a real account.
#[test]
fn the_real_account_carries_a_usable_previous_epoch() {
    let view = native_bcook::parse_stake_pool(REAL_POOL).unwrap();

    assert!(
        view.last_epoch_total_lamports > 0 && view.last_epoch_pool_token_supply > 0,
        "the last_epoch fields are empty -- the tail walk did not land on them"
    );

    let previous = (view.last_epoch_total_lamports as u128) * aera::constants::FIXED_POINT_SCALE
        / (view.last_epoch_pool_token_supply as u128);
    let current = (view.total_lamports as u128) * aera::constants::FIXED_POINT_SCALE
        / (view.pool_token_supply as u128);

    // One epoch of staking yield: fractions of a percent, and upward. A pair of
    // numbers read out of the wrong offsets would not land here.
    let moved_bps = current.abs_diff(previous) * 10_000 / previous;
    assert!(
        moved_bps < 100,
        "the pool moved {moved_bps} bps in one epoch, which is not staking yield -- \
         the last_epoch offsets are probably wrong"
    );
    assert!(
        current >= previous,
        "the live rate fell over the last epoch: {previous} -> {current}"
    );
}

/// The synthetic layout the rest of the suite uses agrees with the real one.
///
/// The harness builds its pools with `stake_pool_bytes`, and every other oracle
/// test is only as good as that builder. Rebuilding the *real* account's
/// figures through it and requiring the parser to produce the same view is what
/// connects the synthetic tests to the captured one: if the builder drifts, the
/// rest of the suite keeps passing and this fails.
#[test]
fn the_synthetic_layout_agrees_with_the_real_one() {
    let real = native_bcook::parse_stake_pool(REAL_POOL).unwrap();

    let synthetic_bytes = stake_pool_bytes_with_history(
        real.pool_mint,
        real.total_lamports,
        real.pool_token_supply,
        real.stake_withdrawal_fee_bps,
        real.last_update_epoch,
        Some((
            real.last_epoch_total_lamports,
            real.last_epoch_pool_token_supply,
        )),
    );
    let synthetic = native_bcook::parse_stake_pool(&synthetic_bytes)
        .expect("the harness must build something the parser accepts");

    assert_eq!(synthetic.pool_mint, real.pool_mint);
    assert_eq!(synthetic.total_lamports, real.total_lamports);
    assert_eq!(synthetic.pool_token_supply, real.pool_token_supply);
    assert_eq!(synthetic.last_update_epoch, real.last_update_epoch);
    assert_eq!(
        synthetic.stake_withdrawal_fee_bps, real.stake_withdrawal_fee_bps,
        "the harness writes the withdrawal fee somewhere the parser does not read it"
    );
    assert_eq!(
        synthetic.last_epoch_total_lamports, real.last_epoch_total_lamports,
        "the harness writes last_epoch_total_lamports somewhere else"
    );
    assert_eq!(
        synthetic.last_epoch_pool_token_supply, real.last_epoch_pool_token_supply,
        "the harness writes last_epoch_pool_token_supply somewhere else"
    );
}

/// The fixture is the account it claims to be.
///
/// A captured fixture is only evidence if its provenance is checkable. This
/// reads the recorded metadata rather than trusting the file name.
#[test]
fn the_fixture_records_where_it_came_from() {
    const PROVENANCE: &str = include_str!("../../../fixtures/bcook_stake_pool.json");

    assert!(
        PROVENANCE.contains(&STAKE_POOL_PROGRAM.to_string()),
        "the fixture does not record the stake-pool program as its owner"
    );
    assert!(
        PROVENANCE.contains("\"len\":611"),
        "the fixture does not record a 611-byte account"
    );
    assert_eq!(
        REAL_POOL[0], 1,
        "the first byte is not AccountType::StakePool"
    );
}
