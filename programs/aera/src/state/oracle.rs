//! Oracle state.
//!
//! v0.1 stored a guardian feed here: five publisher keys, five signed
//! submissions, a quorum and a freshness window. All of it is gone. Aera no
//! longer has a price to publish, because the only price it needs is derived
//! from state that already exists on chain.
//!
//! What remains is the protocol's *memory* of that derivation: the last rate it
//! accepted, the bounds it will accept a new one within, and where in the
//! circuit-breaker state machine it currently sits. Nothing in this account is a
//! price anybody chose.
//!
//! ## Why this is a new account rather than a rewritten one
//!
//! `PriceFeed` and `OracleState` share no fields. Reallocating one into the
//! other in place would mean interpreting guardian submissions as breaker
//! configuration for however long the migration takes, and a half-migrated
//! account is a priced account. The migration instead creates this at a new
//! PDA, repoints the reserve, and closes the old feed — see
//! `docs/V0_1_TO_V0_2_MIGRATION.md`.
//!
//! `Reserve` itself does not change shape: its `oracle` field occupies exactly
//! the bytes `price_feed` did. That is deliberate, and it is what makes the
//! migration a repoint rather than a realloc of every reserve.

use anchor_lang::prelude::*;

use crate::constants::{BPS_DENOMINATOR, FIXED_POINT_SCALE};
use crate::errors::AeraError;
use crate::oracle::breaker::{BreakerConfig, OracleHealth, Reference, RiskAction};
use crate::oracle::native_bcook::NativeOracleBounds;
use crate::oracle::{OracleSourceKind, PriceObservation};

/// Everything Aera knows about one asset's price.
#[account]
#[derive(InitSpace)]
pub struct OracleState {
    /// The market this serves. Part of the PDA seeds, so one market can never
    /// write another's oracle.
    pub market: Pubkey,

    /// The asset being priced. For the native source this must equal the mint
    /// the stake pool issues.
    pub mint: Pubkey,

    /// Which source implementation reads this. [`OracleSourceKind`] as `u8`.
    pub source_kind: u8,

    /// The program that must own `source_account`. Zero for
    /// [`OracleSourceKind::UnitOfAccount`], which reads nothing.
    pub source_program: Pubkey,

    /// The exact account the rate is derived from. Configuration, never
    /// caller-supplied. Zero for `UnitOfAccount`.
    pub source_account: Pubkey,

    /// Past this, an observation is refused and borrowing freezes. This is the
    /// bound that stops the source's operator consuming Aera's risk margin by
    /// raising their own redemption fee.
    pub max_withdrawal_fee_bps: u16,

    /// Absolute bounds on the gross rate, outside which the number cannot be a
    /// rate for this asset at all. Not the circuit breaker — these catch a
    /// redefined or corrupted field, the breaker catches movement.
    pub rate_floor: u128,
    pub rate_ceiling: u128,

    /// The source program's deploy slot at configuration time.
    ///
    /// Zero for [`OracleSourceKind::UnitOfAccount`], which reads no program.
    pub expected_deploy_slot: u64,

    /// The source program's upgrade authority at configuration time, or all
    /// zeros if it was already immutable.
    pub expected_upgrade_authority: Pubkey,

    pub breaker: BreakerConfig,

    /// The last observation the breaker accepted. The protocol's anchor, and
    /// the price liquidation falls back to during an incident.
    pub reference: Reference,

    /// Where in the state machine this oracle sits. [`OracleHealth`] as `u8`.
    pub health: u8,

    /// Movement of the last observation against the reference, in basis
    /// points, whether or not it was accepted. Recorded so an operator can see
    /// how close to a bound the source has been running.
    pub last_moved_bps: u64,

    /// Slot of the last refresh *attempt*. Risk-changing instructions require
    /// this to be the current slot, which is what makes a stale-price attack
    /// have no path: the price must have been re-derived in this same
    /// transaction.
    pub last_refresh_slot: u64,

    /// The source's own freshness marker at the last attempt. For a stake pool
    /// this is `last_update_epoch`, and it is recorded rather than enforced —
    /// a stale pool understates bCOOK's value, which is the safe direction.
    pub last_source_epoch: u64,

    pub bump: u8,
}

impl OracleState {
    pub fn kind(&self) -> Result<OracleSourceKind> {
        OracleSourceKind::from_u8(self.source_kind)
    }

    pub fn health(&self) -> Result<OracleHealth> {
        OracleHealth::from_u8(self.health)
    }

    /// Bounds for the native source, assembled from stored configuration.
    pub fn native_bounds(&self, expected_pool_mint: Pubkey) -> NativeOracleBounds {
        NativeOracleBounds {
            expected_program: self.source_program,
            expected_pool: self.source_account,
            expected_pool_mint,
            max_withdrawal_fee_bps: self.max_withdrawal_fee_bps,
            rate_floor: self.rate_floor,
            rate_ceiling: self.rate_ceiling,
            expected_deploy_slot: self.expected_deploy_slot,
            expected_upgrade_authority: self.expected_upgrade_authority,
        }
    }

    /// Whether this oracle's reference is still provisional.
    pub fn is_bootstrapping(&self) -> Result<bool> {
        Ok(self.health()? == OracleHealth::Bootstrapping)
    }

