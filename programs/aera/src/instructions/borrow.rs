//! Borrow COOK against locked bCOOK.
//!
//! Everything that can refuse a borrow is checked here, in order: protocol
//! pause, borrow pause, the reserve's own borrow switch, the circuit breaker
//! (via `price_for_borrow`), the borrow cap, available liquidity, and finally
//! the post-transaction health check.

use anchor_lang::prelude::*;
use anchor_spl::token_interface::{
    transfer_checked, Mint, TokenAccount, TokenInterface, TransferChecked,
};

use crate::constants::{BPS_DENOMINATOR, FIXED_POINT_SCALE};
use crate::errors::AeraError;
use crate::math::{market_value, mul_div_ceil, mul_div_floor, Rounding};
use crate::oracle::breaker::RiskAction;
use crate::risk::{
    check_borrow_cap, check_borrow_enabled, check_per_wallet_borrow_cap,
    require_within_borrow_limit,
};
use crate::state::{reserve_signer_seeds, Global, Obligation, OracleState, Reserve, RiskConfig};

pub fn handle_borrow(context: Context<Borrow>, liquidity_amount: u64) -> Result<()> {
    require!(liquidity_amount > 0, AeraError::ZeroAmount);
    context.accounts.global.require_borrow_enabled()?;

    let clock = Clock::get()?;
    let reserve_key = context.accounts.reserve.key();

    context.accounts.obligation.require_refreshed()?;
    context.accounts.reserve.require_accrued()?;
    check_borrow_enabled(&context.accounts.reserve)?;

    // An oracle on *any* asset this position depends on that will not permit
    // new risk stops the borrow -- the collateral's as much as the borrowed
    // asset's. The flag is recorded by `refresh_obligation`, which is the only
    // handler that sees the collateral oracles.
    require!(
        !context.accounts.obligation.prices_stressed,
        AeraError::OracleBorrowFrozen
    );

    // Borrowing is a risk-increasing action, so it is gated on the oracle's
    // state machine rather than on a single breaker flag. The price itself is
    // the accepted reference: a refused observation never becomes borrowing
    // capacity.
    let oracle = &context.accounts.oracle;
    oracle.require_fresh(clock.slot)?;
    oracle.require_permits(RiskAction::Borrow)?;
    let price_scaled = oracle.effective_rate()?;

    let decimals = context.accounts.reserve.liquidity_decimals;
    let borrow_value = market_value(liquidity_amount, decimals, price_scaled, Rounding::Up)?;

    let projected = context
        .accounts
        .obligation
        .borrowed_value
        .checked_add(borrow_value)
        .ok_or(AeraError::MathOverflow)?;
    require_within_borrow_limit(
        projected,
        context.accounts.obligation.allowed_borrow_value,
        AeraError::BorrowTooLarge,
    )?;

    check_borrow_cap(&context.accounts.reserve, liquidity_amount)?;

    /*
     * The per-wallet cap, measured against what this wallet already owes here.
     *
     * `debt_at` scales the stored principal by the live borrow index, so the
     * comparison is against real debt including accrued interest rather than
     * the principal as it stood when the loan opened. A wallet that borrowed to
     * the cap and then accrued interest is already over it; that is correct and
     * only blocks *new* borrowing, never repayment.
     *
     * `liquidity_amount` is the full new debt: `borrow.rs` charges the borrower
     * the whole amount and pays out `amount - fee`, so the origination fee is
     * inside this figure already. Checking the paid-out amount instead would let
     * a wallet cross the cap by exactly the fee.
     *
     * The obligation PDA is `["obligation", market, owner]` opened with `init`,
     * so one wallet has one obligation per market and cannot split debt across
     * several to evade this.
     */
    {
        /*
         * Read the reserve's per-wallet cap, if it has one.
         *
         * The address is derived here rather than trusted: a RiskConfig from
         * another reserve, or a look-alike account, lands at a different address
         * and is refused. An account that does not exist -- no lamports, no data
         * -- means no limits, which is what zero means everywhere else in Aera.
         */
        let (expected, _) =
            Pubkey::find_program_address(&[RiskConfig::SEED, reserve_key.as_ref()], &crate::ID);
        require_keys_eq!(
            context.accounts.risk_config.key(),
            expected,
            AeraError::MarketMismatch
        );

        let info = context.accounts.risk_config.to_account_info();
        let cap = if info.data_is_empty() {
            0
        } else {
            require_keys_eq!(*info.owner, crate::ID, AeraError::MarketMismatch);
            let data = info.try_borrow_data()?;
            let config = RiskConfig::try_deserialize(&mut &data[..])?;
            require_keys_eq!(config.reserve, reserve_key, AeraError::MarketMismatch);
            config.per_wallet_borrow_cap
        };

        let obligation = &context.accounts.obligation;
        let already_owed = match obligation.find_borrow(reserve_key) {
            Ok(index) => obligation.debt_at(index, context.accounts.reserve.borrow_index)?,
            Err(_) => 0,
        };
        check_per_wallet_borrow_cap(cap, already_owed, liquidity_amount)?;
    }

    require!(
        liquidity_amount <= context.accounts.reserve.available_liquidity,
        AeraError::InsufficientReserveLiquidity
    );

    // Principal is scaled by the current index, rounded up, so the borrower is
    // never credited a sub-unit of free principal.
    let scaled_added = mul_div_ceil(
        liquidity_amount as u128,
        FIXED_POINT_SCALE,
        context.accounts.reserve.borrow_index,
    )?;

    // The origination fee, if one is configured. The borrower owes the full
    // `liquidity_amount` and receives `liquidity_amount - fee`; the fee stays in
    // the vault and is recognised immediately as protocol revenue.
    //
    // Suppliers' claim on the pool is unchanged by this: `available_liquidity`
    // falls by the amount actually paid out, `accrued_fees` rises by the fee,
    // and `total_liquidity()` (available + borrowed - fees) nets to where it
    // was. The fee comes from the borrower, not from the suppliers.
    let fee = u64::try_from(mul_div_floor(
        liquidity_amount as u128,
        context.accounts.reserve.config.origination_fee_bps as u128,
        BPS_DENOMINATOR,
    )?)
    .map_err(|_| AeraError::MathOverflow)?;
    let paid_out = liquidity_amount
        .checked_sub(fee)
        .ok_or(AeraError::MathOverflow)?;
    require!(paid_out > 0, AeraError::ZeroAmount);

    {
        let reserve = &mut context.accounts.reserve;
        reserve.borrowed_principal = reserve
            .borrowed_principal
            .checked_add(scaled_added)
            .ok_or(AeraError::MathOverflow)?;
        reserve.available_liquidity = reserve
            .available_liquidity
            .checked_sub(paid_out)
            .ok_or(AeraError::MathOverflow)?;
        reserve.accrued_fees = reserve
            .accrued_fees
            .checked_add(fee)
            .ok_or(AeraError::MathOverflow)?;
    }

    {
        let obligation = &mut context.accounts.obligation;
        let index = obligation.upsert_borrow(reserve_key)?;
        obligation.borrows[index].borrowed_principal = obligation.borrows[index]
            .borrowed_principal
            .checked_add(scaled_added)
            .ok_or(AeraError::MathOverflow)?;
        obligation.stale = true;
    }

    let reserve = &context.accounts.reserve;
    let bump = [reserve.bump];
    let seeds = reserve_signer_seeds(&reserve.market, &reserve.liquidity_mint, &bump);
    transfer_checked(
        CpiContext::new_with_signer(
            context.accounts.liquidity_token_program.key(),
            TransferChecked {
                from: context.accounts.liquidity_vault.to_account_info(),
                mint: context.accounts.liquidity_mint.to_account_info(),
                to: context.accounts.user_liquidity.to_account_info(),
                authority: reserve.to_account_info(),
            },
            &[&seeds],
        ),
        paid_out,
        decimals,
    )?;

    Ok(())
}

