//! Close part of an unhealthy position and seize collateral plus the bonus.
//!
//! Permissionless. The close factor comes from the repay reserve (it is a
//! property of the debt being closed) and escalates to 100% below HF 0.95; the
//! bonus comes from the collateral reserve (it prices what is seized).
//!
//! Never gated by the protocol pause or the circuit breaker: liquidation is how
//! the pool avoids bad debt, and it is needed most in exactly the conditions
//! that trip those switches.
//!
//! If the requested repayment would seize more collateral than the position
//! holds, the call fails rather than silently capping — capping would make the
//! liquidator pay full price for less collateral.
//!
//! Self-liquidation is not blocked: it is only possible while unhealthy and is
//! economically pointless, matching Solend and Kamino.

use anchor_lang::prelude::*;
use anchor_spl::token_2022::Token2022;
use anchor_spl::token_interface::{
    transfer_checked, Mint, TokenAccount, TokenInterface, TransferChecked,
};

use crate::constants::{FIXED_POINT_SCALE, OBLIGATION_SHARE_VAULT_SEED};
use crate::errors::AeraError;
use crate::math::{market_value, mul_div_floor, Rounding};
use crate::oracle::breaker::RiskAction;
use crate::risk::{max_repay_amount, seize_shares_for, split_seized_shares};
use crate::state::{obligation_signer_seeds, Global, Obligation, OracleState, Reserve, RiskConfig};

