//! The Tier 3 market oracle, end to end.
//!
//! The decoder reads four fields out of a Meteora pool by byte offset, because
//! no IDL is published. That is the part most likely to be wrong in a way
//! nothing notices, so most of this file is about making it wrong on purpose and
//! checking it fails closed.
//!
//! The parameters used here are **test-only** and deliberately not production
//! values — see `TEST_MARKET_CONFIG`. Real ones have to be calibrated from
//! observed history, which is what `tools/oracle-calibrate.ts` collects.

mod common;

use aera::state::PoolRef;
use anchor_lang::prelude::Pubkey;
use common::damm::*;
use common::market::*;
use common::*;

// ===========================================================================
// The decoder must fail closed
// ===========================================================================

/// Every one of these builds a structurally wrong pool and asserts the refresh
/// is refused. The decoder reads by offset; a wrong account read at those
/// offsets yields arbitrary pubkeys, and arbitrary pubkeys must never become a
/// price.
fn decoder_case(mutate: impl FnOnce(PoolSpec) -> PoolSpec, what: &str) {
    let (mut env, cook, _bcook) = Env::core(1_000);
    let collateral_mint = env
        .add_market_reserve(COLLATERAL_DECIMALS, bcook_config())
        .mint;
    let quote_mint = cook.mint;

    let good = create_mock_damm_pool(
        &mut env.svm,
        &PoolSpec::new(
            Pubkey::new_from_array([21u8; 32]),
            collateral_mint,
            quote_mint,
            30_000_000_000_000,
            850_000_000_000_000,
        ),
    );
    let bad_spec = mutate(PoolSpec::new(
        Pubkey::new_from_array([22u8; 32]),
        collateral_mint,
        quote_mint,
        30_000_000_000_000,
        850_000_000_000_000,
    ));
    let bad = create_mock_damm_pool(&mut env.svm, &bad_spec);
    set_amm_program_data(&mut env.svm, AMM_PROGRAM_DATA, AMM_DEPLOY_SLOT);

    let refs = [
        PoolRef {
            pool: good.pool,
            collateral_vault: good.collateral_vault,
            quote_vault: good.quote_vault,
        },
        PoolRef {
            pool: bad.pool,
            collateral_vault: bad.collateral_vault,
            quote_vault: bad.quote_vault,
        },
    ];
    env.init_market_oracle(
        collateral_mint,
        collateral_mint,
        quote_mint,
        COLLATERAL_DECIMALS,
        QUOTE_DECIMALS,
        refs,
        test_market_config(),
    )
    .expect("init_market_oracle");

    let payer = env.admin.insecure_clone();
    let result =
        env.try_refresh_market_oracle_as(&payer, collateral_mint, &good, &bad, AMM_PROGRAM_DATA);
    assert!(
        result.is_err(),
        "a pool with {what} was accepted; the decoder must fail closed"
    );
}

#[test]
fn dec_00_wrong_owner_is_refused() {
    decoder_case(
        |s| s.owner(Pubkey::new_from_array([77u8; 32])),
        "the wrong owning program",
    );
}

#[test]
fn dec_01_wrong_length_is_refused() {
    // The offsets would read arbitrary bytes as pubkeys.
    decoder_case(|s| s.length(POOL_LEN - 8), "a shorter account");
}

#[test]
fn dec_02_longer_than_expected_is_refused() {
    // A longer account is a different layout, even if the first 1112 bytes look
    // right -- the version that produced it is not the one that was audited.
    decoder_case(|s| s.length(POOL_LEN + 64), "a longer account");
}

#[test]
fn dec_03_wrong_collateral_mint_is_refused() {
    decoder_case(
        |s| s.wrong_collateral_mint(Pubkey::new_from_array([88u8; 32])),
        "a different collateral mint",
    );
}

#[test]
fn dec_04_wrong_quote_mint_is_refused() {
    decoder_case(
        |s| s.wrong_quote_mint(Pubkey::new_from_array([89u8; 32])),
        "a different quote mint",
    );
}

