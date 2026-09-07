//! Every rule that can refuse a user action, in one file.
//!
//! Handlers call these instead of open-coding the checks, so "what stops this"
//! has a single answer per rule and the tests can target it directly.

use anchor_lang::prelude::*;

use crate::constants::{BPS_DENOMINATOR, FULL_CLOSE_HEALTH_FACTOR_BPS};
use crate::errors::AeraError;
use crate::math::{mul_div_floor, Rounding};
use crate::state::{Obligation, Reserve};

/// Apply an asset's haircut to a face value. bCOOK is marked down 5% before it
/// backs anything, so a borrower's limit is computed from what the collateral
/// is conservatively worth, not its mark.
pub fn apply_haircut(value: u128, haircut_bps: u16) -> Result<u128> {
    let keep = BPS_DENOMINATOR
        .checked_sub(haircut_bps as u128)
        .ok_or(AeraError::MathOverflow)?;
    mul_div_floor(value, keep, BPS_DENOMINATOR)
}

/// Reject a supply that would push the reserve past its supply cap.
///
/// The cap is measured against `gross_liquidity` — everything the pool has
/// claim to, borrowed or not — so borrowing does not silently free headroom for
/// more deposits.
pub fn check_supply_cap(reserve: &Reserve, adding: u64) -> Result<()> {
    if reserve.config.supply_cap == 0 {
        return Ok(());
    }
    let after = reserve
        .gross_liquidity()?
        .checked_add(adding as u128)
        .ok_or(AeraError::MathOverflow)?;
    require!(
        after <= reserve.config.supply_cap as u128,
        AeraError::SupplyCapExceeded
    );
    Ok(())
}

/// Reject a borrow that would push total debt past the borrow cap.
/// The most one wallet may owe in this reserve.
///
/// `cap` is `0` when no `RiskConfig` exists for the reserve, which means
/// unlimited -- matching every other cap in the protocol.
///
/// `already_owed` is the obligation's **current debt** for this reserve, i.e.
/// principal scaled by the live borrow index, not the raw principal. A wallet
/// that borrowed to the cap and then accrued interest is over it; that blocks
/// only new borrowing and never repayment.
///
/// `adding` is the full new debt including any origination fee. `borrow` charges
/// the borrower the whole `liquidity_amount` and pays out less, so the fee is
/// already inside that figure -- checking the paid-out amount instead would let a
/// wallet cross the cap by exactly the fee.
///
/// This bounds the size of one liquidation, not the size of one person. A wallet
/// cap is not Sybil resistance and must never be described as such.
pub fn check_per_wallet_borrow_cap(cap: u64, already_owed: u64, adding: u64) -> Result<()> {
    if cap == 0 {
        return Ok(());
    }
    let after = (already_owed as u128)
        .checked_add(adding as u128)
        .ok_or(AeraError::MathOverflow)?;
    require!(after <= cap as u128, AeraError::PerWalletBorrowCapExceeded);
    Ok(())
}

pub fn check_borrow_cap(reserve: &Reserve, adding: u64) -> Result<()> {
    if reserve.config.borrow_cap == 0 {
        return Ok(());
    }
    let after = (reserve.current_borrowed_amount()? as u128)
        .checked_add(adding as u128)
        .ok_or(AeraError::MathOverflow)?;
    require!(
        after <= reserve.config.borrow_cap as u128,
        AeraError::BorrowCapExceeded
    );
    Ok(())
}

/// Reject a supply that would push one wallet past its personal cap.
pub fn check_per_wallet_cap(reserve: &Reserve, already_supplied: u64, adding: u64) -> Result<()> {
    if reserve.config.per_wallet_supply_cap == 0 {
        return Ok(());
    }
    let after = already_supplied
        .checked_add(adding)
        .ok_or(AeraError::MathOverflow)?;
    require!(
        after <= reserve.config.per_wallet_supply_cap,
        AeraError::PerWalletCapExceeded
    );
    Ok(())
}

