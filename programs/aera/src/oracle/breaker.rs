//! The exchange-rate circuit breaker and the oracle state machine.
//!
//! A deterministic rate is not a safe rate. The stake pool's accounting could be
//! wrong, its program could be upgraded to lie, or a genuine incident could
//! move it violently. The breaker's job is to stop any of those becoming
//! borrowing capacity before a human has looked.
//!
//! ## Movement is measured per epoch, not per slot
//!
//! `total_lamports` only moves when the pool is updated, which is once per
//! epoch. Between updates the rate is exactly flat; at the boundary it steps.
//! Measured on Cookie Chain: epochs are 53.4 hours, and the observed step was
//! +0.21%.
//!
//! So a breaker measuring movement over any shorter window would see two days
//! of nothing and then a jump, and would have to be tuned either so loose it
//! never fires or so tight it fires every epoch. Movement is therefore compared
//! against the number of *pool epochs* elapsed since the reference was
//! accepted, and the allowance scales with it.
//!
//! ## Up and down are not symmetric
//!
//! Upward movement is what staking yield looks like, and it is also what an
//! accounting exploit looks like. It is allowed, but only at roughly the pace
//! yield can actually accrue.
//!
//! Downward movement is not yield. A stake pool's rate falls when something has
//! gone wrong -- slashing, a validator failure, a misreported balance. It is
//! held to a tighter bound, and it drops straight to a frozen state rather than
//! a warning.
//!
//! ## What the breaker cannot do
//!
//! It cannot be set by an admin. `reset` re-anchors the reference to *whatever
//! the chain currently says*, which is the only value an operator can choose:
//! the one that is already true. There is no instruction anywhere in this
//! program that writes a rate of the caller's choosing.

use anchor_lang::prelude::*;

use crate::constants::BPS_DENOMINATOR;
use crate::errors::AeraError;
use crate::math::mul_div_floor;

use super::PriceObservation;

/// Pool epochs without an update before an accepted observation is flagged.
///
/// Cookie epochs are ~53 hours, so this is roughly a week. Everything stays
/// permitted -- staleness understates collateral, which is the safe direction.
pub const STALE_EPOCHS_WARNING: u64 = 3;

/// What the protocol will currently permit.
///
/// Stored in `OracleState` as a `u8`, not as a serialized enum.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum OracleHealth {
    /// The rate moved within its per-epoch allowance. Everything is permitted.
    Healthy = 0,
    /// The rate moved further than expected but inside the emergency bound, or
    /// the source is several epochs stale. Everything is still permitted; this
    /// is an observable signal, not a restriction.
    RateWarning = 1,
    /// The rate moved beyond its allowance. Nothing that increases risk is
    /// permitted. Repaying, supplying, adding collateral and liquidation all
    /// remain open, the last using the last accepted reference rate.
    BorrowFrozen = 2,
    /// The rate moved beyond the absolute emergency bound, or the source could
    /// not be read at all. Only actions that reduce risk are permitted.
    Emergency = 3,
    /// A reference exists but has not yet been confirmed by a second
    /// observation in a later source epoch.
    ///
    /// The first observation an oracle ever takes has nothing to be compared
    /// against -- the movement breaker needs a prior reference, and there is
    /// none. Accepting it merely because it sits inside the very wide absolute
    /// band would make the whole breaker bypassable by anyone who could arrange
    /// the state at the moment of configuration.
    ///
    /// So a new oracle values existing positions but does not let anyone open
    /// new risk against it until a second, independent epoch agrees.
    Bootstrapping = 4,
}

impl OracleHealth {
    pub fn from_u8(value: u8) -> Result<Self> {
        match value {
            0 => Ok(Self::Healthy),
            1 => Ok(Self::RateWarning),
            2 => Ok(Self::BorrowFrozen),
            3 => Ok(Self::Emergency),
            4 => Ok(Self::Bootstrapping),
            _ => err!(AeraError::InvalidOracleConfig),
        }
    }
}

