//! The oracle abstraction.
//!
//! The lending engine never parses an external account. It asks for a
//! [`ValidatedPrice`] and gets a normalized, fixed-point, already-checked
//! number, or an error. Everything specific to a particular price source lives
//! behind this boundary, so adding a second source later is a new module rather
//! than a change to `borrow`.
//!
//! ## Two rates, never merged
//!
//! There are two distinct reductions between "what the stake pool reports" and
//! "what Aera will lend against", and they are different *kinds* of thing:
//!
//! ```text
//!   gross_rate       total_lamports / pool_token_supply
//!                    what the pool's own accounting says a bCOOK is worth
//!
//!   effective_rate   gross_rate x (1 - withdrawal_fee)
//!                    what a holder can ACTUALLY redeem it for. This is
//!                    economics of the asset, read live from the pool, and is
//!                    not Aera's to choose.
//!
//!   collateral_value effective_rate x (1 - risk_haircut)
//!                    Aera's own risk policy, configurable, applied on top.
//! ```
//!
//! Folding the withdrawal fee into the haircut would be a mistake in two
//! directions at once. It would let the staking operator silently consume
//! Aera's risk margin by raising their fee, and it would let an Aera admin
//! appear to be taking risk margin they are not. They stay separate, and the
//! oracle is responsible only for the first two.
//!
//! ## What this module refuses to do
//!
//! - accept a price from a caller, in any form
//! - accept a price from an admin, in any form
//! - read a DEX quote
//! - use floating point
//! - return a price that has not passed the circuit breaker

pub mod breaker;
pub mod deployment;
pub mod market_breaker;
pub mod native_bcook;
pub mod validation;

use anchor_lang::prelude::*;

use crate::constants::BPS_DENOMINATOR;
use crate::errors::AeraError;
use crate::math::mul_div_floor;

/// Which implementation produced an observation.
///
/// Stored in `OracleState` as a `u8` rather than as a serialized enum, so
/// adding a source later is not an account-layout change.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum OracleSourceKind {
    /// Exactly 1, always. COOK is the unit of account, so pricing it is a
    /// definition rather than a measurement.
    ///
    /// This is a source kind rather than a special case in the handlers so that
    /// "what is this asset worth" has one shape everywhere in the program. It
    /// reads no account and cannot fail.
    UnitOfAccount = 0,
    /// The bCOOK/COOK redemption rate, from BakeYourStake's own stake-pool
    /// accounting. The only external source implemented in v0.2.
    NativeExchangeRate = 1,
    /// A time-weighted average of AMM spot prices, for assets whose value is
    /// whatever a market will pay.
    ///
    /// Structurally different from the two above, and the difference is a trust
    /// assumption rather than an implementation detail:
    ///
    /// - `UnitOfAccount` is a definition. Nothing can move it.
    /// - `NativeExchangeRate` reads a stake pool's own books. Moving it means
    ///   actually staking or unstaking.
    /// - `MarketTwap` reads AMM reserves, which move whenever anyone trades.
    ///
    /// The program still derives the price itself -- no caller supplies a
    /// number -- but the *sampling times* are chosen by whoever calls refresh.
    /// That is the residual trust, and it is bounded by requiring a minimum
    /// span, a minimum spacing, and time-weighting rather than counting.
    ///
    /// Only for isolated markets. Core must never use it.
    MarketTwap = 2,
}

impl OracleSourceKind {
    pub fn from_u8(value: u8) -> Result<Self> {
        match value {
            0 => Ok(Self::UnitOfAccount),
            1 => Ok(Self::NativeExchangeRate),
            2 => Ok(Self::MarketTwap),
            _ => err!(AeraError::UnknownOracleSource),
        }
    }

    /// Whether this source derives its rate from an external account.
    pub fn reads_an_account(self) -> bool {
        matches!(self, Self::NativeExchangeRate | Self::MarketTwap)
    }

    /// Whether a fall in this rate is evidence of a fault or of the market.
    ///
    /// For a stake-pool rate a large fall cannot happen honestly, so the
    /// breaker refuses it and keeps the last accepted value. For a market price
    /// that logic is inverted and dangerous: holding a pre-crash valuation
    /// means collateral stays overvalued, liquidations do not fire, and the
    /// loss lands on suppliers. A downside breaker on a volatile asset
    /// manufactures the bad debt it exists to prevent.
    pub fn falls_are_always_accepted(self) -> bool {
        matches!(self, Self::MarketTwap)
    }
}

/// A reading taken from a source, before the breaker has judged it.
///
/// Deliberately carries the gross rate and the fee alongside the effective
/// rate: the protocol stores all three, so a later reader can tell whether a
/// move came from the pool earning rewards or from the operator raising their
/// withdrawal fee. Those have very different meanings and a single blended
/// number cannot distinguish them.
#[derive(Clone, Copy, Debug)]
pub struct PriceObservation {
    /// `total_lamports / pool_token_supply`, FIXED_POINT_SCALE-scaled.
    pub gross_rate: u128,
    /// The source's own redemption fee, in basis points, as read on chain.
    pub withdrawal_fee_bps: u16,
    /// `gross_rate * (1 - withdrawal_fee)`, FIXED_POINT_SCALE-scaled.
    pub effective_rate: u128,
    /// The source's internal freshness marker. For a stake pool this is
    /// `last_update_epoch`; it is recorded so staleness is observable, and it
    /// deliberately does not by itself reject a price -- see `native_bcook`.
    pub source_epoch: u64,
    pub slot: u64,
    pub unix_timestamp: i64,
    pub source: OracleSourceKind,