/// Reject borrowing from a reserve that is not a borrow reserve. The bCOOK
/// reserve exists to hold collateral, never to be drawn from.
pub fn check_borrow_enabled(reserve: &Reserve) -> Result<()> {
    require!(reserve.config.borrow_enabled, AeraError::BorrowNotEnabled);
    Ok(())
}

/// Reject posting a reserve's share token as collateral when that reserve is
/// not a collateral reserve. This is what refuses aCOOK: the COOK reserve is
/// configured `collateral_enabled = false`, so there is no path that accepts it.
pub fn check_collateral_enabled(reserve: &Reserve) -> Result<()> {
    require!(
        reserve.config.collateral_enabled,
        AeraError::CollateralNotEnabled
    );
    Ok(())
}

/// An isolated collateral may not sit alongside a different collateral in the
/// same obligation. With one collateral listed this never fires; it is here so
/// listing a second one cannot silently create a cross-margined position.
pub fn check_isolation(
    obligation: &Obligation,
    incoming: &Reserve,
    incoming_key: Pubkey,
) -> Result<()> {
    let others_present = obligation
        .deposits
        .iter()
        .any(|d| d.reserve != incoming_key && d.deposited_shares > 0);

    if incoming.config.isolated && others_present {
        return err!(AeraError::CollateralNotEnabled);
    }
    Ok(())
}

/// The post-transaction health check, applied to borrows and withdrawals alike:
/// debt may not exceed the allowed-borrow value.
///
/// Callers pass the values they have already simulated, so this is the single
/// definition of "would this leave the position underwater".
pub fn require_within_borrow_limit(
    projected_borrowed_value: u128,
    allowed_borrow_value: u128,
    error: AeraError,
) -> Result<()> {
    // Written out rather than `require!`, which needs a literal error path and
    // cannot take one chosen by the caller.
    if projected_borrowed_value > allowed_borrow_value {
        return Err(error.into());
    }
    Ok(())
}

/// How much of a borrow one liquidation may close.
///
/// Normally the reserve's close factor (50%). Once health falls below 0.95 the
/// whole position is closable: a position that deep is near insolvency, and
/// forcing a liquidator through repeated partial closes just adds transactions
/// between the protocol and a bad debt.
pub fn effective_close_factor_bps(obligation: &Obligation, repay_reserve: &Reserve) -> Result<u16> {
    match obligation.health_factor_bps()? {
        Some(hf) if hf < FULL_CLOSE_HEALTH_FACTOR_BPS => Ok(BPS_DENOMINATOR as u16),
        _ => Ok(repay_reserve.config.close_factor_bps),
    }
}

/// Maximum liquidity a liquidation may repay against one borrow entry.
pub fn max_repay_amount(
    obligation: &Obligation,
    borrow_index_in_obligation: usize,
    repay_reserve: &Reserve,
) -> Result<u64> {
    let debt = obligation.debt_at(borrow_index_in_obligation, repay_reserve.borrow_index)?;
    let close_factor = effective_close_factor_bps(obligation, repay_reserve)? as u128;
    let capped = mul_div_floor(debt as u128, close_factor, BPS_DENOMINATOR)?;
    u64::try_from(capped).map_err(|_| AeraError::MathOverflow.into())
}

