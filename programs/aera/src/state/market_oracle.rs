//! Observation history for a market-priced asset.
//!
//! A separate account from `OracleState`, deliberately. `OracleState` is read by
//! Core's two oracles and by every risk-sensitive instruction; growing it to
//! carry a ring buffer would leave v0.2 oracle accounts undeserialisable to a
//! v0.3 program, which is the same trap `migrate.rs:145` records for `Reserve`.
//!
//! So `OracleState` stays exactly as it is and keeps its role — the accepted
//! reference, the health state, the deployment pin, the interface the rest of
//! the program reads. This account holds only what a market price additionally
//! needs: the pools it may be derived from, the thresholds it must satisfy, and
//! the observations it is averaged over.
//!
//! # The trust boundary
//!
//! Nobody supplies a price. `refresh_market_oracle` is handed pool accounts, and
//! the program reads their reserves itself. What a caller chooses is *when* to
//! sample, which is why the bootstrap and TWAP rules below are written in terms
//! of elapsed time rather than observation count: ten samples in ten consecutive
//! slots must not look like ten minutes of price history.

use anchor_lang::prelude::*;

use crate::errors::AeraError;

/// How many observations the ring buffer holds.
///
/// Fixed rather than configurable so the account size is known at compile time
/// and every market's history costs the same rent. Thirty-two observations at a
/// five-minute minimum spacing spans over two hours, which is longer than any
/// TWAP window worth configuring for an asset this volatile.
pub const OBSERVATION_CAPACITY: usize = 32;

/// One verified reading. Every field is derived by the program.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Default, InitSpace, Debug)]
pub struct Observation {
    /// Combined price, 1e18-scaled, in quote units per whole collateral token.
    pub price: u128,
    /// From the `Clock` sysvar, never from the caller.
    pub slot: u64,
    pub unix_timestamp: i64,
    /// Total quote-side depth across both pools when this was taken.
    ///
    /// Recorded so a later reader can tell whether a price was set against a
    /// deep book or a thin one, which is the difference between a real move and
    /// one somebody bought.
    pub quote_depth: u64,
}

/// The pools a market oracle may derive its price from.
///
/// Per-oracle, not global. A second Tier 3 market has its own pools, and generic
/// code must never carry one asset's addresses.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, InitSpace, Debug)]
pub struct PoolRef {
    pub pool: Pubkey,
    /// The pool's vault for the collateral asset, as named by the pool account.
    pub collateral_vault: Pubkey,
    /// The pool's vault for the quote asset.
    pub quote_vault: Pubkey,
}

/// Thresholds a market price must satisfy before it may back a loan.
///
/// Every one is configuration rather than a constant. Reusing bCOOK's breaker
/// bounds here would be wrong by orders of magnitude: those are 200 bps up and
/// 100 bps down per *stake-pool epoch*, roughly 53 hours, and a memecoin moves
/// that much before lunch.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, InitSpace, Debug)]
pub struct MarketOracleConfig {
    /// The AMM whose pools these are. Checked as the owner of every pool account.
    pub amm_program: Pubkey,
    /// The AMM's `ProgramData`, and the deploy slot it stood at when configured.
    ///
    /// The AMM is upgradeable by a key Aera does not control. Aera cannot stop
    /// that; it can refuse to keep pricing against an implementation it has not
    /// seen. Same mechanism as the stake-pool pin on `OracleState`.
    pub amm_program_data: Pubkey,
    pub expected_deploy_slot: u64,

    /// Seconds the TWAP averages over.
    pub twap_window_seconds: u32,
    /// Observations required before borrowing may be enabled.
    pub min_observations: u8,
    /// Seconds those observations must span. Ten samples in ten slots is one
    /// sample, however many rows it writes.
    pub min_span_seconds: u32,
    /// Minimum gap between accepted observations, which is what stops a caller
    /// filling the window from one short manipulation.
    pub min_spacing_seconds: u32,
    /// Older than this and the reference stops being usable.
    pub max_observation_age_seconds: u32,
    /// Age at which the oracle warns before it freezes.
    pub warn_age_seconds: u32,

    /// Maximum tolerated disagreement between the two pools, in bps of their
    /// midpoint. Live COOKHOUSE deviation was measured at ~88 bps, so this is
    /// not a formality.
    pub max_cross_pool_deviation_bps: u16,
    /// Minimum quote-side depth per pool, in quote base units.
    pub min_pool_quote_depth: u64,
    /// The most the reference may *rise* per TWAP window, in bps.
    ///
    /// There is deliberately no downward equivalent. See
    /// `OracleSourceKind::falls_are_always_accepted`.
    pub max_rise_bps_per_window: u16,
}