pub fn handle_liquidate(context: Context<Liquidate>, liquidity_amount: u64) -> Result<()> {
    require!(liquidity_amount > 0, AeraError::ZeroAmount);
    let clock = Clock::get()?;

    context.accounts.obligation.require_refreshed()?;
    context.accounts.repay_reserve.require_accrued()?;
    context.accounts.collateral_reserve.require_accrued()?;

    let obligation = &context.accounts.obligation;
    let repay_reserve = &context.accounts.repay_reserve;
    let collateral_reserve = &context.accounts.collateral_reserve;

    require!(obligation.is_liquidatable(), AeraError::ObligationHealthy);

    /*
     * Liquidation prices from the accepted reference in every oracle state,
     * including EMERGENCY.
     *
     * Blocking it during an incident would let bad debt accumulate for exactly
     * as long as the incident lasts, which is when the protocol can least
     * afford it. The reference is the last rate the breaker was willing to
     * stand behind, so this is "the last trusted price" rather than a suspect
     * one -- and it is deliberately not the refused observation.
     */
    let repay_oracle = &context.accounts.repay_oracle;
    let collateral_oracle = &context.accounts.collateral_oracle;
    repay_oracle.require_fresh(clock.slot)?;
    collateral_oracle.require_fresh(clock.slot)?;
    repay_oracle.require_permits(RiskAction::Liquidate)?;
    collateral_oracle.require_permits(RiskAction::Liquidate)?;

    let repay_price = repay_oracle.effective_rate()?;
    let collateral_price = collateral_oracle.effective_rate()?;

    let borrow_index = obligation.find_borrow(repay_reserve.key())?;
    let collateral_index = obligation.find_collateral(collateral_reserve.key())?;
    let principal = obligation.borrows[borrow_index].borrowed_principal;
    let deposited_shares = obligation.deposits[collateral_index].deposited_shares;

    let max_repay = max_repay_amount(obligation, borrow_index, repay_reserve)?;
    let repay = liquidity_amount.min(max_repay);
    require!(repay > 0, AeraError::ZeroAmount);

    // Value of the repayment, floored, then bonus applied and converted into
    // collateral shares. Every step rounds toward the borrower.
    let repay_value = market_value(
        repay,
        repay_reserve.liquidity_decimals,
        repay_price,
        Rounding::Down,
    )?;
    let seize_shares = seize_shares_for(repay_value, collateral_reserve, collateral_price)?;
    require!(seize_shares > 0, AeraError::ZeroAmount);
    require!(
        seize_shares <= deposited_shares,
        AeraError::LiquidationTooLarge
    );

    /*
     * Aera's share of the bonus, carved out of the seizure just computed.
     *
     * `seize_shares` above is untouched by this, and that is the whole point:
     * the borrower loses the same collateral whether Aera's share is zero or
     * three hundred basis points. See `risk::split_seized_shares`.
     *
     * The share lives on the COLLATERAL reserve's `RiskConfig`, beside the bonus
     * it divides. A reserve with no `RiskConfig` -- which is every Core reserve
     * -- reads as zero and takes the branch below that touches nothing at all.
     */
    let protocol_share_bps = {
        let expected = Pubkey::find_program_address(
            &[RiskConfig::SEED, collateral_reserve.key().as_ref()],
            &crate::ID,
        )
        .0;
        require_keys_eq!(
            context.accounts.collateral_risk_config.key(),
            expected,
            AeraError::MarketMismatch
        );

        let info = context.accounts.collateral_risk_config.to_account_info();
        if info.data_is_empty() {
            0
        } else {
            require_keys_eq!(*info.owner, crate::ID, AeraError::MarketMismatch);
            let data = info.try_borrow_data()?;
            let config = RiskConfig::try_deserialize(&mut &data[..])?;
            require_keys_eq!(
                config.reserve,
                collateral_reserve.key(),
                AeraError::MarketMismatch
            );
            config.protocol_liquidation_share_bps
        }
    };

    let (protocol_shares, liquidator_shares) = split_seized_shares(
        seize_shares,
        collateral_reserve.config.liquidation_bonus_bps,
        protocol_share_bps,
    )?;

    let scaled_removed =
        mul_div_floor(repay as u128, FIXED_POINT_SCALE, repay_reserve.borrow_index)?.min(principal);

    {
        let repay_reserve = &mut context.accounts.repay_reserve;
        repay_reserve.borrowed_principal = repay_reserve
            .borrowed_principal
            .checked_sub(scaled_removed)
            .ok_or(AeraError::MathOverflow)?;
        repay_reserve.available_liquidity = repay_reserve
            .available_liquidity
            .checked_add(repay)
            .ok_or(AeraError::MathOverflow)?;
    }

    let (market, owner, obligation_bump) = {
        let obligation = &mut context.accounts.obligation;
        obligation.borrows[borrow_index].borrowed_principal = principal
            .checked_sub(scaled_removed)
            .ok_or(AeraError::MathOverflow)?;
        if obligation.borrows[borrow_index].borrowed_principal == 0 {
            obligation.borrows.remove(borrow_index);
        }
        obligation.deposits[collateral_index].deposited_shares = deposited_shares
            .checked_sub(seize_shares)
            .ok_or(AeraError::MathOverflow)?;
        if obligation.deposits[collateral_index].deposited_shares == 0 {
            obligation.deposits.remove(collateral_index);
        }
        obligation.stale = true;
        (obligation.market, obligation.owner, obligation.bump)
    };

    emit!(Liquidated {
        obligation: context.accounts.obligation.key(),
        liquidator: context.accounts.liquidator.key(),
        repaid: repay,
        seized_shares: seize_shares,
        liquidator_shares,
        protocol_shares,
        total_bonus_bps: context
            .accounts
            .collateral_reserve
            .config
            .liquidation_bonus_bps,
        protocol_share_bps,
    });

    // Interactions: liquidator repays in liquidity, then receives the seized
    // shares. The two legs are different token programs.
    transfer_checked(
        CpiContext::new(
            context.accounts.liquidity_token_program.key(),
            TransferChecked {
                from: context.accounts.liquidator_repay_source.to_account_info(),
                mint: context.accounts.repay_liquidity_mint.to_account_info(),
                to: context.accounts.repay_liquidity_vault.to_account_info(),
                authority: context.accounts.liquidator.to_account_info(),
            },
        ),
        repay,
        context.accounts.repay_reserve.liquidity_decimals,
    )?;

    let bump = [obligation_bump];
    let seeds = obligation_signer_seeds(&market, &owner, &bump);
    transfer_checked(
        CpiContext::new_with_signer(
            context.accounts.share_token_program.key(),
            TransferChecked {
                from: context
                    .accounts
                    .obligation_collateral_vault
                    .to_account_info(),
                mint: context.accounts.collateral_share_mint.to_account_info(),
                to: context
                    .accounts
                    .liquidator_collateral_dest
                    .to_account_info(),
                authority: context.accounts.obligation.to_account_info(),
            },
            &[&seeds],
        ),
        liquidator_shares,
        context.accounts.collateral_share_mint.decimals,
    )?;

    /*
     * Aera's leg, and only when there is something to send.
     *
     * Skipping it at zero is not an optimisation. It means a reserve with no
     * protocol share -- every Core reserve -- performs exactly the transfers it
     * performed before Gap D existed and never touches a fee account at all. A
     * tiny liquidation whose share floors away takes the same path.
     */
    if protocol_shares > 0 {
        let destination = &context.accounts.protocol_collateral_dest;
        let info = destination.to_account_info();

        // Owned by the SAME token program that owns the shares. A share mint is
        // Token-2022, so a legacy account could not hold it anyway.
        require_keys_eq!(
            *info.owner,
            context.accounts.share_token_program.key(),
            AeraError::WrongFeeDestination
        );

        let token = {
            let data = info.try_borrow_data()?;
            TokenAccount::try_deserialize(&mut &data[..])
                .map_err(|_| AeraError::WrongFeeDestination)?
        };

        /*
         * The caller cannot choose where Aera's cut goes.
         *
         * Both halves are pinned: the mint must be this collateral's share mint,
         * and the authority must be the fee destination the admin already set in
         * `Global`. That is the same owner `collect_fees` pays interest to, so
         * Gap D introduces no second mutable destination to secure -- and
         * because `fee_destination` is an *owner* rather than a token account,
         * it holds one account per collateral asset with no further config.
         */
        require_keys_eq!(
            token.mint,
            context.accounts.collateral_share_mint.key(),
            AeraError::WrongFeeDestination
        );
        require_keys_eq!(
            token.owner,
            context.accounts.global.fee_destination,
            AeraError::WrongFeeDestination
        );

        transfer_checked(
            CpiContext::new_with_signer(
                context.accounts.share_token_program.key(),
                TransferChecked {
                    from: context
                        .accounts
                        .obligation_collateral_vault
                        .to_account_info(),
                    mint: context.accounts.collateral_share_mint.to_account_info(),
                    to: info.clone(),
                    authority: context.accounts.obligation.to_account_info(),
                },
                &[&seeds],
            ),
            protocol_shares,
            context.accounts.collateral_share_mint.decimals,
        )?;
    }

    Ok(())
}