#[test]
fn dec_05_duplicate_vaults_are_refused() {
    // One account named as both sides would make the price 1.0 by construction.
    decoder_case(|s| s.duplicate_vaults(), "the same account as both vaults");
}

// ===========================================================================
// Orientation
// ===========================================================================

#[test]
fn ori_00_both_orientations_produce_the_same_price() {
    // The live pools disagree about which token is A. An implementation that
    // assumed a position would read the price upside down for half the market,
    // which is a 1300x error rather than a rounding one.
    let collateral = 30_000_000_000_000u64;
    let quote = 850_000_000_000_000u64;

    let mut first = Fixture::with(Orientation::CollateralFirst, Orientation::CollateralFirst);
    let refs = first.pool_refs();
    first
        .env
        .init_market_oracle(
            first.collateral_mint,
            first.collateral_mint,
            first.cook.mint,
            COLLATERAL_DECIMALS,
            QUOTE_DECIMALS,
            refs,
            test_market_config(),
        )
        .expect("init");
    first.refresh().expect("refresh with collateral first");
    let a = first
        .env
        .read_oracle(first.collateral_mint)
        .reference
        .effective_rate;

    let mut second = Fixture::with(Orientation::QuoteFirst, Orientation::QuoteFirst);
    let refs = second.pool_refs();
    second
        .env
        .init_market_oracle(
            second.collateral_mint,
            second.collateral_mint,
            second.cook.mint,
            COLLATERAL_DECIMALS,
            QUOTE_DECIMALS,
            refs,
            test_market_config(),
        )
        .expect("init");
    second.refresh().expect("refresh with quote first");
    let b = second
        .env
        .read_oracle(second.collateral_mint)
        .reference
        .effective_rate;

    assert_eq!(a, b, "orientation changed the price");
    assert_eq!(
        a,
        expected_price(collateral, quote, COLLATERAL_DECIMALS, QUOTE_DECIMALS),
        "price does not match the decimal-normalised expectation"
    );
}

// ===========================================================================
// Permissionless
// ===========================================================================

#[test]
fn perm_00_any_wallet_can_refresh() {
    // The property that stops Aera being a liveness monopoly. There is no signer
    // on the instruction; the stranger below pays a fee and nothing else.
    let mut f = Fixture::new();
    let refs = f.pool_refs();
    f.env
        .init_market_oracle(
            f.collateral_mint,
            f.collateral_mint,
            f.cook.mint,
            COLLATERAL_DECIMALS,
            QUOTE_DECIMALS,
            refs,
            test_market_config(),
        )
        .expect("init");

    f.refresh_as_stranger()
        .expect("a wallet with no relationship to Aera must be able to refresh");
    assert_eq!(
        f.env
            .read_market_oracle(f.collateral_mint)
            .observations
            .len(),
        1
    );
}

// ===========================================================================
// The min combiner
// ===========================================================================

#[test]
fn min_00_a_one_pool_pump_cannot_raise_the_price() {
    // The whole argument for min() over a depth-weighted mean: an attacker who
    // moves one pool gains nothing, because the honest pool still floors it.
    let mut f = Fixture::new();
    let refs = f.pool_refs();
    f.env
        .init_market_oracle(
            f.collateral_mint,
            f.collateral_mint,
            f.cook.mint,
            COLLATERAL_DECIMALS,
            QUOTE_DECIMALS,
            refs,
            test_market_config(),
        )
        .expect("init");
    f.refresh().expect("first observation");
    let honest = f
        .env
        .read_oracle(f.collateral_mint)
        .reference
        .effective_rate;

    for pct in [10i64, 25, 50, 100, 300, 900] {
        // Pump pool A only. Pool B is untouched and remains the floor.
        let (c, q) = (f.collateral_reserve, f.quote_reserve);
        move_pool_price_pct(&mut f.env.svm, &f.pool_a, c, q, pct);
        f.env.warp_seconds(120);
        // The deviation guard will refuse to *raise*; either way the recorded
        // price must never exceed the honest pool.
        let _ = f.refresh();
        let after = f
            .env
            .read_oracle(f.collateral_mint)
            .reference
            .effective_rate;
        assert!(
            after <= honest,
            "a +{pct}% pump of one pool raised the accepted price from {honest} to {after}"
        );
        set_pool_reserves(&mut f.env.svm, &f.pool_a, c, q);
    }
}