    /// The rate the source itself reports for its previous epoch, if it has
    /// one.
    ///
    /// Carried on the observation so a bootstrap can check the very first
    /// reading against the source's own history without parsing the account a
    /// second time. `None` only when the source has not completed an epoch.
    pub previous_epoch_rate: Option<u128>,
}

impl PriceObservation {
    /// Whether a first observation of this kind has to be confirmed before the
    /// oracle may permit new risk.
    ///
    /// Bootstrapping exists because a rate read out of another program's
    /// account can be arranged by whoever controls that account, and a
    /// brand-new oracle has no reference with which to object. `UnitOfAccount`
    /// reads nothing: its rate is the constant 1, fixed in this program's code,
    /// so there is no anchor for anyone to choose and nothing a later epoch
    /// could confirm. Making it wait would freeze borrowing for an epoch to
    /// guard against an attack that cannot exist.
    pub fn needs_bootstrap(&self) -> bool {
        !matches!(self.source, OracleSourceKind::UnitOfAccount)
    }
}

/// An observation that has passed account validation, sanity bounds and the
/// circuit breaker. This is the only price type the lending engine sees.
#[derive(Clone, Copy, Debug)]
pub struct ValidatedPrice {
    /// The number collateral is valued from, before Aera's risk haircut.
    pub effective_rate: u128,
    /// Kept for transparency. Never used for valuation.
    pub gross_rate: u128,
    pub withdrawal_fee_bps: u16,
    pub slot: u64,
    pub source: OracleSourceKind,
}

impl ValidatedPrice {
    /// Apply Aera's own risk haircut. Separate from the withdrawal fee above,
    /// and applied after it.
    ///
    /// Rounds down: collateral value benefits the borrower, and the convention
    /// in `math.rs` is that quantities favourable to the user round down.
    pub fn collateral_rate(&self, risk_haircut_bps: u16) -> Result<u128> {
        let keep = BPS_DENOMINATOR
            .checked_sub(risk_haircut_bps as u128)
            .ok_or(AeraError::MathOverflow)?;
        mul_div_floor(self.effective_rate, keep, BPS_DENOMINATOR)
    }
}

/// Apply a source's redemption fee to a gross rate.
///
/// Shared by every source, so "effective means net of the fee the holder
/// actually pays" has one definition. Rounds down, which understates what a
/// redeemer receives and therefore understates collateral -- the safe
/// direction.
pub fn apply_withdrawal_fee(gross_rate: u128, withdrawal_fee_bps: u16) -> Result<u128> {
    let keep = BPS_DENOMINATOR
        .checked_sub(withdrawal_fee_bps as u128)
        .ok_or(AeraError::MathOverflow)?;
    mul_div_floor(gross_rate, keep, BPS_DENOMINATOR)
}

/// Worked example from the v0.2 design discussion, as an executable check.
///
/// gross 1.10, withdrawal fee 2%, risk haircut 5% => 1.0241 COOK per bCOOK.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::FIXED_POINT_SCALE;

    const ONE: u128 = FIXED_POINT_SCALE;

    #[test]
    fn the_two_reductions_compose_in_order() {
        let gross = ONE * 110 / 100; // 1.10
        let effective = apply_withdrawal_fee(gross, 200).unwrap();
        assert_eq!(effective, ONE * 1078 / 1000, "1.10 x 0.98 = 1.078");

        let price = ValidatedPrice {
            effective_rate: effective,
            gross_rate: gross,
            withdrawal_fee_bps: 200,
            slot: 0,
            source: OracleSourceKind::NativeExchangeRate,
        };

        let collateral = price.collateral_rate(500).unwrap();
        assert_eq!(collateral, ONE * 10_241 / 10_000, "1.078 x 0.95 = 1.0241");
    }

    #[test]
    fn the_haircut_never_touches_the_stored_rate() {
        let gross = ONE * 13 / 10;
        let price = ValidatedPrice {
            effective_rate: apply_withdrawal_fee(gross, 200).unwrap(),
            gross_rate: gross,
            withdrawal_fee_bps: 200,
            slot: 0,
            source: OracleSourceKind::NativeExchangeRate,
        };

        let _ = price.collateral_rate(500).unwrap();
        assert_eq!(price.gross_rate, gross, "gross must survive a haircut read");
        assert_eq!(
            price.effective_rate,
            apply_withdrawal_fee(gross, 200).unwrap(),
            "effective must survive a haircut read"
        );
    }

    #[test]
    fn a_zero_fee_leaves_the_gross_rate_alone() {
        let gross = ONE * 13 / 10;
        assert_eq!(apply_withdrawal_fee(gross, 0).unwrap(), gross);
    }

    #[test]
    fn a_total_fee_leaves_nothing() {
        assert_eq!(apply_withdrawal_fee(ONE, 10_000).unwrap(), 0);
    }

    #[test]
    fn a_fee_above_one_hundred_percent_errors_rather_than_wrapping() {
        assert!(apply_withdrawal_fee(ONE, 10_001).is_err());
    }
}
