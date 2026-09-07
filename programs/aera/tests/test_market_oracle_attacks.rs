//! The market oracle under attack, and under a genuine crash.
//!
//! `test_market_oracle` checks the decoder and the mechanics. This file checks
//! the part that actually decides whether the market is safe: what an attacker
//! who can move the pools can make the accepted price do, and -- the mirror
//! image, which is easy to forget -- that a real crash still marks collateral
//! down instead of being mistaken for an attack and frozen out.
//!
//! The two failures are not symmetric. Letting an attacker raise the price
//! creates bad debt on purpose. Refusing to lower it during a crash creates bad
//! debt by omission, and is the failure most oracle designs actually suffer.
//! Both are tested here.
//!
//! Every threshold used is `test_market_config()`, which is **not production
//! calibrated** -- see the note on it.

mod common;

use aera::oracle::breaker::OracleHealth;
use anchor_lang::prelude::Pubkey;
use common::damm::*;
use common::market::*;
use common::*;

/// `max_rise_bps_per_window` in `test_market_config`.
const RISE_CAP_BPS: u128 = 1_000;
/// `max_cross_pool_deviation_bps` in `test_market_config`.
const MAX_DEVIATION_BPS: i64 = 300;

fn health(raw: u8) -> OracleHealth {
    OracleHealth::from_u8(raw).expect("oracle health decodes")
}

/// A rise of `bps` applied to `price`, which is the most the breaker may admit
/// from one observation.
fn ceiling_after_one_rise(price: u128) -> u128 {
    price + price * RISE_CAP_BPS / 10_000
}

// ===========================================================================
// Manipulating one pool
// ===========================================================================

#[test]
fn atk_00_pumping_one_pool_never_raises_the_accepted_price() {
    /*
     * The single most important property of the `min` combiner.
     *
     * `min_00` in the sibling suite checks this from a single observation. This
     * checks it against an oracle that is bootstrapped and Healthy -- the state
     * where a raised price would actually be lent against -- and holds the pump
     * across many observations rather than one.
     */
    let mut f = Fixture::new();
    f.init();
    f.bootstrap();
    let honest = f.accepted();

    let (c, q) = (f.collateral_reserve, f.quote_reserve);
    move_pool_price_pct(&mut f.env.svm, &f.pool_a, c, q, 400);

    for _ in 0..12 {
        f.env.warp_seconds(90);
        let _ = f.refresh();
        assert!(
            f.accepted() <= honest,
            "a sustained +400% pump of one pool raised the accepted price from \
             {honest} to {}; the untouched pool must floor it",
            f.accepted()
        );
    }
}

#[test]
fn atk_01_pumping_one_pool_freezes_borrowing() {
    // Not raising the price is half the answer. The other half is that Aera
    // must notice it is being lied to and stop lending, because it can no
    // longer tell which pool is honest.
    let mut f = Fixture::new();
    f.init();
    f.bootstrap();
    assert_eq!(health(f.health()), OracleHealth::Healthy);

    let (c, q) = (f.collateral_reserve, f.quote_reserve);
    move_pool_price_pct(&mut f.env.svm, &f.pool_a, c, q, 400);
    f.observe_after(90);

    assert_eq!(
        health(f.health()),
        OracleHealth::BorrowFrozen,
        "pools disagreeing by 400% left the oracle willing to permit new debt"
    );
}

// ===========================================================================
// Manipulating both pools
// ===========================================================================

#[test]
fn atk_02_pumping_both_pools_is_capped_per_observation() {
    /*
     * The attacker pays to move BOTH books, which is what `min` forces. Even
     * then a single observation may only carry the reference up by
     * `max_rise_bps_per_window`.
     *
     * This is the honest statement of the guarantee, and it is deliberately
     * per-observation rather than per-window: the breaker compares each new
     * TWAP against the reference it currently holds, so an attacker who
     * sustains the manipulation across N properly spaced observations gets N
     * capped steps, not one. `atk_03` measures exactly that, because a bound
     * that only holds for a single observation is not a bound.
     */
    let mut f = Fixture::new();
    f.init();
    f.bootstrap();
    let before = f.accepted();

    let (c, q) = (f.collateral_reserve, f.quote_reserve);
    // +900% on both, so they still agree and the deviation guard stays quiet.
    move_pool_price_pct(&mut f.env.svm, &f.pool_a, c, q, 900);
    move_pool_price_pct(&mut f.env.svm, &f.pool_b, c, q, 900);
    f.observe_after(90);

    let after = f.accepted();
    assert!(
        after > before,
        "a genuine agreed rise must be admitted at all"
    );
    assert!(
        after <= ceiling_after_one_rise(before),
        "a +900% pump of both pools moved the reference from {before} to {after}, \
         past the {RISE_CAP_BPS} bps per-observation cap"
    );
}