/// Divide an already-computed seizure between Aera and the liquidator.
///
/// # The borrower's penalty does not change
///
/// `seize_shares` is computed exactly as it always was, from the total bonus
/// alone. This function only decides who receives it. A borrower repaying 1,000
/// COOK against a 12% bonus loses collateral worth 1,120 COOK whether Aera's
/// share is 0 or 300 bps; what moves is the split of the 120, never the 1,120.
///
/// Splitting the shares rather than recomputing two seizures is deliberate.
/// Two independent `value -> amount -> shares` conversions would each round
/// down, and the two results would not sum to the total — collateral would go
/// missing, attributed to nobody, and stuck in the obligation's vault.
///
/// # The ratio
///
/// `seize_shares` corresponds to value `repay x (BPS + total_bonus) / BPS`.
/// Aera's claim is `repay x share / BPS`. The `repay` and the `BPS` cancel, so
/// Aera's fraction of the shares is exactly:
///
/// ```text
///   protocol_shares = seize_shares x share_bps / (BPS + total_bonus_bps)
/// ```
///
/// # Rounding
///
/// The protocol's side floors and the liquidator takes the remainder. That is
/// the only rounding in the split, and it is the safe direction on every count:
///
/// - Aera can never receive more than its configured share.
/// - The liquidator can never receive less than its configured share, so the
///   incentive to liquidate is never eroded by arithmetic.
/// - `protocol + liquidator == seize_shares` exactly, for every input.
///
/// A liquidation small enough that Aera's share floors to zero pays Aera
/// nothing. That is correct and deliberately not floored upward: a minimum fee
/// would make the borrower's effective penalty depend on the size of the
/// liquidation, which is exactly what this design refuses to do.
pub fn split_seized_shares(
    seize_shares: u64,
    total_bonus_bps: u16,
    protocol_share_bps: u16,
) -> Result<(u64, u64)> {
    /*
     * Clamped, not merely validated.
     *
     * The share and the bonus live in different accounts -- the share on
     * `RiskConfig`, the bonus on `ReserveConfig` -- and `set_params` can lower
     * the bonus after the share was set. Both admin paths reject the ordering
     * that would produce it, and this still refuses to trust that they did:
     * a share above the bonus would take from the liquidator's principal rather
     * than from the bonus, which is the one outcome this whole design exists to
     * prevent.
     */
    let share = protocol_share_bps.min(total_bonus_bps) as u128;
    if share == 0 {
        return Ok((0, seize_shares));
    }

    let denominator = BPS_DENOMINATOR
        .checked_add(total_bonus_bps as u128)
        .ok_or(AeraError::MathOverflow)?;
    let protocol = mul_div_floor(seize_shares as u128, share, denominator)?;
    let protocol = u64::try_from(protocol).map_err(|_| AeraError::MathOverflow)?;

    // Cannot underflow: `share <= total_bonus_bps < denominator`, so the floored
    // quotient is at most `seize_shares`. Written as a checked subtraction
    // anyway, because "cannot" is a claim about today's callers.
    let liquidator = seize_shares
        .checked_sub(protocol)
        .ok_or(AeraError::MathOverflow)?;
    Ok((protocol, liquidator))
}

/// Collateral shares a liquidator receives for repaying `repay_value` worth of
/// debt, including the bonus.
///
/// Every step rounds down, toward the borrower, so the position is never
/// over-seized by rounding.
pub fn seize_shares_for(
    repay_value: u128,
    collateral_reserve: &Reserve,
    collateral_price_scaled: u128,
) -> Result<u64> {
    let bonus = mul_div_floor(
        repay_value,
        collateral_reserve.config.liquidation_bonus_bps as u128,
        BPS_DENOMINATOR,
    )?;
    let seize_value = repay_value
        .checked_add(bonus)
        .ok_or(AeraError::MathOverflow)?;
    let seize_liquidity = crate::math::value_to_amount(
        seize_value,
        collateral_reserve.liquidity_decimals,
        collateral_price_scaled,
        Rounding::Down,
    )?;
    collateral_reserve.liquidity_to_shares(seize_liquidity, Rounding::Down)
}

#[cfg(test)]
mod split_tests {
    use super::*;

    /*
     * These deliberately do NOT reimplement the formula.
     *
     * `docs/TEST_MATRIX.md` records why: the u128 overflow in the market
     * oracle price maths existed in the program AND in the test mirror of it,
     * so the mirror agreed with the bug and proved nothing. What follows are
     * algebraic identities, bounds and exhaustive sweeps -- properties that hold
     * for a correct split and fail for an incorrect one, whatever it computes.
     */

