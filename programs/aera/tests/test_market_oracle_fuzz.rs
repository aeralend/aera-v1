//! Randomised market conditions against the Tier 3 oracle's invariants.
//!
//! `test_market_oracle_attacks` asks specific questions. This asks the same
//! question thousands of times with a market that moves arbitrarily: pumps,
//! crashes, one pool moving alone, liquidity leaving, refreshes at every
//! spacing from a single slot to hours, and refreshes that are refused.
//!
//! What it is looking for is the case nobody thought to write down. Every
//! documented property of the oracle is restated here as an invariant checked
//! after **every** transition, so a sequence that breaks one is reported with
//! the seed and the step that did it.
//!
//! It is deliberately its own suite. `test_fuzz_deep` already runs for six
//! minutes; folding this into it would make the fast signal slower without
//! making either clearer.

mod common;

use aera::oracle::breaker::OracleHealth;
use aera::state::{MarketOracle, OBSERVATION_CAPACITY};
use common::damm::*;
use common::market::*;

/// `max_rise_bps_per_window` in `test_market_config`.
const RISE_CAP_BPS: u128 = 1_000;
/// `min_spacing_seconds` in `test_market_config`.
const MIN_SPACING: i64 = 60;

/// A deterministic LCG, so a failure reproduces from its seed alone.
///
/// Numerical Recipes' constants. Nothing here needs statistical quality -- it
/// needs to be reproducible and to reach the awkward corners, and a named seed
/// in a failure message is worth more than a better generator.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        self.0
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound.max(1)
    }

    /// An inclusive range.
    fn between(&mut self, low: i64, high: i64) -> i64 {
        low + self.below((high - low + 1) as u64) as i64
    }
}

/// Everything observable about the oracle after one transition.
#[derive(Clone, Copy, Debug)]
struct Snapshot {
    accepted: u128,
    health: u8,
    rows: usize,
    next_index: u8,
    newest: i64,
}

fn snapshot(f: &Fixture) -> Snapshot {
    let market = f.market_oracle();
    Snapshot {
        accepted: f.accepted(),
        health: f.health(),
        rows: market.observations.len(),
        next_index: market.next_index,
        newest: market.latest().map(|o| o.unix_timestamp).unwrap_or(0),
    }
}