#[test]
fn atk_03_a_sustained_dual_pump_rises_no_faster_than_the_cap_allows() {
    /*
     * The rate of the lie, not just its first step.
     *
     * An attacker who holds a 10x pump on both books can take one capped step
     * per `min_spacing_seconds`. The property that matters operationally is
     * that no single step exceeds the cap -- so the cost of moving the
     * reference by X is the cost of holding the manipulation for the time X/cap
     * steps take, which is what makes it uneconomic against a real book.
     */
    let mut f = Fixture::new();
    f.init();
    f.bootstrap();

    let (c, q) = (f.collateral_reserve, f.quote_reserve);
    move_pool_price_pct(&mut f.env.svm, &f.pool_a, c, q, 900);
    move_pool_price_pct(&mut f.env.svm, &f.pool_b, c, q, 900);

    let mut previous = f.accepted();
    for step in 0..20 {
        f.env.warp_seconds(61); // the tightest spacing the program allows
        let _ = f.refresh();
        let now = f.accepted();
        assert!(
            now <= ceiling_after_one_rise(previous),
            "step {step} moved the reference from {previous} to {now}, past the cap"
        );
        previous = now;
    }
}

#[test]
fn atk_04_an_atomic_manipulation_is_diluted_by_the_time_weighting() {
    /*
     * The flash-loan shape: move both pools, refresh, move them back, all
     * without time passing.
     *
     * Two things stop it. `min_spacing_seconds` means the pumped reading cannot
     * be repeated, and the TWAP is weighted by elapsed time, so a reading that
     * stood for one second against half an hour of honest history barely moves
     * the average.
     */
    let mut f = Fixture::new();
    f.init();
    f.bootstrap();
    let honest = f.accepted();

    let (c, q) = (f.collateral_reserve, f.quote_reserve);
    move_pool_price_pct(&mut f.env.svm, &f.pool_a, c, q, 5_000);
    move_pool_price_pct(&mut f.env.svm, &f.pool_b, c, q, 5_000);

    /*
     * No warp: the same second as the last observation. The refresh is accepted
     * -- it has to be, or nothing could act in this slot -- but it must append
     * nothing and, crucially, must not carry the reference up.
     */
    let rows = f.market_oracle().observations.len();
    // One slot, not one second: `warp_slots` adds 0.4s and truncates, so the
    // clock second is unchanged while the blockhash differs. Without it the
    // identical transaction is deduplicated as `AlreadyProcessed`, which would
    // pass the test for entirely the wrong reason.
    f.env.warp_slots(1);
    f.refresh().expect("a same-slot refresh must be accepted");
    assert_eq!(
        f.market_oracle().observations.len(),
        rows,
        "a 50x pump was appended to the history inside the spacing window"
    );
    assert_eq!(
        f.accepted(),
        honest,
        "a refresh that recorded nothing still raised the reference; the rise cap          is only a cost if steps are rationed by the spacing rule"
    );

    // And repeating it every slot must not walk the reference up either.
    for _ in 0..50 {
        f.env.warp_slots(1);
        let _ = f.refresh();
        assert_eq!(
            f.accepted(),
            honest,
            "refreshing every slot during a pump walked the reference up"
        );
    }

    f.set_both_pools(c, q);

    // And even when the attacker does wait out the spacing, one second of
    // 50x weight against half an hour of history must not carry the average
    // anywhere near the manipulated price.
    move_pool_price_pct(&mut f.env.svm, &f.pool_a, c, q, 5_000);
    move_pool_price_pct(&mut f.env.svm, &f.pool_b, c, q, 5_000);
    f.observe_after(61);
    assert!(
        f.accepted() <= ceiling_after_one_rise(honest),
        "a 50x atomic pump moved the reference from {honest} to {}",
        f.accepted()
    );
}