impl MarketOracleConfig {
    /// Protocol ceilings, so a misconfiguration cannot disable a control.
    ///
    /// These bound what an admin may configure at all. They are not the
    /// recommended values -- those have to be modelled from the asset's own
    /// history, which is a separate exercise.
    pub const MAX_DEVIATION_BPS: u16 = 1_000; // 10%; beyond this the pools are not pricing the same thing
    pub const MAX_AGE_SECONDS: u32 = 3_600; // an hour-old price cannot back new debt
    pub const MAX_RISE_BPS: u16 = 5_000; // 50% per window; above this the limiter is decorative
    pub const MIN_SPAN_SECONDS: u32 = 300; // five minutes of history, minimum
    pub const MAX_TWAP_WINDOW_SECONDS: u32 = 86_400;

    pub fn validate(&self) -> Result<()> {
        require!(
            self.max_cross_pool_deviation_bps > 0
                && self.max_cross_pool_deviation_bps <= Self::MAX_DEVIATION_BPS,
            AeraError::InvalidOracleConfig
        );
        require!(
            self.max_observation_age_seconds > 0
                && self.max_observation_age_seconds <= Self::MAX_AGE_SECONDS,
            AeraError::InvalidOracleConfig
        );
        require!(
            self.max_rise_bps_per_window > 0 && self.max_rise_bps_per_window <= Self::MAX_RISE_BPS,
            AeraError::InvalidOracleConfig
        );
        require!(
            self.min_span_seconds >= Self::MIN_SPAN_SECONDS,
            AeraError::InvalidOracleConfig
        );
        require!(
            self.twap_window_seconds > 0
                && self.twap_window_seconds <= Self::MAX_TWAP_WINDOW_SECONDS,
            AeraError::InvalidOracleConfig
        );
        require!(
            self.min_observations >= 2 && (self.min_observations as usize) <= OBSERVATION_CAPACITY,
            AeraError::InvalidOracleConfig
        );
        // Spacing must leave room for the required observations inside the span,
        // or bootstrap is unsatisfiable and the market can never open.
        require!(
            self.min_spacing_seconds > 0
                && self
                    .min_spacing_seconds
                    .saturating_mul(self.min_observations.saturating_sub(1) as u32)
                    <= self.min_span_seconds.saturating_mul(4),
            AeraError::InvalidOracleConfig
        );
        require!(
            self.warn_age_seconds < self.max_observation_age_seconds,
            AeraError::InvalidOracleConfig
        );
        Ok(())
    }

    /// Is `next` no looser than `self`? Loosenings wait out the timelock.
    pub fn is_tightening_from(&self, next: &Self) -> bool {
        // Identity is immutable: repointing at a different AMM is not a
        // parameter change, it is a different oracle.
        self.amm_program == next.amm_program
            && self.amm_program_data == next.amm_program_data
            && self.expected_deploy_slot == next.expected_deploy_slot
            && next.max_cross_pool_deviation_bps <= self.max_cross_pool_deviation_bps
            && next.max_observation_age_seconds <= self.max_observation_age_seconds
            && next.max_rise_bps_per_window <= self.max_rise_bps_per_window
            && next.min_observations >= self.min_observations
            && next.min_span_seconds >= self.min_span_seconds
            && next.min_spacing_seconds >= self.min_spacing_seconds
            && next.min_pool_quote_depth >= self.min_pool_quote_depth
            && next.warn_age_seconds <= self.warn_age_seconds
    }
}

/// Verified observation history for one market-priced oracle.
///
/// Seeded `["market_oracle", oracle]`.
#[account]
#[derive(InitSpace)]
pub struct MarketOracle {
    /// The `OracleState` this history belongs to.
    pub oracle: Pubkey,
    /// The collateral asset being priced, and the asset it is priced in.
    pub collateral_mint: Pubkey,
    pub quote_mint: Pubkey,
    pub collateral_decimals: u8,
    pub quote_decimals: u8,

    pub config: MarketOracleConfig,

    /// The two pools this oracle may read. Both must be healthy.
    pub pools: [PoolRef; 2],

    #[max_len(32)]
    pub observations: Vec<Observation>,
    /// Where the next observation is written.
    pub next_index: u8,