/// Every invariant the design claims, restated as a check.
///
/// `context` names the seed and step so a failure is reproducible rather than
/// merely observed.
fn check_invariants(
    market: &MarketOracle,
    before: Snapshot,
    after: Snapshot,
    degraded: bool,
    context: &str,
) {
    // --- 1. the buffer stays a buffer ------------------------------------
    assert!(
        after.rows <= OBSERVATION_CAPACITY,
        "{context}: the ring buffer holds {} entries, past its {OBSERVATION_CAPACITY} capacity",
        after.rows
    );
    assert!(
        (after.next_index as usize) < OBSERVATION_CAPACITY,
        "{context}: next_index {} is outside the buffer",
        after.next_index
    );
    assert!(
        after.rows >= before.rows,
        "{context}: the buffer shrank from {} to {}",
        before.rows,
        after.rows
    );
    assert!(
        after.rows <= before.rows + 1,
        "{context}: one refresh appended {} entries",
        after.rows - before.rows
    );

    // --- 2. the history stays ordered and spread -------------------------
    let ordered = market.ordered();
    for pair in ordered.windows(2) {
        assert!(
            pair[0].unix_timestamp <= pair[1].unix_timestamp,
            "{context}: the history is not ordered by time"
        );
    }
    // Consecutive entries respect the spacing rule. This is the observation-spam
    // defence, and it is the property a wrapped buffer is most likely to break.
    for pair in ordered.windows(2) {
        let gap = pair[1].unix_timestamp - pair[0].unix_timestamp;
        assert!(
            gap >= MIN_SPACING,
            "{context}: two observations are {gap}s apart, inside the {MIN_SPACING}s minimum"
        );
    }
    for observation in &ordered {
        assert!(
            observation.price > 0,
            "{context}: a zero price was recorded"
        );
    }

    // --- 3. the reference is always a price ------------------------------
    assert!(after.accepted > 0, "{context}: the reference fell to zero");

    // --- 4. rises are rationed -------------------------------------------
    if after.accepted > before.accepted {
        let recorded = after.rows > before.rows || after.newest > before.newest;
        assert!(
            recorded,
            "{context}: the reference rose from {} to {} on a refresh that recorded \
             nothing; the rise cap is only a cost while steps are rationed by the \
             spacing rule",
            before.accepted, after.accepted
        );
        assert!(
            !degraded,
            "{context}: the reference rose from {} to {} while the information was \
             degraded (pools disagreeing, or a book below the depth floor)",
            before.accepted, after.accepted
        );

        let moved = (after.accepted - before.accepted) * 10_000 / before.accepted;
        assert!(
            moved <= RISE_CAP_BPS,
            "{context}: the reference rose {moved} bps in one step, past the \
             {RISE_CAP_BPS} bps cap ({} -> {})",
            before.accepted,
            after.accepted
        );
    }

    // --- 5. the reference never exceeds the history it averages ----------
    //
    // The TWAP is an average of recorded minima, so it cannot exceed the
    // largest of them. A reference above every reading in the buffer would mean
    // the breaker invented a price.
    if let Some(highest) = ordered.iter().map(|o| o.price).max() {
        assert!(
            after.accepted <= highest,
            "{context}: the reference is {} but the highest recorded observation is \
             {highest}",
            after.accepted
        );
    }

    // --- 6. bootstrap outranks everything --------------------------------
    let health = OracleHealth::from_u8(after.health).expect("health decodes");
    if !market.is_bootstrapped() {
        assert_eq!(
            health,
            OracleHealth::Bootstrapping,
            "{context}: an unbootstrapped market reported {health:?}, which would \
             permit new debt against history that spans no elapsed time"
        );
    }

    // --- 7. degraded information never reports Healthy -------------------
    if degraded && market.is_bootstrapped() {
        assert!(
            health != OracleHealth::Healthy,
            "{context}: the two pools disagree or a book is below the depth floor, \
             and the oracle still reports Healthy"
        );
    }
}

/// What a run actually exercised.
///
/// Without this the suite is a liar. A fuzz whose invariants are never reached
/// passes for the same reason an empty test passes, and every change that
/// weakens the generator makes it pass harder. These counts are asserted at the
/// end of a long run, so a market that stopped producing degraded readings or
/// stopped wrapping its buffer fails loudly rather than going quiet.
#[derive(Default, Debug)]
struct Coverage {
    recorded: usize,
    skipped: usize,
    refused: usize,
    degraded: usize,
    shallow: usize,
    disagree: usize,
    healthy: usize,
    rises: usize,
    falls: usize,
    wrapped: bool,
    unbootstrapped: usize,
}