#[test]
fn atk_05_an_attacker_choosing_sample_times_cannot_fill_the_window() {
    /*
     * The sampling-time attack: the caller picks when observations happen, so
     * they try to place every one of them inside their own manipulation.
     *
     * `min_spacing_seconds` is what makes that expensive -- filling the whole
     * 32-slot buffer at 60s apart means holding the pump for over half an hour.
     * Here the attacker manages six observations, and the older honest history
     * still has to be present afterwards.
     */
    let mut f = Fixture::new();
    f.init();
    f.bootstrap();
    let honest_span = f.market_oracle().span_seconds();

    let (c, q) = (f.collateral_reserve, f.quote_reserve);
    move_pool_price_pct(&mut f.env.svm, &f.pool_a, c, q, 900);
    move_pool_price_pct(&mut f.env.svm, &f.pool_b, c, q, 900);
    for _ in 0..6 {
        f.env.warp_seconds(61);
        let _ = f.refresh();
    }

    let oracle = f.market_oracle();
    assert!(
        oracle.span_seconds() > honest_span,
        "the attacker's observations replaced history instead of extending it"
    );
    assert!(
        oracle.observations.len() > 6,
        "six manipulated observations evicted the honest history; the buffer \
         holds {} entries",
        oracle.observations.len()
    );
}

// ===========================================================================
// A genuine crash must still mark collateral down
// ===========================================================================

#[test]
fn crash_00_a_fall_is_accepted_immediately_at_any_size() {
    // No downside cap, deliberately. A capped fall is a protocol valuing
    // collateral above its worth exactly when it needs to liquidate.
    for pct in [-10i64, -40, -75, -95] {
        let mut f = Fixture::new();
        f.init();
        f.bootstrap();

        let (c, q) = (f.collateral_reserve, f.quote_reserve);
        move_pool_price_pct(&mut f.env.svm, &f.pool_a, c, q, pct);
        move_pool_price_pct(&mut f.env.svm, &f.pool_b, c, q, pct);
        f.observe_after(90);

        // The TWAP still contains honest history, so the reference has not
        // fallen the full distance -- but it must have moved down at all, and
        // it must keep moving down as the crash persists.
        let first = f.accepted();
        for _ in 0..30 {
            f.observe_after(90);
        }
        let settled = f.accepted();
        assert!(
            settled < first,
            "a sustained {pct}% crash stopped marking down at {first}"
        );
    }
}

#[test]
fn crash_01_a_crash_with_the_pools_disagreeing_still_marks_down() {
    /*
     * The case that decides whether the degradation flags were designed
     * correctly.
     *
     * In a real crash the two books diverge -- they are being hit at different
     * speeds by different flow. An implementation that treats disagreement as a
     * reason to refuse the reading keeps the pre-crash price, and every
     * underwater position becomes bad debt. Disagreement must freeze *borrowing*
     * and still allow the price *down*.
     */
    let mut f = Fixture::new();
    f.init();
    f.bootstrap();
    let before = f.accepted();

    let (c, q) = (f.collateral_reserve, f.quote_reserve);
    move_pool_price_pct(&mut f.env.svm, &f.pool_a, c, q, -60);
    move_pool_price_pct(&mut f.env.svm, &f.pool_b, c, q, -80); // far past the deviation limit
    for _ in 0..20 {
        f.observe_after(90);
    }

    assert!(
        f.accepted() < before,
        "pools disagreeing during a crash froze the price at {before} instead of \
         marking it down"
    );
    assert_eq!(
        health(f.health()),
        OracleHealth::BorrowFrozen,
        "disagreement must stop new borrowing even while the price falls"
    );
}

