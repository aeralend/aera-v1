//! Re-derive an asset's price from its source and judge it.
//!
//! Permissionless, and deliberately so: there is nothing to authorize, because
//! the caller supplies no price and cannot influence the result. Anyone may
//! crank it, and everyone gets the same answer.
//!
//! Every risk-changing instruction requires the oracle to have been refreshed
//! **in the same transaction**, mirroring the discipline `accrue` already
//! imposes on interest. That is what closes the stale-price window: a value
//! read from an earlier slot is a value an attacker had time to arrange around.
//!
//! ## Why this cannot fail when the source is bad
//!
//! The circuit breaker is only useful if its verdict survives. If this
//! instruction returned an error when it disliked what it read, the transaction
//! would roll back and the freeze would be forgotten — so the next caller would
//! see a pristine oracle and try again, forever.
//!
//! So a source that reads badly is *recorded*, not rejected: the oracle drops
//! to `Emergency`, the reference is left where it was, and the instruction
//! succeeds. The refusal then happens in `borrow`, where it belongs.
//!
//! The one thing that does hard-error is being handed the wrong account. That
//! is the caller's mistake, not the oracle's, and freezing a healthy oracle
//! because somebody passed the wrong key would be a free denial of service.

use anchor_lang::prelude::*;

use crate::errors::AeraError;
use crate::oracle::breaker::{bootstrap_verdict, evaluate, OracleHealth, Verdict};
use crate::oracle::{native_bcook, validation, OracleSourceKind};
use crate::state::{unit_observation, Global, Market, OracleState};

pub fn handle_refresh_oracle(context: Context<RefreshOracle>) -> Result<()> {
    let clock = Clock::get()?;
    let oracle = &mut context.accounts.oracle;
    let kind = oracle.kind()?;

    let observation = match kind {
        // Nothing to read. COOK is worth one COOK by definition, so this is
        // constructed rather than derived and cannot fail.
        OracleSourceKind::UnitOfAccount => unit_observation(clock.slot, clock.unix_timestamp),

        /*
         * A market-priced oracle is not refreshable through here.
         *
         * This path reads one source account and trusts it to be authoritative.
         * A market price is derived from two pools, checked against each other,
         * gated on depth, and appended to a time-weighted history -- none of
         * which this handler does. Routing one through here would produce a
         * reference with no TWAP, no cross-pool check and no spacing rule.
         *
         * `refresh_market_oracle` is the only way to move one.
         */
        OracleSourceKind::MarketTwap => return err!(AeraError::UnknownOracleSource),

        OracleSourceKind::NativeExchangeRate => {
            let source = context
                .remaining_accounts
                .first()
                .ok_or(AeraError::OracleAccountMismatch)?;
            // The source program's ProgramData, so a redeploy can be detected.
            let program_data = context
                .remaining_accounts
                .get(1)
                .ok_or(AeraError::OracleProgramDataMismatch)?;

            /*
             * Identity first, and as a hard error.
             *
             * These are the checks that distinguish "the caller passed the
             * wrong account" from "the source is misbehaving". Only the second
             * should be able to freeze the oracle; letting the first do it
             * would hand anyone a free denial of service against every
             * borrower.
             */
            validation::require_configured_account(
                source,
                &oracle.source_account,
                &oracle.source_program,
            )?;
            validation::require_read_only(source)?;
            validation::require_alive(source)?;

            let bounds = oracle.native_bounds(oracle.mint);

            match native_bcook::observe(
                source,
                program_data,
                &bounds,
                clock.slot,
                clock.unix_timestamp,
            ) {
                Ok(observation) => observation,
                Err(error) => {
                    /*
                     * The account is the right one and we still could not get a
                     * usable rate from it: the source program was redeployed or
                     * changed hands, the data is malformed, the redemption fee
                     * is past the bound Aera accepts, there is no backing, or
                     * the rate is outside the absolute band.
                     *
                     * Record the emergency and succeed, so the freeze persists.
                     * The reference is untouched, which is what keeps
                     * liquidation working on the last trusted price.
                     *
                     * The reason is logged rather than folded into the health,
                     * because every one of those causes needs the same response
                     * and a different investigation. An operator seeing
                     * EMERGENCY needs to know at a glance whether somebody
                     * redeployed the stake pool or whether its accounting broke.
                     */
                    oracle.health = OracleHealth::Emergency as u8;
                    oracle.last_refresh_slot = clock.slot;
                    msg!("aera: oracle source refused, entering EMERGENCY");
                    error.log();
                    return Ok(());
                }
            }
        }
    };

    /*
     * A first observation is judged differently from every later one.
     *
     * The movement breaker compares against a reference, and a brand-new oracle
     * has none -- so accepting the first reading merely because it sits inside
     * the (deliberately very wide) absolute band would let anyone who could
     * arrange the source at configuration time choose Aera's anchor. The
     * bootstrap path checks it against the source's own previous epoch instead,
     * and leaves the oracle unable to open new risk until a later epoch agrees.
     */
    let was_bootstrapping = oracle.is_bootstrapping()?;
    let verdict: Verdict = if !oracle.reference.is_set() && observation.needs_bootstrap() {
        bootstrap_verdict(
            &observation,
            observation.previous_epoch_rate,
            &oracle.breaker,
        )?
    } else {
        evaluate(
            &oracle.reference,
            &observation,
            &oracle.breaker,
            was_bootstrapping,
        )?
    };
    oracle.record(&observation, &verdict, clock.slot);

    if !verdict.accept {
        msg!(
            "aera: rate moved {} bps, refused; oracle now {:?}",
            verdict.moved_bps,
            oracle.health()?
        );
    }

    Ok(())
}

