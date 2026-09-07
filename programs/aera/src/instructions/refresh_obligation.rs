//! Recompute an obligation's values from the current state of every reserve it
//! touches.
//!
//! Reserve and oracle accounts arrive as `remaining_accounts`, two per entry —
//! first the deposit reserves in `obligation.deposits` order, then the borrow
//! reserves in `obligation.borrows` order — each as `[reserve, oracle]`. Every
//! reserve must already be accrued this slot, and every oracle refreshed this
//! slot.
//!
//! Collateral value floors and debt value ceils, so health is always evaluated
//! conservatively against the borrower. The haircut is applied here, once, and
//! both limits derive from the haircut value — matching the spec's
//! `collateral_value = q * price * (1 - haircut)` and
//! `max_borrow = collateral_value * LTV`.

use anchor_lang::prelude::*;

use crate::constants::BPS_DENOMINATOR;
use crate::errors::AeraError;
use crate::math::{market_value, mul_div_floor, Rounding};
use crate::risk::apply_haircut;
use crate::state::{Obligation, OracleState, Reserve};

pub fn handle_refresh_obligation(context: Context<RefreshObligation>) -> Result<()> {
    let clock = Clock::get()?;
    let obligation = &mut context.accounts.obligation;
    let market = obligation.market;
    let accounts = context.remaining_accounts;
    let mut cursor = 0usize;

    let mut deposited_value: u128 = 0;
    let mut effective_collateral_value: u128 = 0;
    let mut allowed_borrow_value: u128 = 0;
    let mut unhealthy_borrow_value: u128 = 0;
    let mut prices_stressed = false;

    for collateral in obligation.deposits.iter_mut() {
        let (reserve, price_scaled, tripped) = read_pair(
            accounts,
            &mut cursor,
            collateral.reserve,
            market,
            clock.unix_timestamp,
            clock.slot,
        )?;

        let liquidity = reserve.shares_to_liquidity(collateral.deposited_shares, Rounding::Down)?;
        let face = market_value(
            liquidity,
            reserve.liquidity_decimals,
            price_scaled,
            Rounding::Down,
        )?;
        let effective = apply_haircut(face, reserve.config.collateral_haircut_bps)?;

        prices_stressed |= tripped;
        collateral.market_value = face;

        deposited_value = deposited_value
            .checked_add(face)
            .ok_or(AeraError::MathOverflow)?;
        effective_collateral_value = effective_collateral_value
            .checked_add(effective)
            .ok_or(AeraError::MathOverflow)?;
        allowed_borrow_value = allowed_borrow_value
            .checked_add(mul_div_floor(
                effective,
                reserve.config.loan_to_value_bps as u128,
                BPS_DENOMINATOR,
            )?)
            .ok_or(AeraError::MathOverflow)?;
        unhealthy_borrow_value = unhealthy_borrow_value
            .checked_add(mul_div_floor(
                effective,
                reserve.config.liquidation_threshold_bps as u128,
                BPS_DENOMINATOR,
            )?)
            .ok_or(AeraError::MathOverflow)?;
    }

    let mut borrowed_value: u128 = 0;
    for (index, borrow) in obligation.borrows.iter_mut().enumerate() {
        let (reserve, price_scaled, tripped) = read_pair(
            accounts,
            &mut cursor,
            borrow.reserve,
            market,
            clock.unix_timestamp,
            clock.slot,
        )?;

        // Recomputed inline rather than through `Obligation::debt_at`, which
        // would need a borrow of `self` we already hold mutably here.
        let _ = index;
        let exact = borrow
            .borrowed_principal
            .checked_mul(reserve.borrow_index)
            .ok_or(AeraError::MathOverflow)?;
        let floor = exact / crate::constants::FIXED_POINT_SCALE;
        let debt = if exact % crate::constants::FIXED_POINT_SCALE == 0 {
            floor
        } else {
            floor.checked_add(1).ok_or(AeraError::MathOverflow)?
        };
        let debt = u64::try_from(debt).map_err(|_| AeraError::MathOverflow)?;

        let value = market_value(debt, reserve.liquidity_decimals, price_scaled, Rounding::Up)?;
        prices_stressed |= tripped;
        borrow.market_value = value;
        borrowed_value = borrowed_value
            .checked_add(value)
            .ok_or(AeraError::MathOverflow)?;
    }

    require!(
        cursor == accounts.len(),
        AeraError::InvalidObligationAccount
    );

    obligation.deposited_value = deposited_value;
    obligation.effective_collateral_value = effective_collateral_value;
    obligation.allowed_borrow_value = allowed_borrow_value;
    obligation.unhealthy_borrow_value = unhealthy_borrow_value;
    obligation.borrowed_value = borrowed_value;
    obligation.last_update_slot = clock.slot;
    obligation.stale = false;
    obligation.prices_stressed = prices_stressed;
    Ok(())
}

/// Read the next `[reserve, oracle]` pair, checking it matches the obligation's
/// stored reserve, belongs to the obligation's market, that the reserve was
/// accrued this slot and the oracle refreshed this slot.
///
/// Values from the oracle's **accepted reference**, never from an observation
/// the breaker refused. A frozen oracle must still be able to value an existing
/// position, or repay and liquidate would break exactly when they are needed —
/// which is the whole reason the reference is held rather than overwritten.
fn read_pair<'a, 'info>(
    accounts: &'a [AccountInfo<'info>],
    cursor: &mut usize,
    expected_reserve: Pubkey,
    market: Pubkey,
    now: i64,
    slot: u64,
) -> Result<(Reserve, u128, bool)>
where
    'a: 'info,
{
    let reserve_info = accounts
        .get(*cursor)
        .ok_or(AeraError::InvalidObligationAccount)?;
    let price_info = accounts
        .get(*cursor + 1)
        .ok_or(AeraError::InvalidObligationAccount)?;
    *cursor += 2;

    require_keys_eq!(
        reserve_info.key(),
        expected_reserve,
        AeraError::InvalidObligationAccount
    );
    let reserve = Account::<Reserve>::try_from(reserve_info)?;
    require_keys_eq!(reserve.market, market, AeraError::MarketMismatch);
    reserve.require_accrued()?;

    require_keys_eq!(
        price_info.key(),
        reserve.oracle,
        AeraError::InvalidObligationAccount
    );
    let oracle = Account::<OracleState>::try_from(price_info)?;
    oracle.require_fresh(slot)?;
    let price_scaled = oracle.effective_rate()?;

    /*
     * "Stressed" means an oracle this position depends on will not permit new
     * risk -- not merely that it is less than perfectly healthy.
     *
     * The distinction matters because RATE_WARNING permits everything. Treating
     * any non-Healthy state as stressed would block borrowing on a pool that is
     * simply a few epochs behind on its crank, which is a safe condition and
     * the wrong thing to halt the market for.
     *
     * It is recorded here because this is the only handler that sees the
     * *collateral* oracles; `borrow` sees only the borrowed asset's.
     */
    let stressed = !oracle
        .health()?
        .permits(crate::oracle::breaker::RiskAction::Borrow);

    let _ = now;
    Ok((reserve.into_inner(), price_scaled, stressed))
}

#[derive(Accounts)]
pub struct RefreshObligation<'info> {
    #[account(mut)]
    pub obligation: Box<Account<'info, Obligation>>,
}
