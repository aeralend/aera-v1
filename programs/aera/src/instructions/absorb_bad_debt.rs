//! Recognising debt that will never be repaid.
//!
//! A position can end up owing more than its collateral is worth. The rate
//! falls faster than liquidators act, and on Cookie Chain the collateral is a
//! staking receipt whose rate steps once per 53-hour epoch, so "faster" is not
//! a hypothetical. Liquidation recovers whatever is there; when the collateral
//! is gone and debt remains, there is nothing left to recover.
//!
//! ## The problem this solves is accounting, not the loss
//!
//! The loss has already happened by the time this instruction can be called.
//! What has *not* happened is anyone recognising it.
//!
//! `Reserve::total_liquidity()` is what the share exchange rate is computed
//! from, and it counts `borrowed_principal` as an asset. Debt with no
//! collateral behind it is therefore counted as if it would be repaid, and
//! every supplier is quoted a redemption value the vault cannot pay. That is
//! not a rounding problem: it is a first-mover advantage. Suppliers who
//! withdraw early are paid in full out of the claims of the ones still in, and
//! the last ones out absorb the entire shortfall.
//!
//! Recognition converts that into what it actually is: a loss shared pro rata
//! by everyone holding the share token at the moment it is recognised.
//!
//! ## Who pays
//!
//! **The suppliers of the borrowed asset.** There is no insurance fund, no
//! protocol capital and no backstop. This is stated plainly here, in
//! `docs/KNOWN_RISKS.md`, and in the risk disclosures, because it is the single
//! most important thing a supplier needs to understand about the position they
//! are taking.
//!
//! ## Why it is permissionless
//!
//! The precondition -- an obligation with debt and no collateral -- is a fact
//! about accounts the program can verify. The effect only ever *reduces* what
//! the protocol claims to hold, so there is no version of calling this that
//! profits the caller. Requiring an admin would mean the honest number waits on
//! a human, and the interval between the loss and the admin noticing is exactly
//! the window in which the share rate is a lie.
//!
//! ## Why the collateral check is total, not per-reserve
//!
//! The obligation must hold *no* collateral anywhere, not merely none in this
//! reserve. A borrower with bCOOK posted against one debt and none against
//! another is still a borrower whose debt can be recovered by liquidating what
//! they have. Writing anything off while any collateral remains would delete a
//! claim the suppliers could still have been paid from.
//!
//! ## Why there is no `Reserve::bad_debt` field
//!
//! The obvious design is a running total on the reserve, subtracted from
//! `total_liquidity`. It was built that way first, and then removed, because
//! adding a field grows `Reserve` -- and a v0.1 `Reserve` that v0.2 cannot
//! deserialise cannot be passed to `accrue`, and every repayment needs an
//! accrued reserve. A sixteen-byte field would therefore have trapped every
//! borrower for the whole duration of a v0.1 -> v0.2 migration.
//! `test_half_migrated::half_06` caught it.
//!
//! Repayment always being available is the rule the rest of this design bends
//! around, and it outranks the convenience of a counter. So the write-off
//! removes the principal from `Reserve::borrowed_principal` outright: the
//! phantom asset stops being counted, which is the substantive requirement,
//! and the share exchange rate falls to the truth on the next read.
//!
//! What is lost is a state field a reader could poll. What replaces it is the
//! `BadDebtAbsorbed` event, which records the obligation, the amount and the
//! slot permanently in the ledger. Aera's off-chain monitor sums those events and
//! reports cumulative bad debt, so the number is available to anyone who wants
//! it -- just not as a single account read.
//!
//! ## What this does not do
//!
//! It does not decide that a loss has occurred. It recognises one that already
//! has, on the evidence of a borrower with debt and no collateral. Nothing here
//! can be called on a position a liquidator could still act on, which is the
//! guard that stops it being a way to destroy recoverable claims.

use anchor_lang::prelude::*;

use crate::constants::FIXED_POINT_SCALE;
use crate::errors::AeraError;
use crate::math::mul_div_ceil;
use crate::state::{Market, Obligation, Reserve};