/// Emitted once per liquidation.
///
/// `seized_shares` keeps its meaning exactly -- everything taken from the
/// borrower -- so an existing consumer reading it still gets the borrower's
/// total penalty and needs no change. The four fields after it are additive,
/// and `liquidator_shares + protocol_shares == seized_shares` always holds.
#[event]
pub struct Liquidated {
    pub obligation: Pubkey,
    pub liquidator: Pubkey,
    pub repaid: u64,
    /// Everything removed from the borrower. Unchanged by the protocol share.
    pub seized_shares: u64,
    /// What the liquidator actually received.
    pub liquidator_shares: u64,
    /// What Aera received. Zero for Core, and zero whenever the share rounds
    /// away on a small liquidation.
    pub protocol_shares: u64,
    /// The collateral reserve's total bonus, for context on the split.
    pub total_bonus_bps: u16,
    /// The configured share, before rounding. `protocol_shares` may be zero
    /// while this is not.
    pub protocol_share_bps: u16,
}

// Liquidation touches 13 accounts; every Account/InterfaceAccount is boxed so
// deserialization happens on the heap and stays within the BPF stack frame.
#[derive(Accounts)]
pub struct Liquidate<'info> {
    /// Read only, and never a gate: liquidation ignores `paused` exactly as it
    /// did before. It is here so Aera's share can be checked against
    /// `fee_destination`.
    pub global: Box<Account<'info, Global>>,

    #[account(mut)]
    pub obligation: Box<Account<'info, Obligation>>,

    pub liquidator: Signer<'info>,

    #[account(
        mut,
        constraint = repay_reserve.market == obligation.market @ AeraError::MarketMismatch,
    )]
    pub repay_reserve: Box<Account<'info, Reserve>>,

    #[account(
        constraint = collateral_reserve.market == obligation.market @ AeraError::MarketMismatch,
    )]
    pub collateral_reserve: Box<Account<'info, Reserve>>,

    #[account(address = repay_reserve.oracle)]
    pub repay_oracle: Box<Account<'info, OracleState>>,

    #[account(address = collateral_reserve.oracle)]
    pub collateral_oracle: Box<Account<'info, OracleState>>,

    #[account(address = repay_reserve.liquidity_mint)]
    pub repay_liquidity_mint: Box<InterfaceAccount<'info, Mint>>,

    #[account(address = collateral_reserve.share_mint)]
    pub collateral_share_mint: Box<InterfaceAccount<'info, Mint>>,

    #[account(mut, address = repay_reserve.liquidity_vault)]
    pub repay_liquidity_vault: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(
        mut,
        seeds = [OBLIGATION_SHARE_VAULT_SEED, collateral_reserve.key().as_ref(), obligation.key().as_ref()],
        bump,
        token::mint = collateral_share_mint,
        token::authority = obligation,
        token::token_program = share_token_program,
    )]
    pub obligation_collateral_vault: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(mut)]
    pub liquidator_repay_source: Box<InterfaceAccount<'info, TokenAccount>>,

    #[account(mut)]
    pub liquidator_collateral_dest: Box<InterfaceAccount<'info, TokenAccount>>,

    pub liquidity_token_program: Interface<'info, TokenInterface>,

    /// CHECK: verified in the handler against `["risk_config", collateral_reserve]`.
    ///
    /// The collateral reserve's risk limits, carrying Aera's share of the
    /// liquidation bonus. Follows `borrow`'s pattern exactly: **required, not
    /// `Option`**, because an `Option` is omittable and a liquidator could then
    /// keep Aera's share simply by leaving it out. A reserve with no limits is
    /// addressed by its derived-but-uncreated PDA and reads as a zero share.
    pub collateral_risk_config: UncheckedAccount<'info>,

    /// CHECK: verified in the handler, and only when Aera is owed something.
    ///
    /// Where Aera's share goes: a token account for this collateral's share
    /// mint, owned by `global.fee_destination`.
    ///
    /// Unchecked rather than typed on purpose. A typed account is deserialised
    /// on every call, so a liquidation of a reserve that owes Aera nothing would
    /// start depending on a fee account existing and being well-formed.
    /// Liquidation is the last operation that should acquire a new way to fail.
    ///
    /// `mut` because it receives a transfer. That marks it writable and nothing
    /// more -- who may be written to is decided entirely by the checks in the
    /// handler, which pin both the mint and the authority.
    #[account(mut)]
    pub protocol_collateral_dest: UncheckedAccount<'info>,

    /// The seized collateral is a share token, so Token-2022.
    pub share_token_program: Program<'info, Token2022>,
}