// ===========================================================================
// Spam and spacing
// ===========================================================================

#[test]
fn spam_00_a_second_observation_too_soon_is_refused() {
    let mut f = Fixture::new();
    let refs = f.pool_refs();
    f.env
        .init_market_oracle(
            f.collateral_mint,
            f.collateral_mint,
            f.cook.mint,
            COLLATERAL_DECIMALS,
            QUOTE_DECIMALS,
            refs,
            test_market_config(),
        )
        .expect("init");

    f.refresh().expect("first");
    f.env.warp_seconds(10); // under the 60s minimum spacing
                            /*
                             * The call SUCCEEDS -- it restates freshness so an action can follow it in
                             * the same slot -- but it must not append. The spam defence is the buffer
                             * staying at one entry, not the transaction failing.
                             *
                             * Failing the transaction was the earlier behaviour, and it made a Tier 3
                             * market unusable: `require_fresh` needs a same-slot refresh, so every
                             * borrow and liquidation inside the spacing window failed with it. See
                             * `live_00` in the integration suite.
                             */
    f.refresh()
        .expect("a refresh inside the spacing window must still restate freshness");
    assert_eq!(
        f.env
            .read_market_oracle(f.collateral_mint)
            .observations
            .len(),
        1,
        "an observation inside the minimum spacing was appended, which is how a \
         30-second manipulation fills the whole window"
    );
}

#[test]
fn spam_01_rejection_does_not_corrupt_the_buffer_index() {
    let mut f = Fixture::new();
    let refs = f.pool_refs();
    f.env
        .init_market_oracle(
            f.collateral_mint,
            f.collateral_mint,
            f.cook.mint,
            COLLATERAL_DECIMALS,
            QUOTE_DECIMALS,
            refs,
            test_market_config(),
        )
        .expect("init");

    f.refresh().expect("first");
    for _ in 0..5 {
        f.env.warp_seconds(5);
        let _ = f.refresh();
    }
    let before = f.env.read_market_oracle(f.collateral_mint);
    assert_eq!(before.observations.len(), 1);
    assert_eq!(before.next_index, 1);

    f.env.warp_seconds(120);
    f.refresh()
        .expect("a properly spaced observation must still work");
    let after = f.env.read_market_oracle(f.collateral_mint);
    assert_eq!(after.observations.len(), 2);
    assert_eq!(after.next_index, 2);
}

// ===========================================================================
// Bootstrap
// ===========================================================================

#[test]
fn boot_00_observations_without_span_do_not_bootstrap() {
    // Three observations at the minimum spacing is 120 seconds of history
    // against a 300-second requirement. Count alone must not be enough.
    let mut f = Fixture::new();
    let refs = f.pool_refs();
    f.env
        .init_market_oracle(
            f.collateral_mint,
            f.collateral_mint,
            f.cook.mint,
            COLLATERAL_DECIMALS,
            QUOTE_DECIMALS,
            refs,
            test_market_config(),
        )
        .expect("init");

    for _ in 0..3 {
        f.refresh().expect("observation");
        f.env.warp_seconds(61);
    }

    let market_oracle = f.env.read_market_oracle(f.collateral_mint);
    assert!(market_oracle.observations.len() >= 3);
    assert!(
        !market_oracle.is_bootstrapped(),
        "three closely spaced samples satisfied a 300-second span requirement"
    );

    let oracle = f.env.read_oracle(f.collateral_mint);
    assert_eq!(
        oracle.health,
        aera::oracle::breaker::OracleHealth::Bootstrapping as u8,
        "an unbootstrapped oracle must not report anything else"
    );
}