#[derive(Accounts)]
pub struct Borrow<'info> {
    pub global: Box<Account<'info, Global>>,

    #[account(mut, has_one = owner)]
    pub obligation: Box<Account<'info, Obligation>>,

    pub owner: Signer<'info>,

    #[account(
        mut,
        has_one = liquidity_mint,
        has_one = liquidity_vault,
        has_one = oracle,
        constraint = reserve.market == obligation.market @ AeraError::MarketMismatch,
    )]
    pub reserve: Box<Account<'info, Reserve>>,

    pub oracle: Box<Account<'info, OracleState>>,

    pub liquidity_mint: Box<InterfaceAccount<'info, Mint>>,

    #[account(mut)]
    pub liquidity_vault: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(mut)]
    pub user_liquidity: Box<InterfaceAccount<'info, TokenAccount>>,

    pub liquidity_token_program: Interface<'info, TokenInterface>,

    /// CHECK: verified in the handler against `["risk_config", reserve]`.
    ///
    /// The reserve's risk limits, which may not exist.
    ///
    /// **Required, not `Option`.** An `Option<Account<..>>` is omittable by the
    /// caller, so a borrower could evade the cap simply by not passing it. This
    /// account must always be supplied; when the reserve has no limits the
    /// caller passes the derived address of an account that was never created,
    /// and the handler reads it as unlimited.
    ///
    /// It cannot be substituted either: the handler derives the expected address
    /// from this reserve and refuses anything else, so a RiskConfig belonging to
    /// a different reserve is rejected.
    ///
    /// It lives beside the reserve rather than inside it because `Reserve` could
    /// not grow -- `migrate.rs:145` records that a longer `Reserve` leaves older
    /// accounts undeserialisable, which breaks `accrue`, which breaks repayment
    /// for the whole migration window.
    pub risk_config: UncheckedAccount<'info>,
}