    /// The single most important property: nothing appears and nothing vanishes.
    #[test]
    fn conservation_holds_for_every_input() {
        for seize in [0u64, 1, 2, 3, 7, 99, 100, 101, 1_000, 999_999, u64::MAX] {
            for bonus in [0u16, 1, 500, 1_200, 1_500, 10_000] {
                for share in [0u16, 1, 25, 150, 300, 1_200, 1_500, 10_000] {
                    let (protocol, liquidator) = split_seized_shares(seize, bonus, share).unwrap();
                    assert_eq!(
                        protocol.checked_add(liquidator),
                        Some(seize),
                        "seize {seize}, bonus {bonus}, share {share}: \
                         {protocol} + {liquidator} != {seize}"
                    );
                }
            }
        }
    }

    /// A share of zero must be identical to the behaviour before Gap D.
    #[test]
    fn a_zero_share_gives_the_liquidator_everything() {
        for seize in [1u64, 2, 1_000, u64::MAX] {
            for bonus in [0u16, 1_200, 1_500] {
                assert_eq!(
                    split_seized_shares(seize, bonus, 0).unwrap(),
                    (0, seize),
                    "a zero share took collateral from the liquidator"
                );
            }
        }
    }

    /// Aera can never be paid more than it is configured to receive.
    ///
    /// Checked as an inequality against exact rational arithmetic in u128,
    /// cross-multiplied so no division rounds the comparison itself.
    #[test]
    fn the_protocol_never_exceeds_its_configured_share() {
        for seize in [1u64, 7, 100, 1_003, 1_000_000, u64::MAX / 2] {
            for bonus in [0u16, 300, 1_200, 1_500] {
                for share in [1u16, 25, 150, 300, 1_200] {
                    let (protocol, _) = split_seized_shares(seize, bonus, share).unwrap();
                    let effective = share.min(bonus) as u128;
                    let denominator = BPS_DENOMINATOR + bonus as u128;
                    assert!(
                        (protocol as u128) * denominator <= (seize as u128) * effective,
                        "seize {seize}, bonus {bonus}, share {share}: protocol {protocol} \
                         exceeds its exact entitlement"
                    );
                }
            }
        }
    }

    /// And is never short by more than the one unit flooring can cost.
    ///
    /// Together with the test above this pins the rounding exactly: floor, and
    /// nothing else.
    #[test]
    fn the_protocol_is_never_short_by_more_than_one_unit() {
        for seize in [1u64, 7, 100, 1_003, 1_000_000] {
            for bonus in [300u16, 1_200, 1_500] {
                for share in [1u16, 25, 150, 300] {
                    let (protocol, _) = split_seized_shares(seize, bonus, share).unwrap();
                    let effective = share.min(bonus) as u128;
                    let denominator = BPS_DENOMINATOR + bonus as u128;
                    assert!(
                        ((protocol as u128) + 1) * denominator > (seize as u128) * effective,
                        "seize {seize}, bonus {bonus}, share {share}: protocol {protocol} \
                         is more than one unit below its entitlement, so the rounding is \
                         not a floor"
                    );
                }
            }
        }
    }

    /// A share above the bonus must be clamped, not applied.
    ///
    /// Above the bonus, Aera would be taking the liquidator principal rather
    /// than a slice of the bonus. Both admin paths refuse to configure it; this
    /// is the third line of defence, because `set_params` can lower a bonus
    /// after a share was set against the old one.
    #[test]
    fn a_share_above_the_bonus_is_clamped_to_the_bonus() {
        let seize = 1_000_000u64;
        let bonus = 1_200u16;
        let at_bonus = split_seized_shares(seize, bonus, bonus).unwrap();
        for excessive in [bonus + 1, 2_000, 5_000, 10_000, u16::MAX] {
            assert_eq!(
                split_seized_shares(seize, bonus, excessive).unwrap(),
                at_bonus,
                "a share of {excessive} bps against a {bonus} bps bonus was not clamped"
            );
        }
    }

