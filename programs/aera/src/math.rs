//! Integer-only money math. No floats, no fixed-point crates.
//!
//! Adapted from the QuickNode `finance/lending` Anchor example (MIT). The
//! rounding discipline is the important part and is unchanged: the protocol
//! never loses a base unit to rounding, so dust cannot be extracted by repeated
//! round-trips.

use anchor_lang::prelude::*;

use crate::constants::FIXED_POINT_SCALE_DECIMALS;
use crate::errors::AeraError;

/// Which way to break ties when a division truncates. Quantities favourable to
/// the user (collateral value, redeemed liquidity) round DOWN; quantities the
/// user owes (debt, seized collateral basis) round UP.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Rounding {
    Down,
    Up,
}

/// 10^exponent as a u128, erroring instead of wrapping.
pub fn ten_pow(exponent: u32) -> Result<u128> {
    Ok(10u128
        .checked_pow(exponent)
        .ok_or(AeraError::MathOverflow)?)
}

/// floor((a * b) / denominator), computed in u128.
pub fn mul_div_floor(a: u128, b: u128, denominator: u128) -> Result<u128> {
    require!(denominator > 0, AeraError::MathOverflow);
    let product = a.checked_mul(b).ok_or(AeraError::MathOverflow)?;
    Ok(product
        .checked_div(denominator)
        .ok_or(AeraError::MathOverflow)?)
}

/// ceil((a * b) / denominator), computed in u128.
pub fn mul_div_ceil(a: u128, b: u128, denominator: u128) -> Result<u128> {
    require!(denominator > 0, AeraError::MathOverflow);
    let product = a.checked_mul(b).ok_or(AeraError::MathOverflow)?;
    let rounding = denominator.checked_sub(1).ok_or(AeraError::MathOverflow)?;
    Ok(product
        .checked_add(rounding)
        .ok_or(AeraError::MathOverflow)?
        .checked_div(denominator)
        .ok_or(AeraError::MathOverflow)?)
}

pub fn mul_div(a: u128, b: u128, denominator: u128, rounding: Rounding) -> Result<u128> {
    match rounding {
        Rounding::Down => mul_div_floor(a, b, denominator),
        Rounding::Up => mul_div_ceil(a, b, denominator),
    }
}

/// Quote-currency value (FIXED_POINT_SCALE-scaled) of `amount` base units of a
/// token with `decimals`, given `price_scaled` from a feed.
///
/// `price_scaled` already carries the FIXED_POINT_SCALE factor, so the value is
/// `amount * price_scaled / 10^decimals`.
pub fn market_value(
    amount: u64,
    decimals: u8,
    price_scaled: u128,
    rounding: Rounding,
) -> Result<u128> {
    let divisor = ten_pow(decimals as u32)?;
    mul_div(amount as u128, price_scaled, divisor, rounding)
}

/// Inverse of [`market_value`]: how many base units of a token with `decimals`
/// are worth `value_scaled` at `price_scaled`.
pub fn value_to_amount(
    value_scaled: u128,
    decimals: u8,
    price_scaled: u128,
    rounding: Rounding,
) -> Result<u64> {
    let multiplier = ten_pow(decimals as u32)?;
    let amount = mul_div(value_scaled, multiplier, price_scaled, rounding)?;
    u64::try_from(amount).map_err(|_| AeraError::MathOverflow.into())
}

/// Combine a feed's exponent with the fixed-point scale into one net power of
/// ten. `price_scaled = mantissa * 10^(exponent + FIXED_POINT_SCALE_DECIMALS)`.
/// Folding the two avoids forming a 10^18 intermediate that would overflow for
/// high-priced assets.
pub fn price_mantissa_to_scaled(mantissa: u128, exponent: i32) -> Result<u128> {
    let net_exponent = exponent
        .checked_add(FIXED_POINT_SCALE_DECIMALS)
        .ok_or(AeraError::MathOverflow)?;
    if net_exponent >= 0 {
        Ok(mantissa
            .checked_mul(ten_pow(net_exponent as u32)?)
            .ok_or(AeraError::MathOverflow)?)
    } else {
        Ok(mantissa
            .checked_div(ten_pow((-net_exponent) as u32)?)
            .ok_or(AeraError::MathOverflow)?)
    }
}

/// Absolute difference between two scaled prices as a fraction of `previous`,
/// in basis points. Used by the circuit breaker.
pub fn move_bps(previous: u128, next: u128) -> Result<u128> {
    if previous == 0 {
        return Ok(0);
    }
    let delta = previous.abs_diff(next);
    mul_div_floor(delta, crate::constants::BPS_DENOMINATOR, previous)
}