#[derive(Accounts)]
pub struct RefreshOracle<'info> {
    #[account(mut)]
    pub oracle: Box<Account<'info, OracleState>>,
    // The source account, when the configured kind reads one, arrives in
    // `remaining_accounts`. It is not a named account because
    // `UnitOfAccount` has none, and an `Option` here would let a caller omit
    // it for a kind that requires it.
}

/// Clear a breaker freeze by re-anchoring to whatever the chain currently says.
///
/// This is the only administrative action the oracle has, and it is
/// deliberately not "set the price". The admin cannot choose a value: the
/// reference is replaced with the source's present state, whatever that is, and
/// the same absolute bounds still apply. An admin who wants a different number
/// has no instruction that produces one.
///
/// It exists because a frozen oracle otherwise stays frozen forever — the
/// breaker refuses the observation, so the reference never moves toward it, so
/// the next observation is refused for the same reason.
pub fn handle_reset_oracle_breaker(context: Context<ResetOracleBreaker>) -> Result<()> {
    let clock = Clock::get()?;
    let oracle = &mut context.accounts.oracle;
    let kind = oracle.kind()?;

    let observation = match kind {
        OracleSourceKind::UnitOfAccount => unit_observation(clock.slot, clock.unix_timestamp),
        // Not refreshable here — see the note on the first handler.
        OracleSourceKind::MarketTwap => return err!(AeraError::UnknownOracleSource),

        OracleSourceKind::NativeExchangeRate => {
            let source = context
                .remaining_accounts
                .first()
                .ok_or(AeraError::OracleAccountMismatch)?;
            let program_data = context
                .remaining_accounts
                .get(1)
                .ok_or(AeraError::OracleProgramDataMismatch)?;
            validation::require_configured_account(
                source,
                &oracle.source_account,
                &oracle.source_program,
            )?;
            validation::require_read_only(source)?;
            validation::require_alive(source)?;

            let bounds = oracle.native_bounds(oracle.mint);
            /*
             * No recovery here: an operator clearing a freeze must be clearing
             * it to a rate that actually reads, so a bad source is a hard error
             * rather than a silent re-freeze.
             *
             * Note this still enforces the deployment pin. A reset cannot be
             * used to wave through a redeployed source program -- that needs
             * `set_oracle`, which is a deliberate reconfiguration.
             */
            native_bcook::observe(
                source,
                program_data,
                &bounds,
                clock.slot,
                clock.unix_timestamp,
            )?
        }
    };

    // Accept unconditionally -- that is what "reset" means -- but only ever to
    // the value the source reports right now.
    let verdict = Verdict {
        accept: true,
        health: OracleHealth::Healthy,
        moved_bps: 0,
    };
    oracle.record(&observation, &verdict, clock.slot);

    msg!("aera: oracle re-anchored to the current on-chain rate");
    Ok(())
}

#[derive(Accounts)]
pub struct ResetOracleBreaker<'info> {
    #[account(
        constraint = global.admin == admin.key() @ AeraError::NotAdmin,
    )]
    pub global: Box<Account<'info, Global>>,

    #[account(
        mut,
        constraint = oracle.market == market.key() @ AeraError::MarketMismatch,
    )]
    pub oracle: Box<Account<'info, OracleState>>,

    pub market: Box<Account<'info, Market>>,

    pub admin: Signer<'info>,
}