    /// The rate to value collateral from, before the reserve's risk haircut.
    ///
    /// Always the **accepted reference**, never the latest observation. During
    /// an incident the reference is deliberately not moved, so this returns the
    /// last rate the breaker was willing to stand behind — which is what lets
    /// liquidation keep working when borrowing cannot.
    pub fn effective_rate(&self) -> Result<u128> {
        require!(self.reference.is_set(), AeraError::InvalidOraclePrice);
        Ok(self.reference.effective_rate)
    }

    /// The gross rate, for reporting only. Never used to value anything.
    pub fn gross_rate(&self) -> u128 {
        self.reference.gross_rate
    }

    /// Refuse an action the current state does not permit.
    pub fn require_permits(&self, action: RiskAction) -> Result<()> {
        let health = self.health()?;
        if !health.permits(action) {
            return Err(health.refusal().into());
        }
        Ok(())
    }

    /// Refuse a price that was not re-derived in this transaction.
    ///
    /// The same discipline `Reserve::require_fresh` applies to interest: a
    /// value read from a previous slot is a value an attacker had time to
    /// arrange around.
    pub fn require_fresh(&self, current_slot: u64) -> Result<()> {
        require!(
            self.last_refresh_slot == current_slot,
            AeraError::OracleStale
        );
        Ok(())
    }

    /// Record an observation and the breaker's verdict.
    ///
    /// The reference moves only when the verdict accepts. That is the whole
    /// point of the mechanism: a refused observation still updates the
    /// diagnostics, but the number the protocol lends against does not follow
    /// it.
    pub fn record(
        &mut self,
        observation: &PriceObservation,
        verdict: &crate::oracle::breaker::Verdict,
        slot: u64,
    ) {
        self.last_refresh_slot = slot;
        self.last_source_epoch = observation.source_epoch;
        self.last_moved_bps = u64::try_from(verdict.moved_bps).unwrap_or(u64::MAX);
        self.health = verdict.health as u8;

        if verdict.accept {
            self.reference = Reference {
                gross_rate: observation.gross_rate,
                effective_rate: observation.effective_rate,
                withdrawal_fee_bps: observation.withdrawal_fee_bps,
                source_epoch: observation.source_epoch,
                slot: observation.slot,
                unix_timestamp: observation.unix_timestamp,
            };
        }
    }

    /// Validate a configuration before it is written.
    pub fn validate_config(
        source_kind: u8,
        source_program: Pubkey,
        source_account: Pubkey,
        max_withdrawal_fee_bps: u16,
        rate_floor: u128,
        rate_ceiling: u128,
        breaker: &BreakerConfig,
    ) -> Result<()> {
        let kind = OracleSourceKind::from_u8(source_kind)?;

        require!(
            (max_withdrawal_fee_bps as u128) < BPS_DENOMINATOR,
            AeraError::InvalidOracleConfig
        );
        require!(rate_floor > 0, AeraError::InvalidOracleConfig);
        require!(rate_ceiling >= rate_floor, AeraError::InvalidOracleConfig);
        breaker.validate()?;

        match kind {
            /*
             * A market-priced oracle names its AMM in `MarketOracle`, not here.
             *
             * `source_program` / `source_account` describe a single account this
             * oracle reads directly. A market price is derived from two pools
             * checked against each other, so there is no one source account to
             * name, and naming one would imply a guarantee this kind does not
             * make.
             */
            OracleSourceKind::MarketTwap => {
                require!(
                    source_program == Pubkey::default() && source_account == Pubkey::default(),
                    AeraError::InvalidOracleConfig
                );
            }
            OracleSourceKind::UnitOfAccount => {
                // Reads nothing, so naming a source would be misleading.
                require!(
                    source_program == Pubkey::default() && source_account == Pubkey::default(),
                    AeraError::InvalidOracleConfig
                );
                // Its rate is exactly 1 by definition, so the band must contain 1.
                require!(
                    rate_floor <= FIXED_POINT_SCALE && rate_ceiling >= FIXED_POINT_SCALE,
                    AeraError::InvalidOracleConfig
                );
            }
            OracleSourceKind::NativeExchangeRate => {
                require!(
                    source_program != Pubkey::default() && source_account != Pubkey::default(),
                    AeraError::InvalidOracleConfig
                );
                /*
                 * No requirement that the floor sits at or above parity.
                 *
                 * An earlier version required exactly that, and it was wrong:
                 * a slashed stake pool genuinely reports below 1.0, and
                 * refusing to read it would pin the reference at the last
                 * pre-slash rate -- so liquidation would value collateral
                 * above its worth precisely when it needed to be conservative.
                 * The floor exists to catch a corrupted field, not a loss.
                 */
            }
        }

        Ok(())
    }
}

/// The unit-of-account observation: exactly 1, always.
///
/// COOK prices COOK. There is no source to read and nothing that could make
/// this anything other than `FIXED_POINT_SCALE`, so it is constructed rather
/// than derived. It exists as a source kind rather than as a special case in
/// the handlers so that "what is this asset worth" has exactly one shape
/// throughout the program.
pub fn unit_observation(slot: u64, unix_timestamp: i64) -> PriceObservation {
    PriceObservation {
        gross_rate: FIXED_POINT_SCALE,
        withdrawal_fee_bps: 0,
        effective_rate: FIXED_POINT_SCALE,
        source_epoch: 0,
        slot,
        unix_timestamp,
        source: OracleSourceKind::UnitOfAccount,
        // One COOK was one COOK last epoch too.
        previous_epoch_rate: Some(FIXED_POINT_SCALE),
    }
}
