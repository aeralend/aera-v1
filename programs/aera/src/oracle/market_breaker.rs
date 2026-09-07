//! The breaker for market-priced assets.
//!
//! Deliberately not the one in `breaker.rs`. The two oracle kinds have opposite
//! safety properties and sharing the logic would get one of them wrong:
//!
//! - A **stake-pool rate** cannot fall 40% honestly. A large fall is evidence of
//!   a fault, so refusing it and keeping the last accepted value is correct.
//! - A **market price** can fall 40% before lunch. Refusing it keeps collateral
//!   valued at a price nobody will pay, so liquidations do not fire and the loss
//!   lands on suppliers. A downside breaker on a volatile asset manufactures the
//!   bad debt it exists to prevent.
//!
//! Hence: **falls are always accepted, rises are rate-limited.**
//!
//! # The invariant everything else follows from
//!
//! > Degraded information may only ever *lower* the accepted price. It may never
//! > raise it, and it may never preserve a higher one.
//!
//! Staleness, pool disagreement and depth collapse all mean "we know less than
//! we did". None of them is a reason to keep lending against yesterday's higher
//! number. Each freezes new risk *and* still lets a lower verified price through.

use crate::oracle::breaker::OracleHealth;
use crate::state::MarketOracleConfig;

use crate::constants::BPS_DENOMINATOR;

/// Why the information behind an observation is worse than it should be.
///
/// None of these refuses a *fall*. All of them refuse a *rise*.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Degradation {
    /// The two pools disagree by more than the configured bound.
    pub pools_disagree: bool,
    /// One or both pools are thinner than the configured minimum.
    pub shallow: bool,
}

impl Degradation {
    pub fn any(self) -> bool {
        self.pools_disagree || self.shallow
    }
}

/// What the oracle should do with a new reading.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct MarketVerdict {
    /// The price the reference should now hold.
    ///
    /// Not necessarily the observed price: a rise beyond the per-window limit is
    /// admitted only up to that limit.
    pub accepted_price: u128,
    pub health: OracleHealth,
    /// Movement from the previous reference, in bps. Sign-free; `rose` says which.
    pub moved_bps: u128,
    pub rose: bool,
    /// Whether a rise was clipped to the configured bound.
    pub rise_capped: bool,
}

/// Movement between two prices as bps of the first.
fn moved_bps(from: u128, to: u128) -> u128 {
    if from == 0 {
        return 0;
    }
    let diff = from.abs_diff(to);
    diff.saturating_mul(BPS_DENOMINATOR) / from
}

/// Judge a new time-weighted price against the accepted reference.
///
/// `current` is `None` while the oracle has never accepted one, which happens
/// only during bootstrap.
///
/// `age_seconds` is the age of the newest observation, not of this call, so an
/// oracle that stopped being refreshed ages into a restriction on its own.
pub fn evaluate_market(
    current: Option<u128>,
    twap: u128,
    bootstrapped: bool,
    age_seconds: i64,
    degradation: Degradation,
    config: &MarketOracleConfig,
) -> MarketVerdict {
    let Some(current) = current else {
        /*
         * No reference yet. Take the reading and stay in Bootstrapping.
         *
         * The reference tracks from the first observation so that the moment
         * bootstrap completes there is already a sensible number, rather than a
         * market opening on a value chosen at an arbitrary later instant.
         * Bootstrapping refuses borrowing regardless, so tracking early costs
         * nothing.
         */
        return MarketVerdict {
            accepted_price: twap,
            health: OracleHealth::Bootstrapping,
            moved_bps: 0,
            rose: false,
            rise_capped: false,
        };
    };

    let rose = twap > current;
    let movement = moved_bps(current, twap);

    /*
     * Bootstrap outranks every other consideration.
     *
     * However healthy a reading looks, a market without enough history over
     * enough *elapsed time* must not permit new debt. Owned here rather than
     * left to the caller so the rule cannot be forgotten at one call site: a
     * breaker that returns Healthy for an unbootstrapped oracle is a breaker
     * that has to be corrected by everyone who uses it.
     *
     * The price still tracks -- see the `None` arm above for why.
     */
    let floor = if bootstrapped {
        OracleHealth::Healthy
    } else {
        OracleHealth::Bootstrapping
    };

    /*
     * Staleness, evaluated first because it constrains everything after it.
     *
     * An oracle nobody refreshes must not keep permitting new debt. It ages
     * Healthy -> RateWarning -> BorrowFrozen. Note this is a *floor* on the
     * restriction, not the final answer: a fall is still applied below, because
     * "we have not heard recently" is never a reason to hold a higher price.
     */
    let age_health = if age_seconds >= config.max_observation_age_seconds as i64 {
        OracleHealth::BorrowFrozen
    } else if age_seconds >= config.warn_age_seconds as i64 {
        OracleHealth::RateWarning
    } else {
        OracleHealth::Healthy
    };

    // ---------------------------------------------------------------- falls
    if !rose {
        /*
         * Accepted immediately, at any size, always.
         *
         * -1%, -60%, -95%: all applied. There is deliberately no downside cap.
         * The health may still be restricted -- by age, or by degraded
         * information -- but the *price* moves down regardless, so liquidation
         * works from the lower valuation rather than a stale higher one.
         */
        let health = if degradation.any() {
            worse(age_health, OracleHealth::BorrowFrozen)
        } else {
            age_health
        };
        return MarketVerdict {
            accepted_price: twap,
            health: worse(health, floor),
            moved_bps: movement,
            rose: false,
            rise_capped: false,
        };
    }

    // ---------------------------------------------------------------- rises
    /*
     * Degraded information can never raise the price.
     *
     * Pools that disagree, or a book too thin to price against, are exactly the
     * conditions an attacker creates. Holding the current reference is the
     * conservative answer, and new borrowing freezes.
     */
    if degradation.any() {
        return MarketVerdict {
            accepted_price: current,
            health: worse(OracleHealth::BorrowFrozen, floor),
            moved_bps: movement,
            rose: true,
            rise_capped: true,
        };
    }

    let limit = config.max_rise_bps_per_window as u128;
    if movement <= limit {
        return MarketVerdict {
            accepted_price: twap,
            health: worse(age_health, floor),
            moved_bps: movement,
            rose: true,
            rise_capped: false,
        };
    }

    /*
     * A rise beyond the limit is admitted only as far as the limit.
     *
     * Two policies were modelled:
     *
     *   A. hold the previous price entirely
     *   B. advance to the maximum permitted bound
     *
     * **B is implemented.** A looks safer and deadlocks: the comparison is
     * always against the frozen reference, so after any genuine spike the
     * accepted price can never catch up -- each subsequent window still measures
     * an over-limit rise against the same stale value, and the asset stays
     * permanently undervalued until the market falls back on its own.
     *
     * B converges instead. Each window admits at most `max_rise_bps_per_window`,
     * so a genuine rise is recognised over several windows and a manipulated one
     * gains at most one window's worth -- which reverts on the next honest
     * observation, because falls are immediate. The attacker's ceiling is the
     * same under both policies; only B also works when the rise is real.
     */
    let capped = current.saturating_mul(BPS_DENOMINATOR.saturating_add(limit)) / BPS_DENOMINATOR;

    MarketVerdict {
        accepted_price: capped.min(twap),
        health: worse(OracleHealth::BorrowFrozen, floor),
        moved_bps: movement,
        rose: true,
        rise_capped: true,
    }
}