/// One randomised run.
fn run(seed: u64, steps: usize) -> Coverage {
    let mut rng = Rng(seed);
    let mut f = Fixture::new();
    f.init();

    // The reserves each pool currently holds, tracked so a move is applied to
    // the pool's real state rather than to the fixture's starting values.
    let (mut ca, mut qa) = (f.collateral_reserve, f.quote_reserve);
    let (mut cb, mut qb) = (f.collateral_reserve, f.quote_reserve);

    f.refresh().expect("the first observation");
    let mut coverage = Coverage::default();

    for step in 0..steps {
        let context = format!("seed {seed}, step {step}");

        /*
         * Move the market.
         *
         * The weighting is the whole design of this generator, and the first
         * version got it wrong in a way only the coverage gate caught: it moved
         * the two pools independently and never brought them back, so within a
         * few steps they disagreed permanently, every reading was degraded, the
         * reference could never rise, and the rise-cap invariant was checked
         * exactly zero times in three thousand steps.
         *
         * A real pair of AMMs holding the same asset is arbitraged. Divergence
         * happens and then closes. So: mostly a correlated move that keeps the
         * books together, sometimes one pool alone, and sometimes an arbitrage
         * that pulls them back into line.
         */
        /*
         * Symmetric in RATIO, not in percent.
         *
         * `between(-90, 900)` looks balanced and is not: its mean ratio is well
         * above 1, so a long run drifts upward and the reference only ever
         * climbs. The coverage gate caught it as `falls: 0` over a thousand
         * steps of a market that was supposedly moving both ways.
         *
         * A +100% move and a -50% move are the same size; this pairs them.
         */
        let size = match rng.below(10) {
            0..=5 => rng.between(1, 15),
            6..=8 => rng.between(15, 80),
            _ => rng.between(80, 900),
        };
        let magnitude = if rng.below(2) == 0 {
            size
        } else {
            -(size * 100 / (100 + size))
        };

        /*
         * A small one-pool move: ordinary divergence, not an attack.
         *
         * The live COOKHOUSE pair measured ~88 bps apart. A generator whose
         * every divergence is 1500 bps spends its whole run degraded -- 92% of
         * steps, in the version before this -- and the healthy paths go
         * untested. Both sizes need to appear.
         */
        let drift = rng.between(-4, 4);

        match rng.below(20) {
            // One pool alone at attack scale: what the `min` combiner defeats.
            0 => {
                move_pool_price_pct(&mut f.env.svm, &f.pool_a, ca, qa, magnitude);
                (ca, qa) = moved(ca, qa, magnitude);
            }
            1 => {
                move_pool_price_pct(&mut f.env.svm, &f.pool_b, cb, qb, magnitude);
                (cb, qb) = moved(cb, qb, magnitude);
            }
            // One pool alone at ordinary scale: inside the deviation limit, or
            // just outside it, which is where the threshold is actually decided.
            2..=4 => {
                move_pool_price_pct(&mut f.env.svm, &f.pool_a, ca, qa, drift);
                (ca, qa) = moved(ca, qa, drift);
            }
            5..=6 => {
                move_pool_price_pct(&mut f.env.svm, &f.pool_b, cb, qb, drift);
                (cb, qb) = moved(cb, qb, drift);
            }
            // Arbitrage: both books to the midpoint of their two prices.
            7..=10 => {
                let target = (f.price_of(ca, qa) + f.price_of(cb, qb)) / 2;
                (ca, qa) = at_price(ca, qa, target, &f);
                (cb, qb) = at_price(cb, qb, target, &f);
                set_pool_reserves(&mut f.env.svm, &f.pool_a, ca, qa);
                set_pool_reserves(&mut f.env.svm, &f.pool_b, cb, qb);
            }
            // The ordinary case: the market moves and both books follow.
            _ => {
                move_pool_price_pct(&mut f.env.svm, &f.pool_a, ca, qa, magnitude);
                move_pool_price_pct(&mut f.env.svm, &f.pool_b, cb, qb, magnitude);
                (ca, qa) = moved(ca, qa, magnitude);
                (cb, qb) = moved(cb, qb, magnitude);
            }
        }

        /*
         * Occasionally drain a book rather than move its price, which is what
         * liquidity leaving during a crash looks like.
         *
         * Bounded at a 20x drain: draining to nothing makes the pool
         * unreadable, and a run whose refreshes are mostly refused tests the
         * refusal path over and over and everything else not at all.
         */
        if rng.below(25) == 0 {
            let divisor = 2 + rng.below(19);
            ca = (ca / divisor).max(1_000);
            qa = (qa / divisor).max(1_000);
            set_pool_reserves(&mut f.env.svm, &f.pool_a, ca, qa);
        }

        /*
         * And liquidity comes back.
         *
         * Without this the run was a one-way ratchet: the first drain put pool
         * A permanently under the depth floor, so 91% of steps were degraded
         * and the healthy paths went untested. Books recover after a crash, and
         * a generator that never lets them recover is not modelling a market.
         */
        if rng.below(6) == 0 {
            let multiplier = 2 + rng.below(19);
            ca = ca.saturating_mul(multiplier).min(f.collateral_reserve * 4);
            qa = qa.saturating_mul(multiplier).min(f.quote_reserve * 4);
            set_pool_reserves(&mut f.env.svm, &f.pool_a, ca, qa);
        }

        /*
         * And sometimes the market simply returns to normal.
         *
         * Real books are replenished, and without this the run is a slow
         * ratchet into permanent degradation: a large fall takes quote-side
         * depth with it, several falls compound, and the pool never comes back
         * above `min_pool_quote_depth`. That left 92% of steps degraded and the
         * healthy paths -- an ordinary rise, an ordinary Healthy verdict --
         * barely reached.
         */
        if rng.below(15) == 0 {
            ca = f.collateral_reserve;
            qa = f.quote_reserve;
            cb = f.collateral_reserve;
            qb = f.quote_reserve;
            f.set_both_pools(ca, qa);
        }

        /*
         * Choose when to refresh, spanning both sides of the spacing rule.
         *
         * Zero and one slot are included on purpose: a refresh inside the
         * spacing window must succeed, restate freshness, and append nothing.
         */
        let wait = match rng.below(10) {
            0..=2 => 0,
            3..=5 => rng.between(1, 59),
            6..=8 => rng.between(60, 400),
            _ => rng.between(400, 4_000),
        };
        if wait == 0 {
            // A distinct blockhash without moving the clock second, so the
            // transaction is not deduplicated as AlreadyProcessed.
            f.env.warp_slots(1);
        } else {
            f.env.warp_seconds(wait);
        }

        let before = snapshot(&f);
        let degraded = is_degraded(ca, qa, cb, qb, &f);

        if degraded {
            coverage.degraded += 1;
        }
        let config = f.market_oracle().config;
        if qa < config.min_pool_quote_depth || qb < config.min_pool_quote_depth {
            coverage.shallow += 1;
        }
        if degraded && qa >= config.min_pool_quote_depth && qb >= config.min_pool_quote_depth {
            coverage.disagree += 1;
        }
        if !degraded {
            coverage.healthy += 1;
        }

        if f.refresh().is_err() {
            coverage.refused += 1;
            // A refusal is a legitimate outcome -- an empty pool, say. What it
            // must never be is a silent state change.
            let after = snapshot(&f);
            assert_eq!(
                after.rows, before.rows,
                "{context}: a refused refresh still appended to the history"
            );
            assert_eq!(
                after.accepted, before.accepted,
                "{context}: a refused refresh still moved the reference"
            );
            continue;
        }

        let after = snapshot(&f);
        let market = f.market_oracle();
        if after.rows > before.rows || after.newest > before.newest {
            coverage.recorded += 1;
        } else {
            coverage.skipped += 1;
        }
        if after.accepted > before.accepted {
            coverage.rises += 1;
        }
        if after.accepted < before.accepted {
            coverage.falls += 1;
        }
        if market.observations.len() == OBSERVATION_CAPACITY {
            coverage.wrapped = true;
        }
        if !market.is_bootstrapped() {
            coverage.unbootstrapped += 1;
        }

        check_invariants(&market, before, after, degraded, &context);
    }

    coverage
}