#[test]
fn crash_02_a_crash_with_the_book_collapsing_still_marks_down() {
    // Same argument for depth. Liquidity leaving is the normal accompaniment to
    // a crash, and must not be a reason to hold the old valuation.
    let mut f = Fixture::new();
    f.init();
    f.bootstrap();
    let before = f.accepted();

    // Half the price and a book far below `min_pool_quote_depth`.
    let thin_collateral = f.collateral_reserve / 1_000;
    let thin_quote = f.quote_reserve / 2_000;
    f.set_both_pools(thin_collateral, thin_quote);
    for _ in 0..20 {
        f.observe_after(90);
    }

    assert!(
        f.accepted() < before,
        "a collapsing book froze the price at {before}"
    );
    assert_eq!(
        health(f.health()),
        OracleHealth::BorrowFrozen,
        "a book too thin to liquidate against must stop new borrowing"
    );
}

#[test]
fn crash_03_a_thin_book_cannot_be_used_to_raise_the_price() {
    // The other direction: an attacker who drains a pool to make it cheap to
    // move must not then be able to move it.
    let mut f = Fixture::new();
    f.init();
    f.bootstrap();
    let before = f.accepted();

    // Thin, and priced far above the reference.
    let thin_collateral = f.collateral_reserve / 10_000;
    let thin_quote = f.quote_reserve / 100;
    f.set_both_pools(thin_collateral, thin_quote);
    for _ in 0..20 {
        f.observe_after(90);
    }

    assert!(
        f.accepted() <= before,
        "a book below the depth floor raised the reference from {before} to {}",
        f.accepted()
    );
}

// ===========================================================================
// Policy B: advance to the bound rather than hold
// ===========================================================================

#[test]
fn pol_00_a_real_rise_converges_instead_of_deadlocking() {
    /*
     * Why Policy B (advance to the bound) and not Policy A (hold entirely).
     *
     * Under Policy A a price that genuinely doubles is never reached: the
     * reference holds, every reading is "too high", and it holds forever. The
     * asset is permanently undervalued and the market never reopens. Policy B
     * walks to the truth in capped steps.
     */
    let mut f = Fixture::new();
    f.init();
    f.bootstrap();
    let before = f.accepted();

    let (c, q) = (f.collateral_reserve, f.quote_reserve);
    let doubled = f.price_of(c, q * 2);
    f.set_both_pools(c, q * 2);

    for _ in 0..80 {
        f.observe_after(90);
    }

    let after = f.accepted();
    assert!(after > before, "the reference never moved off {before}");
    // Within 1% of the true price: converged, not merely nudged.
    let gap = doubled.abs_diff(after) * 10_000 / doubled;
    assert!(
        gap < 100,
        "after a genuine doubling the reference settled at {after} against a true \
         price of {doubled} ({gap} bps away); Policy B must converge"
    );
}

#[test]
fn pol_01_a_manipulation_that_reverses_leaves_the_reference_near_the_truth() {
    /*
     * The cost of Policy B, measured rather than asserted away.
     *
     * Advancing to the bound means a manipulation that is later abandoned has
     * left the reference somewhat high. That is real, and it is bounded: the
     * reference must come back down once the pools do, because falls are
     * uncapped.
     */
    let mut f = Fixture::new();
    f.init();
    f.bootstrap();
    let honest = f.accepted();

    let (c, q) = (f.collateral_reserve, f.quote_reserve);
    move_pool_price_pct(&mut f.env.svm, &f.pool_a, c, q, 900);
    move_pool_price_pct(&mut f.env.svm, &f.pool_b, c, q, 900);
    for _ in 0..10 {
        f.observe_after(61);
    }
    let peak = f.accepted();
    assert!(
        peak > honest,
        "the setup did not actually move the reference"
    );

    // The attacker stops paying. The pools revert.
    f.set_both_pools(c, q);
    for _ in 0..40 {
        f.observe_after(90);
    }

    let settled = f.accepted();
    assert!(
        settled < peak,
        "the reference stayed at the manipulated {peak}"
    );
    let gap = honest.abs_diff(settled) * 10_000 / honest;
    assert!(
        gap < 100,
        "after the manipulation reversed, the reference settled at {settled} \
         against the honest {honest} ({gap} bps away)"
    );
}

// ===========================================================================
// Staleness
// ===========================================================================

