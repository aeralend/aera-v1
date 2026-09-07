//! Borrowing, capping and liquidating against a Tier 3 market-priced reserve.
//!
//! The sibling suites test the oracle as an oracle. This one tests it as part of
//! the protocol, which is where a Tier 3 source differs from Tier 1 in ways that
//! do not show up in isolation:
//!
//!   * every write path calls `require_fresh`, so a refresh must be able to
//!     happen in the same slot as the action -- always, not only when the
//!     spacing rule happens to allow a new observation;
//!   * `Bootstrapping` must actually block borrowing rather than merely being
//!     reported;
//!   * liquidation must keep working from the last accepted reference when the
//!     oracle is degraded, since that is precisely when it is needed.

mod common;

use aera::oracle::breaker::OracleHealth;
use anchor_lang::prelude::Pubkey;
use common::damm::move_pool_price_pct;
use common::market::*;
use common::*;
use solana_keypair::Keypair;

fn health(raw: u8) -> OracleHealth {
    OracleHealth::from_u8(raw).expect("oracle health decodes")
}

/// `whole` collateral tokens at COLLATERAL_DECIMALS (6), not the 9 the rest of
/// the harness assumes.
fn collateral_tokens(whole: u64) -> u64 {
    whole * 10u64.pow(COLLATERAL_DECIMALS as u32)
}

/// A funded lender, a borrower holding market-priced collateral, and the
/// obligation it is posted to.
fn book(f: &mut Fixture) -> (Keypair, Pubkey) {
    let (collateral, cook) = (f.collateral, f.cook);

    let lender = f.env.create_user();
    f.env.fund(&lender, cook.mint, tokens(500_000));
    f.env.supply(&lender, &cook, tokens(500_000));

    let borrower = f.env.create_user();
    f.env
        .fund(&borrower, collateral.mint, collateral_tokens(400_000));
    f.env.fund(&borrower, cook.mint, 0);
    let obligation = f
        .env
        .open_position(&borrower, &collateral, collateral_tokens(400_000));
    (borrower, obligation)
}

// ===========================================================================
// Freshness and spacing must not deadlock each other
// ===========================================================================

#[test]
fn live_00_an_action_is_possible_at_any_moment_not_just_after_the_spacing() {
    /*
     * The liveness question, and the one that decides whether a Tier 3 market
     * is usable at all.
     *
     * `require_fresh` demands `last_refresh_slot == current_slot`, so every
     * borrow, withdrawal and liquidation must carry a refresh in the same
     * transaction. `min_spacing_seconds` refuses a *new observation* inside 60
     * seconds. If those two rules are implemented as one, the market is open
     * for a single slot once a minute and shut the rest of the time -- and
     * liquidation, which cannot choose its moment, is the thing that breaks.
     *
     * The spacing rule governs the observation history. It must not govern the
     * freshness stamp.
     */
    let mut f = Fixture::new();
    f.init();
    f.bootstrap();

    // A refresh moments after the last one: too soon for a new observation.
    let before = f.market_oracle().observations.len();
    f.env.warp_seconds(3);
    f.refresh()
        .expect("a refresh inside the spacing window must still restate freshness");

    let after = f.market_oracle();
    assert_eq!(
        after.observations.len(),
        before,
        "a refresh inside the spacing window added an observation; the spam \
         defence is what stops a 30-second manipulation filling the buffer"
    );
    assert_eq!(
        f.env.read_oracle(f.collateral_mint).last_refresh_slot,
        f.env.clock().slot,
        "the refresh did not stamp the current slot, so no action can follow it"
    );
}

#[test]
fn live_01_a_borrow_lands_in_the_same_slot_as_its_refresh() {
    let mut f = Fixture::new();
    f.init();
    f.bootstrap();

    let (collateral, cook) = (f.collateral, f.cook);
    let (borrower, obligation) = book(&mut f);

    // Immediately after a bootstrap observation -- i.e. inside the spacing
    // window, the state a borrower is in most of the time.
    f.env.warp_seconds(5);
    f.env
        .try_borrow(
            &borrower,
            &cook,
            obligation,
            tokens(100),
            &[&collateral, &cook],
        )
        .expect("borrowing against a market-priced collateral must be possible");
}

// ===========================================================================
// Bootstrapping must block new debt
// ===========================================================================