/// Assert a long run reached every state the invariants are about.
fn require_coverage(seed: u64, coverage: &Coverage) {
    let need = |what: &str, count: usize| {
        assert!(
            count > 0,
            "seed {seed}: the run never produced {what}, so the invariant about it              was never checked. Coverage: {coverage:?}"
        );
    };
    // Printed so a reader can see what a run actually reaches without having to
    // make one fail to find out.
    eprintln!("seed {seed}: {coverage:?}");
    need("a recorded observation", coverage.recorded);
    need("a refresh inside the spacing window", coverage.skipped);
    need("degraded information", coverage.degraded);
    need("a rise in the reference", coverage.rises);
    need("a fall in the reference", coverage.falls);
    need("an unbootstrapped state", coverage.unbootstrapped);
    need("a thin book", coverage.shallow);
    need(
        "pools disagreeing while both books are deep",
        coverage.disagree,
    );
    need("an undegraded reading", coverage.healthy);
    assert!(
        coverage.wrapped,
        "seed {seed}: the ring buffer never filled, so wraparound was never          exercised. Coverage: {coverage:?}"
    );
}

/// Reserves holding the same collateral side but priced at `target`.
///
/// How an arbitrageur leaves a pool: the collateral is unchanged and the quote
/// side is whatever that price implies.
fn at_price(collateral: u64, _quote: u64, target: u128, f: &Fixture) -> (u64, u64) {
    // price = quote * SCALE * 10^(cd-qd) / collateral, so invert for quote.
    let mut low = 1u64;
    let mut high = u64::MAX / 2;
    while low < high {
        let mid = low + (high - low) / 2;
        if f.price_of(collateral, mid) < target {
            low = mid + 1;
        } else {
            high = mid;
        }
    }
    (collateral, low.max(1))
}