#[test]
fn stale_00_an_unrefreshed_oracle_degrades_and_recovers() {
    /*
     * Nobody cranking is itself a risk signal. It ages Healthy -> RateWarning
     * -> BorrowFrozen, and one good observation restores it -- there is no
     * admin step, because there is nothing an admin knows that the next
     * observation does not.
     */
    let mut f = Fixture::new();
    f.init();
    f.bootstrap();
    assert_eq!(health(f.health()), OracleHealth::Healthy);

    // Past `warn_age_seconds` (300) but inside `max_observation_age_seconds`.
    f.observe_after(400);
    assert_eq!(
        health(f.health()),
        OracleHealth::RateWarning,
        "a 400-second gap did not register as a warning"
    );

    // Past `max_observation_age_seconds` (900).
    f.observe_after(1_000);
    assert_eq!(
        health(f.health()),
        OracleHealth::BorrowFrozen,
        "a 1000-second gap did not stop new borrowing"
    );

    f.observe_after(60);
    assert_eq!(
        health(f.health()),
        OracleHealth::Healthy,
        "a fresh observation did not restore the oracle"
    );
}

#[test]
fn stale_01_staleness_does_not_prevent_a_markdown() {
    // "We have not heard recently" must never be a reason to hold a higher
    // price than the market is showing.
    let mut f = Fixture::new();
    f.init();
    f.bootstrap();
    let before = f.accepted();

    let (c, q) = (f.collateral_reserve, f.quote_reserve);
    f.set_both_pools(c, q / 4);
    // A single observation after a long silence: stale *and* much lower.
    f.observe_after(5_000);

    assert!(
        f.accepted() < before,
        "a stale-but-lower reading was refused; the reference stayed at {before}"
    );
    assert_eq!(health(f.health()), OracleHealth::BorrowFrozen);
}

// ===========================================================================
// The ring buffer
// ===========================================================================

#[test]
fn ring_00_the_buffer_wraps_without_losing_ordering() {
    /*
     * 32 slots, then overwrite in place. The TWAP sorts by timestamp rather
     * than trusting insertion order, so a wrapped buffer must still produce a
     * monotone history and a sane average.
     */
    let mut f = Fixture::new();
    f.init();
    f.refresh().expect("first");
    for _ in 0..45 {
        f.observe_after(61);
    }

    let oracle = f.market_oracle();
    assert_eq!(
        oracle.observations.len(),
        aera::state::OBSERVATION_CAPACITY,
        "the buffer grew past its capacity"
    );

    let ordered = oracle.ordered();
    assert_eq!(ordered.len(), aera::state::OBSERVATION_CAPACITY);
    for pair in ordered.windows(2) {
        assert!(
            pair[0].unix_timestamp <= pair[1].unix_timestamp,
            "the wrapped buffer did not order by time"
        );
    }
    // The oldest surviving entry must be recent: 32 slots at 61s is ~32 minutes.
    let now = f.env.unix_timestamp();
    assert!(
        now - ordered[0].unix_timestamp < 46 * 61,
        "the buffer is holding entries older than the writes that should have \
         evicted them"
    );
}

#[test]
fn ring_01_a_wrapped_buffer_still_bounds_a_pump() {
    // The cap and the min combiner must survive wraparound; an off-by-one in
    // the index arithmetic would be invisible until exactly here.
    let mut f = Fixture::new();
    f.init();
    f.refresh().expect("first");
    for _ in 0..45 {
        f.observe_after(61);
    }
    let before = f.accepted();

    let (c, q) = (f.collateral_reserve, f.quote_reserve);
    move_pool_price_pct(&mut f.env.svm, &f.pool_a, c, q, 900);
    f.observe_after(90);

    assert!(
        f.accepted() <= before,
        "after wraparound, a one-pool pump raised the reference from {before} to {}",
        f.accepted()
    );
}

// ===========================================================================
// Decimals
// ===========================================================================