/// The actions the state machine gates.
///
/// Named by their effect on risk rather than by their instruction, so the
/// matrix below reads as policy rather than as a list of handlers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RiskAction {
    /// Increases debt.
    Borrow,
    /// Removes collateral, so it can only worsen a health factor.
    WithdrawCollateral,
    /// Supplier exit. Needs no collateral price at all -- COOK is the unit of
    /// account -- but is still stopped in an emergency, see `permits`.
    WithdrawLiquidity,
    /// Adds liquidity. Cannot worsen anyone's position.
    SupplyLiquidity,
    /// Reduces debt.
    Repay,
    /// Adds collateral.
    DepositCollateral,
    /// Reduces debt and removes a bad position. Improves protocol solvency.
    Liquidate,
}

impl OracleHealth {
    /// The whole gate matrix, in one place.
    ///
    /// | Action              | Healthy | Warning | Bootstrapping | BorrowFrozen | Emergency |
    /// |---------------------|---------|---------|---------------|--------------|-----------|
    /// | Borrow              | yes     | yes     | **no**        | **no**       | **no**    |
    /// | WithdrawCollateral  | yes     | yes     | **no**        | **no**       | **no**    |
    /// | WithdrawLiquidity   | yes     | yes     | yes           | yes          | **no**    |
    /// | SupplyLiquidity     | yes     | yes     | yes           | yes          | yes       |
    /// | Repay               | yes     | yes     | yes           | yes          | yes       |
    /// | DepositCollateral   | yes     | yes     | yes           | yes          | yes       |
    /// | Liquidate           | yes     | yes     | yes           | yes          | yes       |
    ///
    /// `Bootstrapping` sits with `BorrowFrozen`: an unconfirmed reference is
    /// good enough to value a position that already exists, and not good enough
    /// to create a new one. Liquidation stays open for the same reason it stays
    /// open in every other state -- a market that cannot close bad positions
    /// accumulates them.
    ///
    /// Two rows are worth explaining.
    ///
    /// **Repay is permitted in every state, without exception.** A borrower who
    /// cannot repay can only be liquidated, so an emergency that traps them
    /// converts a protocol problem into their loss. This is the single rule the
    /// rest of the design bends around.
    ///
    /// **Liquidate is permitted in every state**, using the last accepted
    /// reference rate rather than the suspect observation. Blocking it would
    /// let bad debt accumulate for exactly as long as the incident lasts, which
    /// is when the protocol can least afford it. The brief's instruction is
    /// explicit: do not automatically block liquidations if the last trusted
    /// price can safely be used.
    ///
    /// `WithdrawLiquidity` is the one judgement call. It needs no bCOOK price,
    /// so an oracle fault is not a reason to stop it, and trapping suppliers is
    /// its own harm. It is nevertheless stopped in `Emergency`, because an
    /// emergency may mean undiscovered bad debt, and a race for the exit would
    /// pay the fastest suppliers out of the slowest ones' claims.
    pub fn permits(self, action: RiskAction) -> bool {
        use OracleHealth::*;
        use RiskAction::*;

        match action {
            Repay | DepositCollateral | SupplyLiquidity | Liquidate => true,
            WithdrawLiquidity => !matches!(self, Emergency),
            Borrow | WithdrawCollateral => matches!(self, Healthy | RateWarning),
        }
    }

    /// The error to return when [`permits`] is false, so a refusal names the
    /// state that caused it.
    pub fn refusal(self) -> AeraError {
        match self {
            OracleHealth::Emergency => AeraError::OracleEmergency,
            OracleHealth::Bootstrapping => AeraError::OracleBootstrapping,
            _ => AeraError::OracleBorrowFrozen,
        }
    }
}