    /// A queued loosening and when it may be applied.
    pub pending_config: MarketOracleConfig,
    pub pending_eta: i64,

    pub bump: u8,
}

impl MarketOracle {
    pub const SEED: &'static [u8] = b"market_oracle";

    /// Observations ordered oldest to newest.
    ///
    /// Every element is written: the buffer is a `Vec` that grows by `push`
    /// until it reaches `OBSERVATION_CAPACITY` and only then starts overwriting
    /// in place, so there is no unwritten slot to skip. An earlier version
    /// filtered on `slot != 0` as an emptiness sentinel, which was a leftover
    /// from a fixed-size-array design and threw away any legitimate observation
    /// recorded at slot 0.
    pub fn ordered(&self) -> Vec<Observation> {
        let mut out: Vec<Observation> = self.observations.to_vec();
        out.sort_by_key(|o| o.unix_timestamp);
        out
    }

    /// The newest observation, if any.
    pub fn latest(&self) -> Option<Observation> {
        self.ordered().last().copied()
    }

    /// Seconds between the oldest and newest observation held.
    pub fn span_seconds(&self) -> i64 {
        let ordered = self.ordered();
        match (ordered.first(), ordered.last()) {
            (Some(a), Some(b)) => b.unix_timestamp - a.unix_timestamp,
            _ => 0,
        }
    }

    /// **Time-weighted** average over the configured window.
    ///
    /// Not a mean of the values. Observations arrive irregularly -- a caller
    /// chooses when to sample -- so counting them equally is exactly the attack:
    /// thirty samples during a thirty-second manipulation would outvote two
    /// samples covering the preceding hour.
    ///
    /// Each observation is weighted by how long it stood before the next one
    /// replaced it. The final observation is weighted to `now`, so a price that
    /// has been true for an hour dominates one that was true for a slot.
    pub fn twap(&self, now: i64) -> Result<u128> {
        let ordered = self.ordered();
        require!(!ordered.is_empty(), AeraError::InvalidOraclePrice);

        let window = self.config.twap_window_seconds as i64;
        let cutoff = now.saturating_sub(window);

        let mut weighted: u128 = 0;
        let mut total_weight: u128 = 0;

        let last = ordered.len() - 1;
        for (i, observation) in ordered.iter().enumerate() {
            // How long this reading stood.
            let until = ordered
                .get(i + 1)
                .map(|next| next.unix_timestamp)
                .unwrap_or(now);
            // Clip to the window, so history older than it contributes nothing.
            let from = observation.unix_timestamp.max(cutoff);
            /*
             * The newest reading is given at least one second.
             *
             * `refresh_market_oracle` writes an observation stamped `now` and
             * then averages at that same `now`, so the interval it has stood
             * for is genuinely empty. Weighting it zero is not conservative --
             * it makes the very first observation produce no TWAP at all, and
             * `require!(total_weight > 0)` below then rejects it. One second of
             * weight against a 1800-second window is negligible for an
             * established history and is the whole of the average for a new
             * one, which is exactly the intended behaviour.
             */
            let to = if i == last {
                until.max(cutoff).max(from + 1)
            } else {
                until.max(cutoff)
            };
            if to <= from {
                continue;
            }
            let weight = (to - from) as u128;
            weighted = weighted
                .checked_add(
                    observation
                        .price
                        .checked_mul(weight)
                        .ok_or(AeraError::MathOverflow)?,
                )
                .ok_or(AeraError::MathOverflow)?;
            total_weight = total_weight
                .checked_add(weight)
                .ok_or(AeraError::MathOverflow)?;
        }

        /*
         * Unreachable in practice, and kept as a guard rather than a claim.
         *
         * The newest observation is always extended forward to `now`, so a
         * stale history does not lose its weight -- the last known price simply
         * stands. That is deliberate: staleness is reported through
         * `age_seconds` and the resulting health state, which stop new
         * borrowing, and is not a reason to have no price at all. A price of
         * `None` during an outage would be worse than a price known to be old.
         */
        require!(total_weight > 0, AeraError::InvalidOraclePrice);
        Ok(weighted / total_weight)
    }

    /// Whether enough history exists, over enough time, to lend against.
    ///
    /// Count and span are both required, and the span is the one that matters:
    /// `min_observations` alone is satisfiable in consecutive slots.
    pub fn is_bootstrapped(&self) -> bool {
        let ordered = self.ordered();
        ordered.len() >= self.config.min_observations as usize
            && self.span_seconds() >= self.config.min_span_seconds as i64
    }
}