/// The price must be right for any decimal pair, not just 6/9.
///
/// This is where the overflow that the first version of this instruction
/// shipped with lived: computing `quote * 10^collateral_decimals * 1e18` before
/// dividing overflows u128 at any realistic reserve, and a small synthetic pool
/// hides it. Wide decimals are tested for exactly that reason.
fn decimal_case(collateral_decimals: u8, quote_decimals: u8) {
    let collateral_reserve = 30_000u64 * 10u64.pow(collateral_decimals as u32);
    let quote_reserve = 850u64 * 10u64.pow(quote_decimals as u32);

    let (mut env, cook, _bcook) = Env::core(1_000);
    let collateral_mint = env
        .add_market_reserve(collateral_decimals, bcook_config())
        .mint;

    let pool_a = create_mock_damm_pool(
        &mut env.svm,
        &PoolSpec::new(
            Pubkey::new_from_array([31u8; 32]),
            collateral_mint,
            cook.mint,
            collateral_reserve,
            quote_reserve,
        ),
    );
    let pool_b = create_mock_damm_pool(
        &mut env.svm,
        &PoolSpec::new(
            Pubkey::new_from_array([32u8; 32]),
            collateral_mint,
            cook.mint,
            collateral_reserve,
            quote_reserve,
        )
        .orientation(Orientation::QuoteFirst),
    );
    set_amm_program_data(&mut env.svm, AMM_PROGRAM_DATA, AMM_DEPLOY_SLOT);

    let refs = [
        aera::state::PoolRef {
            pool: pool_a.pool,
            collateral_vault: pool_a.collateral_vault,
            quote_vault: pool_a.quote_vault,
        },
        aera::state::PoolRef {
            pool: pool_b.pool,
            collateral_vault: pool_b.collateral_vault,
            quote_vault: pool_b.quote_vault,
        },
    ];
    env.init_market_oracle(
        collateral_mint,
        collateral_mint,
        cook.mint,
        collateral_decimals,
        quote_decimals,
        refs,
        test_market_config(),
    )
    .expect("init");

    let payer = env.admin.insecure_clone();
    env.try_refresh_market_oracle_as(&payer, collateral_mint, &pool_a, &pool_b, AMM_PROGRAM_DATA)
        .unwrap_or_else(|cause| {
            panic!("refresh at {collateral_decimals}/{quote_decimals} decimals: {cause}")
        });

    let accepted = env.read_oracle(collateral_mint).reference.effective_rate;
    let expected = expected_price(
        collateral_reserve,
        quote_reserve,
        collateral_decimals,
        quote_decimals,
    );
    assert_eq!(
        accepted, expected,
        "price wrong at {collateral_decimals}/{quote_decimals} decimals"
    );
    // 850 quote per 30,000 collateral is 0.02833..., scaled by 1e18. The same
    // number whatever the decimals, which is the entire point of normalising.
    assert_eq!(
        accepted, 28_333_333_333_333_333,
        "the normalised price changed with the decimal pair"
    );
}

#[test]
fn dcm_00_six_over_nine_the_real_cookhouse_pair() {
    decimal_case(6, 9);
}

#[test]
fn dcm_01_matching_decimals() {
    decimal_case(9, 9);
}

#[test]
fn dcm_02_collateral_wider_than_quote() {
    decimal_case(9, 6);
}

#[test]
fn dcm_03_zero_decimal_collateral() {
    decimal_case(0, 9);
}

#[test]
fn dcm_04_the_widest_pair_that_fits_a_u64_reserve() {
    // 12 decimals with a 30,000-token reserve is 3e16 base units; the u128
    // intermediate is the thing under test, not the token.
    decimal_case(12, 6);
}

// ===========================================================================
// The deviation threshold itself
// ===========================================================================

#[test]
fn dev_00_disagreement_below_the_threshold_is_not_a_degradation() {
    // The pools always differ a little -- the live COOKHOUSE pair measured 88
    // bps apart. A threshold that fired on ordinary divergence would freeze the
    // market permanently, which is the failure mode `oracle-calibrate.ts`
    // exists to prevent.
    let mut f = Fixture::new();
    f.init();
    f.bootstrap();

    let (c, q) = (f.collateral_reserve, f.quote_reserve);
    // Well inside the 300 bps limit.
    move_pool_price_pct(&mut f.env.svm, &f.pool_a, c, q, MAX_DEVIATION_BPS / 200);
    f.observe_after(90);

    assert_eq!(
        health(f.health()),
        OracleHealth::Healthy,
        "ordinary cross-pool divergence was treated as an attack"
    );
}