/// Breaker bounds. Held in `OracleState` so a second market could be tuned
/// differently, and so tests can drive the boundaries directly.
#[derive(Clone, Copy, Debug, AnchorSerialize, AnchorDeserialize, InitSpace)]
pub struct BreakerConfig {
    /// Maximum upward move per elapsed pool epoch, in basis points.
    pub max_up_bps_per_epoch: u16,
    /// Maximum downward move per elapsed pool epoch, in basis points.
    pub max_down_bps_per_epoch: u16,
    /// Absolute move from the reference, in either direction, that goes
    /// straight to `Emergency` regardless of how long has elapsed.
    pub emergency_deviation_bps: u16,
    /// Ceiling on how many epochs of allowance may accumulate. Without it, a
    /// reference left un-refreshed for a year would permit an unbounded jump.
    pub max_epoch_allowance: u8,
}

impl BreakerConfig {
    pub fn validate(&self) -> Result<()> {
        require!(
            (self.max_up_bps_per_epoch as u128) <= BPS_DENOMINATOR,
            AeraError::InvalidOracleConfig
        );
        require!(
            (self.max_down_bps_per_epoch as u128) <= BPS_DENOMINATOR,
            AeraError::InvalidOracleConfig
        );
        require!(
            (self.emergency_deviation_bps as u128) <= BPS_DENOMINATOR,
            AeraError::InvalidOracleConfig
        );
        // An emergency bound below the per-epoch allowance would fire before
        // the ordinary breaker ever could, making the ordinary one dead code.
        require!(
            self.emergency_deviation_bps >= self.max_up_bps_per_epoch
                && self.emergency_deviation_bps >= self.max_down_bps_per_epoch,
            AeraError::InvalidOracleConfig
        );
        require!(self.max_epoch_allowance > 0, AeraError::InvalidOracleConfig);
        Ok(())
    }
}

/// The last observation the breaker accepted. This is the protocol's anchor.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, AnchorSerialize, AnchorDeserialize, InitSpace, Default,
)]
pub struct Reference {
    pub gross_rate: u128,
    pub effective_rate: u128,
    pub withdrawal_fee_bps: u16,
    /// The source's own epoch marker at the moment of acceptance. Movement is
    /// measured per elapsed epoch against this, not against wall clock.
    pub source_epoch: u64,
    pub slot: u64,
    pub unix_timestamp: i64,
}

impl Reference {
    pub fn is_set(&self) -> bool {
        self.effective_rate > 0
    }
}

/// What the breaker decided about an observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Verdict {
    /// Whether the reference should move to this observation.
    pub accept: bool,
    /// The health the oracle should now be in.
    pub health: OracleHealth,
    /// Absolute movement from the reference, in basis points. Recorded so an
    /// operator can see how close to a bound the last observation came.
    pub moved_bps: u128,
}

/// Movement between two rates as a fraction of the first, in basis points.
///
/// Floors, so a move is never overstated into tripping the breaker by rounding.
fn move_bps(previous: u128, next: u128) -> Result<u128> {
    if previous == 0 {
        return Ok(0);
    }
    mul_div_floor(previous.abs_diff(next), BPS_DENOMINATOR, previous)
}

/// Judge the very first observation an oracle ever takes.
///
/// The movement breaker cannot help here: it compares against a reference, and
/// there is none. What is available instead is the source's own account of the
/// previous epoch -- a stake pool stores `last_epoch_total_lamports` and
/// `last_epoch_pool_token_supply` alongside the current figures.
///
/// That is one epoch of history the source published about itself, and it
/// cannot be forged independently of the current figures without the account
/// becoming internally inconsistent. Requiring the bootstrap rate to sit within
/// one epoch's normal movement of it turns "any rate inside a very wide
/// absolute band" into "a rate consistent with where this pool actually was".
///
/// `previous` is `None` only when the pool has not completed an epoch, which is
/// the one case with genuinely nothing to check against. That is not fatal --
/// the oracle simply stays in `Bootstrapping` until a second epoch confirms it
/// -- but it is recorded so an operator can see it happened.
pub fn bootstrap_verdict(
    observation: &PriceObservation,
    previous_epoch_rate: Option<u128>,
    config: &BreakerConfig,
) -> Result<Verdict> {
    let Some(previous) = previous_epoch_rate else {
        // No history to check against. Accepted, but unconfirmed.
        return Ok(Verdict {
            accept: true,
            health: OracleHealth::Bootstrapping,
            moved_bps: 0,
        });
    };

    let moved_bps = move_bps(previous, observation.gross_rate)?;
    let rising = observation.gross_rate > previous;
    let allowance = if rising {
        config.max_up_bps_per_epoch
    } else {
        config.max_down_bps_per_epoch
    } as u128;

    if moved_bps > allowance {
        // The pool's current figures disagree with its own previous epoch by
        // more than an epoch of movement can explain. Refuse outright: this is
        // the moment an attacker would choose to arrange state, and there is no
        // prior reference to fall back to.
        return Ok(Verdict {
            accept: false,
            health: OracleHealth::Emergency,
            moved_bps,
        });
    }

    Ok(Verdict {
        accept: true,
        health: OracleHealth::Bootstrapping,
        moved_bps,
    })
}