#[event]
pub struct BadDebtAbsorbed {
    pub market: Pubkey,
    pub reserve: Pubkey,
    pub obligation: Pubkey,
    /// The amount written off, in the reserve's base units, at the index
    /// current when it was recognised.
    ///
    /// This event is the permanent record. There is no `Reserve::bad_debt`
    /// field -- see the module docs for why -- so a reader wanting cumulative
    /// bad debt sums these.
    pub amount: u64,
    /// The scaled principal removed from `Reserve::borrowed_principal`.
    pub principal_removed: u128,
    pub slot: u64,
}

pub fn handle_absorb_bad_debt(context: Context<AbsorbBadDebt>) -> Result<()> {
    let clock = Clock::get()?;
    let obligation = &mut context.accounts.obligation;
    let reserve = &mut context.accounts.reserve;

    // Both must be current. A stale obligation could report collateral it no
    // longer has, or fail to report collateral it does -- and the second of
    // those would write off a recoverable debt.
    obligation.require_refreshed()?;
    reserve.require_accrued()?;

    /*
     * Nothing anywhere. Checked against the obligation's own deposit entries
     * rather than against a value, so a mispriced or frozen oracle cannot make
     * collateral look absent.
     */
    let collateral: u64 = obligation
        .deposits
        .iter()
        .map(|deposit| deposit.deposited_shares)
        .sum();
    require!(collateral == 0, AeraError::ObligationHasCollateral);

    let borrow_index = obligation.find_borrow(reserve.key())?;
    let principal = obligation.borrows[borrow_index].borrowed_principal;
    require!(principal > 0, AeraError::NoBadDebt);

    /*
     * Ceiling, so the write-off is never smaller than what was actually owed.
     *
     * Every other rounding decision in this program favours the protocol.
     * This one favours honesty: understating the loss would leave a sliver of
     * phantom asset in `total_liquidity`, which is the exact defect being
     * fixed. One base unit in the suppliers' disfavour is the correct
     * direction here, and it is the only place in the program where that is
     * true.
     */
    let owed = mul_div_ceil(principal, reserve.borrow_index, FIXED_POINT_SCALE)?;
    let owed_u64 = u64::try_from(owed).map_err(|_| AeraError::MathOverflow)?;

    /*
     * Off the reserve's books as an asset.
     *
     * `gross_liquidity` counts `borrowed_principal`, which is correct for debt
     * that will be repaid and a lie for debt that will not. Removing it is what
     * makes the share exchange rate honest, and it is the whole economic effect
     * of this instruction: the loss moves from being invisible to being shared
     * pro rata by everyone holding the share token right now.
     */
    reserve.borrowed_principal = reserve
        .borrowed_principal
        .checked_sub(principal)
        .ok_or(AeraError::MathOverflow)?;

    /*
     * The obligation's entry is cleared.
     *
     * Not the debt being forgotten: the event below records the obligation, the
     * amount and the slot permanently. It is the obligation being closed out,
     * because there is no collateral at stake and therefore no borrower with a
     * reason to return. Leaving the entry would let a later repayment reduce a
     * `reserve.borrowed_principal` the write-off has already removed, which
     * underflows and fails -- so the borrower could not repay even if they
     * wanted to, and the reserve's books would be wrong either way.
     */
    obligation.borrows.remove(borrow_index);
    obligation.stale = true;

    emit!(BadDebtAbsorbed {
        market: context.accounts.market.key(),
        reserve: reserve.key(),
        obligation: obligation.key(),
        amount: owed_u64,
        principal_removed: principal,
        slot: clock.slot,
    });

    msg!(
        "aera: bad debt absorbed, {} base units written off",
        owed_u64
    );

    Ok(())
}

#[derive(Accounts)]
pub struct AbsorbBadDebt<'info> {
    pub market: Box<Account<'info, Market>>,

    #[account(
        mut,
        constraint = obligation.market == market.key() @ AeraError::MarketMismatch,
    )]
    pub obligation: Box<Account<'info, Obligation>>,

    #[account(
        mut,
        constraint = reserve.market == market.key() @ AeraError::MarketMismatch,
    )]
    pub reserve: Box<Account<'info, Reserve>>,
    //
    // No signer. See the module comment: the precondition is verifiable on
    // chain and the effect cannot profit whoever calls it.
}