/// The reserves after `move_pool_price_pct`, mirroring its arithmetic.
fn moved(collateral: u64, quote: u64, pct: i64) -> (u64, u64) {
    let root = (1.0 + (pct as f64) / 100.0).sqrt();
    (
        (((collateral as f64) / root) as u64).max(1),
        (((quote as f64) * root) as u64).max(1),
    )
}

/// Whether the program would flag this market as degraded.
///
/// Recomputed from the reserves the run is tracking rather than read back from
/// the event, so the invariant check does not depend on the same code path it
/// is checking.
fn is_degraded(ca: u64, qa: u64, cb: u64, qb: u64, f: &Fixture) -> bool {
    let config = f.market_oracle().config;
    if qa < config.min_pool_quote_depth || qb < config.min_pool_quote_depth {
        return true;
    }
    let pa = f.price_of(ca, qa);
    let pb = f.price_of(cb, qb);
    let sum = pa + pb;
    if sum == 0 {
        return true;
    }
    let deviation = pa.abs_diff(pb) * 2 * 10_000 / sum;
    deviation > config.max_cross_pool_deviation_bps as u128
}

#[test]
fn fuzz_00_a_thousand_random_market_moves() {
    let seed = 1;
    require_coverage(seed, &run(seed, 1_000));
}

#[test]
fn fuzz_01_a_second_seed() {
    let seed = 0xC00C1E;
    require_coverage(seed, &run(seed, 1_000));
}

#[test]
fn fuzz_02_a_third_seed() {
    let seed = 0xAE2A;
    require_coverage(seed, &run(seed, 1_000));
}

#[test]
fn fuzz_03_short_runs_from_many_seeds() {
    /*
     * Breadth rather than depth: the first few steps of a market's life are
     * where bootstrap, the empty buffer and the first reference all interact,
     * and a long run only visits that once.
     *
     * Coverage is required of the aggregate rather than of each short run --
     * thirty steps cannot fill a 32-slot buffer, and demanding it of each would
     * only teach the suite to run longer.
     */
    let mut total = Coverage::default();
    for seed in 0..60u64 {
        let coverage = run(seed.wrapping_mul(0x9E37_79B9), 30);
        total.recorded += coverage.recorded;
        total.skipped += coverage.skipped;
        total.refused += coverage.refused;
        total.degraded += coverage.degraded;
        total.rises += coverage.rises;
        total.falls += coverage.falls;
        total.unbootstrapped += coverage.unbootstrapped;
        total.wrapped |= coverage.wrapped;
    }
    assert!(total.recorded > 0 && total.skipped > 0 && total.degraded > 0);
    assert!(
        total.unbootstrapped > 0,
        "sixty fresh markets never spent a step unbootstrapped, which cannot be          right: every one of them starts that way. Coverage: {total:?}"
    );
}