/// The more restrictive of two health states.
fn worse(a: OracleHealth, b: OracleHealth) -> OracleHealth {
    if (a as u8) >= (b as u8) {
        a
    } else {
        b
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCALE: u128 = 1_000_000_000_000_000_000;

    fn config() -> MarketOracleConfig {
        MarketOracleConfig {
            amm_program: Default::default(),
            amm_program_data: Default::default(),
            expected_deploy_slot: 0,
            twap_window_seconds: 1_800,
            min_observations: 4,
            min_span_seconds: 900,
            min_spacing_seconds: 60,
            max_observation_age_seconds: 900,
            warn_age_seconds: 300,
            max_cross_pool_deviation_bps: 300,
            min_pool_quote_depth: 0,
            max_rise_bps_per_window: 1_000, // 10%
        }
    }

    #[test]
    fn a_fall_of_any_size_is_accepted_immediately() {
        for pct in [1u128, 5, 10, 20, 40, 60, 80, 95] {
            let twap = SCALE * (100 - pct) / 100;
            let v = evaluate_market(
                Some(SCALE),
                twap,
                true,
                0,
                Degradation::default(),
                &config(),
            );
            assert_eq!(
                v.accepted_price, twap,
                "a {pct}% fall must be applied in full, not clamped"
            );
        }
    }

    #[test]
    fn a_total_collapse_is_accepted() {
        // 1 raw unit. The asset is worth essentially nothing and the oracle must
        // say so, or every position against it looks solvent forever.
        let v = evaluate_market(Some(SCALE), 1, true, 0, Degradation::default(), &config());
        assert_eq!(v.accepted_price, 1);
    }

    #[test]
    fn a_rise_within_the_limit_is_accepted() {
        let twap = SCALE * 105 / 100;
        let v = evaluate_market(
            Some(SCALE),
            twap,
            true,
            0,
            Degradation::default(),
            &config(),
        );
        assert_eq!(v.accepted_price, twap);
        assert_eq!(v.health, OracleHealth::Healthy);
        assert!(!v.rise_capped);
    }

    #[test]
    fn a_rise_beyond_the_limit_is_clipped_to_the_limit_and_freezes() {
        for pct in [25u128, 50, 100, 300, 900] {
            let twap = SCALE * (100 + pct) / 100;
            let v = evaluate_market(
                Some(SCALE),
                twap,
                true,
                0,
                Degradation::default(),
                &config(),
            );
            let ceiling = SCALE * 110 / 100; // +10%
            assert_eq!(
                v.accepted_price, ceiling,
                "a +{pct}% move must be admitted only to the configured bound"
            );
            assert!(v.rise_capped);
            assert_eq!(v.health, OracleHealth::BorrowFrozen);
        }
    }

    #[test]
    fn repeated_capped_rises_converge_rather_than_deadlocking() {
        // Policy B: the reference climbs toward a genuine rise over several
        // windows. Policy A would sit at SCALE forever.
        let target = SCALE * 200 / 100;
        let mut current = SCALE;
        for _ in 0..12 {
            current = evaluate_market(
                Some(current),
                target,
                true,
                0,
                Degradation::default(),
                &config(),
            )
            .accepted_price;
        }
        assert!(
            current > SCALE * 190 / 100,
            "a genuine doubling must be reachable; got {current}"
        );
        assert!(current <= target, "must never overshoot the observed price");
    }

    #[test]
    fn degraded_information_cannot_raise_the_price() {
        for degradation in [
            Degradation {
                pools_disagree: true,
                shallow: false,
            },
            Degradation {
                pools_disagree: false,
                shallow: true,
            },
            Degradation {
                pools_disagree: true,
                shallow: true,
            },
        ] {
            let v = evaluate_market(
                Some(SCALE),
                SCALE * 105 / 100,
                true,
                0,
                degradation,
                &config(),
            );
            assert_eq!(
                v.accepted_price, SCALE,
                "worse information must never buy a higher valuation"
            );
            assert_eq!(v.health, OracleHealth::BorrowFrozen);
        }
    }

    #[test]
    fn degraded_information_can_still_lower_the_price() {
        // The invariant that keeps a market solvent: disagreement or thin books
        // are never a reason to keep lending against a higher old price.
        let lower = SCALE * 60 / 100;
        let v = evaluate_market(
            Some(SCALE),
            lower,
            true,
            0,
            Degradation {
                pools_disagree: true,
                shallow: true,
            },
            &config(),
        );
        assert_eq!(v.accepted_price, lower);
        assert_eq!(v.health, OracleHealth::BorrowFrozen);
    }

    #[test]
    fn staleness_escalates_and_still_admits_a_fall() {
        let c = config();
        let fresh = evaluate_market(Some(SCALE), SCALE, true, 0, Degradation::default(), &c);
        assert_eq!(fresh.health, OracleHealth::Healthy);

        let warn = evaluate_market(Some(SCALE), SCALE, true, 400, Degradation::default(), &c);
        assert_eq!(warn.health, OracleHealth::RateWarning);

        let frozen = evaluate_market(Some(SCALE), SCALE, true, 1_000, Degradation::default(), &c);
        assert_eq!(frozen.health, OracleHealth::BorrowFrozen);

        let falling = SCALE / 2;
        let stale_fall = evaluate_market(
            Some(SCALE),
            falling,
            true,
            1_000,
            Degradation::default(),
            &c,
        );
        assert_eq!(
            stale_fall.accepted_price, falling,
            "an old oracle must still be able to mark collateral down"
        );
    }

    #[test]
    fn the_first_observation_bootstraps_without_permitting_borrowing() {
        let v = evaluate_market(None, SCALE, false, 0, Degradation::default(), &config());
        assert_eq!(v.accepted_price, SCALE);
        assert_eq!(v.health, OracleHealth::Bootstrapping);
        assert!(!v.health.permits(crate::oracle::breaker::RiskAction::Borrow));
    }

    #[test]
    fn an_unbootstrapped_oracle_never_reports_healthy() {
        // The floor is owned by the breaker, not by its callers. A breaker that
        // returned Healthy here would have to be corrected at every call site.
        let c = config();
        for twap_pct in [50u128, 100, 105, 300] {
            for age in [0i64, 400, 1_000] {
                for degradation in [
                    Degradation::default(),
                    Degradation {
                        pools_disagree: true,
                        shallow: true,
                    },
                ] {
                    let v = evaluate_market(
                        Some(SCALE),
                        SCALE * twap_pct / 100,
                        false,
                        age,
                        degradation,
                        &c,
                    );
                    assert_ne!(
                        v.health,
                        OracleHealth::Healthy,
                        "unbootstrapped oracle reported Healthy at {twap_pct}% / age {age}"
                    );
                    assert!(
                        !v.health.permits(crate::oracle::breaker::RiskAction::Borrow),
                        "unbootstrapped oracle permitted borrowing"
                    );
                }
            }
        }
    }

    /// The §35 invariant, as a property.
    #[test]
    fn worse_information_never_increases_the_accepted_price() {
        let c = config();
        for twap_pct in [50u128, 80, 95, 100, 105, 120, 300] {
            let twap = SCALE * twap_pct / 100;
            let clean = evaluate_market(Some(SCALE), twap, true, 0, Degradation::default(), &c)
                .accepted_price;
            for degradation in [
                Degradation {
                    pools_disagree: true,
                    shallow: false,
                },
                Degradation {
                    pools_disagree: false,
                    shallow: true,
                },
                Degradation {
                    pools_disagree: true,
                    shallow: true,
                },
            ] {
                for age in [0i64, 400, 1_000] {
                    let degraded = evaluate_market(Some(SCALE), twap, true, age, degradation, &c)
                        .accepted_price;
                    assert!(
                        degraded <= clean,
                        "degraded info produced a HIGHER price ({degraded} > {clean})"
                    );
                }
            }
        }
    }
}