#[test]
fn boot_02_an_unbootstrapped_market_refuses_a_borrow() {
    let mut f = Fixture::new();
    f.init();
    f.refresh().expect("first observation");
    assert_eq!(health(f.health()), OracleHealth::Bootstrapping);

    let (collateral, cook) = (f.collateral, f.cook);
    let (borrower, obligation) = book(&mut f);

    f.env.warp_seconds(5);
    assert!(
        f.env
            .try_borrow(
                &borrower,
                &cook,
                obligation,
                tokens(100),
                &[&collateral, &cook]
            )
            .is_err(),
        "a market with one observation and no elapsed history permitted new debt"
    );

    // And the same borrow succeeds once the history exists.
    f.bootstrap();
    f.env.warp_seconds(5);
    f.env
        .try_borrow(
            &borrower,
            &cook,
            obligation,
            tokens(100),
            &[&collateral, &cook],
        )
        .expect("a bootstrapped market must permit the same borrow");
}

// ===========================================================================
// Degradation stops borrowing without stopping liquidation
// ===========================================================================

#[test]
fn deg_00_a_degraded_oracle_blocks_borrowing_and_permits_repayment() {
    /*
     * `permits(health, action)` is the matrix under test, reached through the
     * real instructions rather than called directly. Repay must stay open in
     * every state -- a borrower locked out of repaying during an incident is a
     * borrower who gets liquidated by the incident.
     */
    let mut f = Fixture::new();
    f.init();
    f.bootstrap();

    let (collateral, cook) = (f.collateral, f.cook);
    let (borrower, obligation) = book(&mut f);
    f.env.warp_seconds(5);
    f.env
        .try_borrow(
            &borrower,
            &cook,
            obligation,
            tokens(200),
            &[&collateral, &cook],
        )
        .expect("the initial borrow");

    // Push the pools apart, past the deviation limit.
    let (c, q) = (f.collateral_reserve, f.quote_reserve);
    move_pool_price_pct(&mut f.env.svm, &f.pool_a, c, q, 400);
    f.observe_after(90);
    assert_eq!(health(f.health()), OracleHealth::BorrowFrozen);

    f.env.warp_seconds(5);
    assert!(
        f.env
            .try_borrow(
                &borrower,
                &cook,
                obligation,
                tokens(10),
                &[&collateral, &cook]
            )
            .is_err(),
        "new debt was permitted while the two pools disagreed by 400%"
    );

    f.env
        .try_repay(&borrower, &cook, obligation, tokens(50))
        .expect("repayment must remain open in every oracle state");
}

// ===========================================================================
// The per-wallet borrow cap
// ===========================================================================

#[test]
fn cap_04_the_per_wallet_cap_applies_to_a_market_priced_market() {
    /*
     * The wallet cap is the solvency control for COOKHOUSE, because liquidator
     * profit peaks around 50k and falls after -- so a position larger than the
     * cap is one no liquidator wants. It is configured on the borrowed reserve,
     * and must behave the same whether the collateral is Tier 1 or Tier 3.
     */
    let mut f = Fixture::new();
    f.init();
    f.bootstrap();

    let (collateral, cook) = (f.collateral, f.cook);
    f.env
        .set_risk_config(&cook, tokens(500))
        .expect("set the per-wallet cap");
    let (borrower, obligation) = book(&mut f);

    f.env.warp_seconds(5);
    f.env
        .try_borrow(
            &borrower,
            &cook,
            obligation,
            tokens(400),
            &[&collateral, &cook],
        )
        .expect("a borrow inside the cap");

    f.env.warp_seconds(5);
    assert!(
        f.env
            .try_borrow(
                &borrower,
                &cook,
                obligation,
                tokens(200),
                &[&collateral, &cook]
            )
            .is_err(),
        "the per-wallet cap did not bind against a market-priced collateral"
    );
}

#[test]
fn cap_05_the_cap_is_per_wallet_not_per_market() {
    // Sybil resistance is not claimed: splitting across wallets is possible and
    // the cap is not a defence against it. What the cap does is bound the size
    // of any single position a liquidator has to clear, which is the property
    // the liquidation model depends on. Recorded here so the limit is explicit
    // rather than implied.
    let mut f = Fixture::new();
    f.init();
    f.bootstrap();

    let (collateral, cook) = (f.collateral, f.cook);
    f.env
        .set_risk_config(&cook, tokens(500))
        .expect("set the per-wallet cap");

    let lender = f.env.create_user();
    f.env.fund(&lender, cook.mint, tokens(500_000));
    f.env.supply(&lender, &cook, tokens(500_000));

    for _ in 0..3 {
        let borrower = f.env.create_user();
        f.env
            .fund(&borrower, collateral.mint, collateral_tokens(400_000));
        f.env.fund(&borrower, cook.mint, 0);
        let obligation = f
            .env
            .open_position(&borrower, &collateral, collateral_tokens(400_000));
        f.env.warp_seconds(5);
        f.env
            .try_borrow(
                &borrower,
                &cook,
                obligation,
                tokens(400),
                &[&collateral, &cook],
            )
            .expect("each wallet gets its own allowance");
    }
}