    /// At the clamp, the liquidator still keeps the principal-equivalent
    /// collateral. That floor is what makes "carved from the bonus" true.
    #[test]
    fn at_the_full_bonus_the_liquidator_keeps_the_principal_equivalent() {
        let bonus = 1_200u16;
        for seize in [1_120u64, 11_200, 1_120_000, 999_999_999] {
            let (protocol, liquidator) = split_seized_shares(seize, bonus, bonus).unwrap();
            let expected_floor =
                ((seize as u128) * BPS_DENOMINATOR) / (BPS_DENOMINATOR + bonus as u128);
            assert!(
                liquidator as u128 >= expected_floor,
                "seize {seize}: liquidator {liquidator} fell below the principal \
                 equivalent {expected_floor}"
            );
            assert_eq!(protocol as u128 + liquidator as u128, seize as u128);
        }
    }

    /// Raising the share never lowers what Aera receives, and never raises what
    /// the liquidator receives. A non-monotone split would signal that the two
    /// sides were computed independently rather than divided.
    #[test]
    fn the_split_is_monotone_in_the_share() {
        for seize in [1u64, 137, 100_000, 7_777_777] {
            let bonus = 1_200u16;
            let mut previous = (0u64, seize);
            for share in 0..=bonus {
                let current = split_seized_shares(seize, bonus, share).unwrap();
                assert!(
                    current.0 >= previous.0 && current.1 <= previous.1,
                    "seize {seize}: share {share} moved the split backwards"
                );
                previous = current;
            }
        }
    }

    /// Small liquidations round Aera share away entirely, and that is correct.
    ///
    /// A minimum fee would make the borrower effective penalty depend on the
    /// size of the liquidation, which is precisely what this design refuses.
    #[test]
    fn a_small_liquidation_pays_the_protocol_nothing() {
        let (protocol, liquidator) = split_seized_shares(74, 1_200, 150).unwrap();
        assert_eq!(protocol, 0, "a 74-unit seizure paid the protocol something");
        assert_eq!(liquidator, 74);

        let (protocol, _) = split_seized_shares(75, 1_200, 150).unwrap();
        assert_eq!(protocol, 1, "the first unit should land at 75");
    }

    /// The boundary values the market-oracle overflow proved are mandatory.
    #[test]
    fn the_arithmetic_survives_the_extremes() {
        for bonus in [0u16, 1_500, 10_000] {
            for share in [0u16, 1, 300, 10_000] {
                let (protocol, liquidator) = split_seized_shares(u64::MAX, bonus, share).unwrap();
                assert_eq!(protocol.checked_add(liquidator), Some(u64::MAX));
            }
        }
        // A zero seizure cannot divide by zero or produce a phantom unit.
        assert_eq!(split_seized_shares(0, 1_200, 300).unwrap(), (0, 0));
        // A zero bonus leaves nothing to carve from, whatever the share says.
        assert_eq!(split_seized_shares(1_000, 0, 300).unwrap(), (0, 1_000));
    }

    /// Exhaustive over a small range: every seizure from 0 to 5,000 units, at
    /// the COOKHOUSE candidate, satisfies every property at once.
    #[test]
    fn exhaustive_over_a_small_range() {
        let (bonus, share) = (1_200u16, 150u16);
        let denominator = BPS_DENOMINATOR + bonus as u128;
        for seize in 0u64..=5_000 {
            let (protocol, liquidator) = split_seized_shares(seize, bonus, share).unwrap();
            assert_eq!(protocol + liquidator, seize);
            assert!((protocol as u128) * denominator <= (seize as u128) * share as u128);
            assert!(((protocol as u128) + 1) * denominator > (seize as u128) * share as u128);
        }
    }
}