/// Judge an observation against the stored reference.
///
/// Compares the **gross** rate, not the effective one. The withdrawal fee is
/// bounded separately in `native_bcook::observe`, and blending it in here would
/// make a fee change indistinguishable from a backing change -- which are very
/// different events needing different responses.
pub fn evaluate(
    reference: &Reference,
    observation: &PriceObservation,
    config: &BreakerConfig,
    was_bootstrapping: bool,
) -> Result<Verdict> {
    // First ever observation: nothing to compare against, so it becomes the
    // anchor. The absolute floor and ceiling in `native_bcook` are what guard
    // this case -- the breaker cannot.
    if !reference.is_set() {
        return Ok(Verdict {
            accept: true,
            health: OracleHealth::Healthy,
            moved_bps: 0,
        });
    }

    let moved_bps = move_bps(reference.gross_rate, observation.gross_rate)?;

    // Absolute bound first: a large enough move is an emergency however long it
    // took, so a source left stale for many epochs cannot accumulate its way
    // past this.
    if moved_bps >= config.emergency_deviation_bps as u128 {
        return Ok(Verdict {
            accept: false,
            health: OracleHealth::Emergency,
            moved_bps,
        });
    }

    // Allowance scales with elapsed pool epochs, since that is the only clock
    // on which this rate moves at all. A source that has not advanced an epoch
    // gets one epoch of allowance rather than zero: the pool can be updated
    // more than once within an epoch, and demanding exactly zero movement would
    // trip on the resulting dust.
    let epochs_elapsed = observation
        .source_epoch
        .saturating_sub(reference.source_epoch)
        .max(1)
        .min(config.max_epoch_allowance as u64);

    let rising = observation.gross_rate > reference.gross_rate;
    let per_epoch = if rising {
        config.max_up_bps_per_epoch
    } else {
        config.max_down_bps_per_epoch
    } as u128;

    let allowance = per_epoch
        .checked_mul(epochs_elapsed as u128)
        .ok_or(AeraError::MathOverflow)?;

    if moved_bps <= allowance {
        /*
         * Confirming a bootstrap.
         *
         * A reference taken at configuration time is provisional until a
         * *different* epoch of the source agrees with it. Requiring a later
         * epoch, rather than merely a later slot, is what makes the
         * confirmation independent: within one epoch a stake pool's figures do
         * not move at all, so a second reading in the same epoch would confirm
         * nothing but that the bytes had not changed in the last few seconds.
         */
        if was_bootstrapping && observation.source_epoch <= reference.source_epoch {
            return Ok(Verdict {
                accept: true,
                health: OracleHealth::Bootstrapping,
                moved_bps,
            });
        }

        /*
         * Accepted -- but say so with a warning if the source has not advanced
         * for several epochs.
         *
         * A stale stake pool understates bCOOK's value, so it is safe and must
         * not block anything. It is still worth surfacing: a pool nobody has
         * cranked for a week means the rate the protocol is lending against is
         * a week behind the rewards actually earned, and the jump when someone
         * finally cranks it is correspondingly larger.
         *
         * Without this branch RATE_WARNING would be declared and never
         * produced, which a state-machine test caught.
         */
        let health = if epochs_elapsed >= STALE_EPOCHS_WARNING {
            OracleHealth::RateWarning
        } else {
            OracleHealth::Healthy
        };
        return Ok(Verdict {
            accept: true,
            health,
            moved_bps,
        });
    }

    /*
     * Past the allowance but inside the emergency bound.
     *
     * Upward is the direction yield moves, so an over-large rise is suspicious
     * rather than certainly wrong: the reference is held, borrowing freezes,
     * and an operator decides. Downward is never yield, so it is treated as the
     * more serious signal even at the same magnitude.
     */
    Ok(Verdict {
        accept: false,
        health: if rising {
            OracleHealth::BorrowFrozen
        } else {
            OracleHealth::Emergency
        },
        moved_bps,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::FIXED_POINT_SCALE;
    use crate::oracle::OracleSourceKind;

    const ONE: u128 = FIXED_POINT_SCALE;

    fn config() -> BreakerConfig {
        BreakerConfig {
            max_up_bps_per_epoch: 200,      // 2%
            max_down_bps_per_epoch: 100,    // 1%
            emergency_deviation_bps: 1_000, // 10%
            max_epoch_allowance: 10,
        }
    }

    fn reference_at(rate: u128, epoch: u64) -> Reference {
        Reference {
            gross_rate: rate,
            effective_rate: rate,
            withdrawal_fee_bps: 200,
            source_epoch: epoch,
            slot: 1,
            unix_timestamp: 1,
        }
    }

    fn observation_at(rate: u128, epoch: u64) -> PriceObservation {
        PriceObservation {
            gross_rate: rate,
            withdrawal_fee_bps: 200,
            effective_rate: rate,
            source_epoch: epoch,
            slot: 2,
            unix_timestamp: 2,
            source: OracleSourceKind::NativeExchangeRate,
            previous_epoch_rate: None,
        }
    }

    #[test]
    fn the_first_observation_becomes_the_anchor() {
        let verdict = evaluate(
            &Reference::default(),
            &observation_at(ONE, 1),
            &config(),
            false,
        )
        .unwrap();
        assert!(verdict.accept);
        assert_eq!(verdict.health, OracleHealth::Healthy);
    }

    #[test]
    fn the_measured_epoch_move_is_comfortably_inside_the_allowance() {
        // The +0.21% observed on Cookie Chain between epochs 50 and 51.
        let reference = reference_at(ONE * 129_769 / 100_000, 50);
        let observation = observation_at(ONE * 130_051 / 100_000, 51);

        let verdict = evaluate(&reference, &observation, &config(), false).unwrap();
        assert!(
            verdict.accept,
            "a normal reward epoch must not trip the breaker"
        );
        assert_eq!(verdict.health, OracleHealth::Healthy);
        assert!(verdict.moved_bps < 30, "moved {} bps", verdict.moved_bps);
    }

    #[test]
    fn a_rise_past_the_allowance_freezes_borrowing_without_moving_the_reference() {
        let reference = reference_at(ONE, 50);
        let observation = observation_at(ONE * 105 / 100, 51); // +5% in one epoch

        let verdict = evaluate(&reference, &observation, &config(), false).unwrap();
        assert!(
            !verdict.accept,
            "the reference must not follow a suspicious jump"
        );
        assert_eq!(verdict.health, OracleHealth::BorrowFrozen);
    }

    #[test]
    fn a_fall_is_held_to_a_tighter_bound_than_a_rise() {
        let reference = reference_at(ONE, 50);

        // +1.5% is inside the 2% up-allowance.
        let up = evaluate(
            &reference,
            &observation_at(ONE * 1015 / 1000, 51),
            &config(),
            false,
        )
        .unwrap();
        assert!(up.accept, "1.5% up is within allowance");

        // -1.5% is outside the 1% down-allowance, and a fall is an incident.
        let down = evaluate(
            &reference,
            &observation_at(ONE * 985 / 1000, 51),
            &config(),
            false,
        )
        .unwrap();
        assert!(!down.accept);
        assert_eq!(
            down.health,
            OracleHealth::Emergency,
            "a fall past its bound is treated as an incident, not a warning"
        );
    }

    #[test]
    fn the_emergency_bound_ignores_elapsed_epochs() {
        let reference = reference_at(ONE, 0);
        // 100 epochs elapsed would otherwise allow 100 x 2% = 200%.
        let observation = observation_at(ONE * 150 / 100, 100); // +50%

        let verdict = evaluate(&reference, &observation, &config(), false).unwrap();
        assert!(!verdict.accept);
        assert_eq!(
            verdict.health,
            OracleHealth::Emergency,
            "no amount of staleness may accumulate past the absolute bound"
        );
    }

    #[test]
    fn allowance_accumulates_across_epochs_but_is_capped() {
        let reference = reference_at(ONE, 0);

        // 3 epochs at 2% each = 6% allowed; +5% fits.
        let verdict = evaluate(
            &reference,
            &observation_at(ONE * 105 / 100, 3),
            &config(),
            false,
        )
        .unwrap();
        assert!(verdict.accept, "three epochs of yield is legitimate");

        // 50 epochs would be 100%, but max_epoch_allowance caps it at 10 -> 20%,
        // and the emergency bound catches it first anyway.
        let verdict = evaluate(
            &reference,
            &observation_at(ONE * 130 / 100, 50),
            &config(),
            false,
        )
        .unwrap();
        assert!(!verdict.accept, "the epoch allowance must be bounded");
    }

    #[test]
    fn an_unchanged_rate_is_always_accepted() {
        let reference = reference_at(ONE, 50);
        let verdict = evaluate(&reference, &observation_at(ONE, 50), &config(), false).unwrap();
        assert!(verdict.accept);
        assert_eq!(verdict.moved_bps, 0);
    }

    #[test]
    fn repay_is_permitted_in_every_state() {
        for health in [
            OracleHealth::Healthy,
            OracleHealth::RateWarning,
            OracleHealth::BorrowFrozen,
            OracleHealth::Emergency,
        ] {
            assert!(
                health.permits(RiskAction::Repay),
                "repay must never be blocked, even in {health:?}"
            );
            assert!(
                health.permits(RiskAction::DepositCollateral),
                "adding collateral must never be blocked, even in {health:?}"
            );
            assert!(
                health.permits(RiskAction::Liquidate),
                "liquidation must stay open in {health:?}"
            );
        }
    }

    #[test]
    fn risk_increasing_actions_stop_at_borrow_frozen() {
        for health in [OracleHealth::BorrowFrozen, OracleHealth::Emergency] {
            assert!(!health.permits(RiskAction::Borrow));
            assert!(!health.permits(RiskAction::WithdrawCollateral));
        }
        for health in [OracleHealth::Healthy, OracleHealth::RateWarning] {
            assert!(health.permits(RiskAction::Borrow));
            assert!(health.permits(RiskAction::WithdrawCollateral));
        }
    }

    #[test]
    fn supplier_exit_survives_a_freeze_but_not_an_emergency() {
        assert!(OracleHealth::BorrowFrozen.permits(RiskAction::WithdrawLiquidity));
        assert!(!OracleHealth::Emergency.permits(RiskAction::WithdrawLiquidity));
    }

    #[test]
    fn a_config_whose_emergency_bound_undercuts_its_allowance_is_refused() {
        let bad = BreakerConfig {
            max_up_bps_per_epoch: 500,
            max_down_bps_per_epoch: 100,
            emergency_deviation_bps: 200, // below the up-allowance
            max_epoch_allowance: 10,
        };
        assert!(bad.validate().is_err());
    }

    #[test]
    fn a_zero_epoch_allowance_is_refused() {
        let bad = BreakerConfig {
            max_epoch_allowance: 0,
            ..config()
        };
        assert!(bad.validate().is_err());
    }
}