#[test]
fn boot_01_enough_observations_over_enough_time_do_bootstrap() {
    let mut f = Fixture::new();
    let refs = f.pool_refs();
    f.env
        .init_market_oracle(
            f.collateral_mint,
            f.collateral_mint,
            f.cook.mint,
            COLLATERAL_DECIMALS,
            QUOTE_DECIMALS,
            refs,
            test_market_config(),
        )
        .expect("init");

    for _ in 0..4 {
        f.refresh().expect("observation");
        f.env.warp_seconds(150);
    }

    assert!(
        f.env
            .read_market_oracle(f.collateral_mint)
            .is_bootstrapped(),
        "four observations over 450s should satisfy 3 over 300s"
    );
}

// ===========================================================================
// The deployment pin
// ===========================================================================

#[test]
fn pin_00_a_redeployed_amm_is_refused() {
    // Aera cannot stop Meteora being upgraded -- its authority is a key Aera
    // does not hold. What it can do is stop pricing against a layout it has
    // never seen, since the decoder depends on one.
    let mut f = Fixture::new();
    let refs = f.pool_refs();
    f.env
        .init_market_oracle(
            f.collateral_mint,
            f.collateral_mint,
            f.cook.mint,
            COLLATERAL_DECIMALS,
            QUOTE_DECIMALS,
            refs,
            test_market_config(),
        )
        .expect("init");
    f.refresh().expect("healthy first observation");

    set_amm_program_data(&mut f.env.svm, AMM_PROGRAM_DATA, AMM_DEPLOY_SLOT + 1);
    f.env.warp_seconds(120);
    assert!(
        f.refresh().is_err(),
        "the oracle kept pricing after the AMM was redeployed"
    );
}

// ===========================================================================
// Core must not be routable through this path
// ===========================================================================

#[test]
fn core_00_a_stake_pool_oracle_cannot_be_refreshed_as_a_market_oracle() {
    // bCOOK's oracle is NativeExchangeRate. Routing it through here would give
    // it a reference with no stake-pool reading behind it at all.
    let mut f = Fixture::new();
    let payer = f.env.admin.insecure_clone();
    let bcook_mint = f.bcook.mint;
    let (a, b) = (f.pool_a, f.pool_b);

    let result = f
        .env
        .try_refresh_market_oracle_as(&payer, bcook_mint, &a, &b, AMM_PROGRAM_DATA);
    assert!(
        result.is_err(),
        "a Core oracle was refreshed through the market path"
    );
}

#[test]
fn core_01_core_oracles_still_refresh_normally() {
    // The other half of the separation: adding MarketTwap must not disturb the
    // two kinds Core actually uses.
    let (mut env, cook, bcook) = Env::core(1_000);
    // These panic on failure rather than returning, so reaching the assertions
    // below is itself the check.
    env.refresh_oracle(cook.mint);
    env.refresh_oracle(bcook.mint);
    assert_eq!(
        env.read_oracle(cook.mint).source_kind,
        aera::oracle::OracleSourceKind::UnitOfAccount as u8,
        "COOK must still be a unit of account"
    );
    assert_eq!(
        env.read_oracle(bcook.mint).source_kind,
        aera::oracle::OracleSourceKind::NativeExchangeRate as u8,
        "bCOOK must still read the stake pool"
    );
}

#[test]
fn core_02_a_market_oracle_cannot_be_attached_to_a_core_oracle() {
    /*
     * The mirror of `core_00`, one step earlier.
     *
     * `refresh_market_oracle` refuses a Core oracle, so an accidental attachment
     * is not exploitable -- but it is silent until the first crank, and what an
     * operator sees then is `UnknownOracleSource` from an instruction they did
     * not knowingly misconfigure. Refusing at configuration time puts the error
     * where the mistake is.
     */
    let mut f = Fixture::new();
    let refs = f.pool_refs();
    let bcook_mint = f.bcook.mint;
    let quote_mint = f.cook.mint;

    let result = f.env.init_market_oracle(
        bcook_mint,
        f.collateral_mint,
        quote_mint,
        COLLATERAL_DECIMALS,
        QUOTE_DECIMALS,
        refs,
        test_market_config(),
    );
    assert!(
        result.is_err(),
        "a MarketOracle was attached to a stake-pool oracle"
    );
}
