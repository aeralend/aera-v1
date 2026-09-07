//! The native bCOOK/COOK exchange-rate source.
//!
//! Reads BakeYourStake's own stake-pool account and derives what one bCOOK is
//! actually redeemable for. Nothing here trusts a caller, an admin, or a market
//! quote: the only inputs are an account the reserve's configuration names in
//! advance, and the clock.
//!
//! The formula and the evidence for it are in `docs/ORACLE_V0_2.md`. In short:
//!
//! ```text
//!   gross     = total_lamports / pool_token_supply
//!   effective = gross x (1 - max(stake_withdrawal_fee, sol_withdrawal_fee))
//! ```
//!
//! ## Why the fee is read rather than configured
//!
//! The pool charges 2% to redeem. A holder of bCOOK therefore cannot obtain
//! `gross` for it -- and neither can a liquidator who seizes it. Valuing
//! collateral at `gross` would credit borrowers with value nobody can realize,
//! and would make liquidation unprofitable exactly when it matters.
//!
//! The fee is read live on every observation, because it is the pool
//! operator's to change and Aera must not be surprised by it. It is bounded:
//! past `max_withdrawal_fee_bps` the observation is refused, which freezes new
//! borrowing while leaving repayment open. That bound is what stops the
//! operator consuming Aera's risk margin by raising their own fee.
//!
//! ## Why the layout is walked, not indexed
//!
//! Everything up to `epoch_fee` sits at a fixed offset and is read directly.
//! Past that the struct contains `FutureEpoch<Fee>` and `Option<Pubkey>`
//! fields whose widths depend on their own tags, so the withdrawal fees cannot
//! be reached by a constant. They are reached by walking those tags, which is
//! deterministic; guessing an offset is not. Any tag outside its valid range
//! aborts the read rather than being skipped.

use anchor_lang::prelude::*;

use crate::constants::{BPS_DENOMINATOR, FIXED_POINT_SCALE};
use crate::errors::AeraError;
use crate::math::{mul_div_ceil, mul_div_floor};

use super::{apply_withdrawal_fee, deployment, OracleSourceKind, PriceObservation};

// ---------------------------------------------------------------------------
// Layout
// ---------------------------------------------------------------------------

/// `spl_stake_pool::state::AccountType::StakePool`.
const ACCOUNT_TYPE_STAKE_POOL: u8 = 1;

/// Serialized length of the `StakePool` account. Checked exactly, not as a
/// minimum: an account of a different size is a different type, and reading a
/// longer one positionally is how a "type cosplay" attack succeeds.
pub const STAKE_POOL_LEN: usize = 611;

// Fixed prefix offsets, in declaration order. Written as a running sum so the
// derivation is visible rather than a table of magic numbers.
const OFF_ACCOUNT_TYPE: usize = 0;
const OFF_MANAGER: usize = OFF_ACCOUNT_TYPE + 1;
const OFF_STAKER: usize = OFF_MANAGER + 32;
const OFF_STAKE_DEPOSIT_AUTHORITY: usize = OFF_STAKER + 32;
const OFF_WITHDRAW_BUMP: usize = OFF_STAKE_DEPOSIT_AUTHORITY + 32;
const OFF_VALIDATOR_LIST: usize = OFF_WITHDRAW_BUMP + 1;
const OFF_RESERVE_STAKE: usize = OFF_VALIDATOR_LIST + 32;
const OFF_POOL_MINT: usize = OFF_RESERVE_STAKE + 32; // 162
const OFF_MANAGER_FEE_ACCOUNT: usize = OFF_POOL_MINT + 32;
const OFF_TOKEN_PROGRAM: usize = OFF_MANAGER_FEE_ACCOUNT + 32;
const OFF_TOTAL_LAMPORTS: usize = OFF_TOKEN_PROGRAM + 32; // 258
const OFF_POOL_TOKEN_SUPPLY: usize = OFF_TOTAL_LAMPORTS + 8; // 266
const OFF_LAST_UPDATE_EPOCH: usize = OFF_POOL_TOKEN_SUPPLY + 8; // 274
const OFF_LOCKUP: usize = OFF_LAST_UPDATE_EPOCH + 8;
const OFF_EPOCH_FEE: usize = OFF_LOCKUP + 48; // 330
/// Where the variable-width tail begins.
const OFF_VARIABLE: usize = OFF_EPOCH_FEE + FEE_LEN; // 346

/// `Fee { denominator: u64, numerator: u64 }` -- denominator first.
const FEE_LEN: usize = 16;

// ---------------------------------------------------------------------------
// Reading primitives, all bounds-checked
// ---------------------------------------------------------------------------

fn read_u8(data: &[u8], at: usize) -> Result<u8> {
    data.get(at)
        .copied()
        .ok_or(AeraError::OracleAccountMalformed.into())
}

fn read_u64(data: &[u8], at: usize) -> Result<u64> {
    let bytes = data
        .get(at..at + 8)
        .ok_or(AeraError::OracleAccountMalformed)?;
    let array: [u8; 8] = bytes
        .try_into()
        .map_err(|_| AeraError::OracleAccountMalformed)?;
    Ok(u64::from_le_bytes(array))
}

fn read_pubkey(data: &[u8], at: usize) -> Result<Pubkey> {
    let bytes = data
        .get(at..at + 32)
        .ok_or(AeraError::OracleAccountMalformed)?;
    let array: [u8; 32] = bytes
        .try_into()
        .map_err(|_| AeraError::OracleAccountMalformed)?;
    Ok(Pubkey::from(array))
}

/// A `Fee`, converted to basis points and rounded **up**.
///
/// Rounding up overstates the fee, which understates the effective rate, which
/// understates collateral. That is the safe direction: never credit a borrower
/// with value a redeemer could not obtain.
fn read_fee_bps(data: &[u8], at: usize) -> Result<u16> {
    let denominator = read_u64(data, at)? as u128;
    let numerator = read_u64(data, at + 8)? as u128;

    // A zero denominator is how spl-stake-pool encodes "no fee".
    if denominator == 0 {
        return Ok(0);
    }
    require!(numerator <= denominator, AeraError::OracleAccountMalformed);

    let bps = mul_div_ceil(numerator, BPS_DENOMINATOR, denominator)?;
    u16::try_from(bps).map_err(|_| AeraError::OracleAccountMalformed.into())
}

/// Walk one variable-width field, returning the offset just past it.
///
/// `widths` maps each valid tag to the payload width that follows it. A tag
/// outside the table is a malformed account, not something to skip.
fn skip_tagged(data: &[u8], at: usize, widths: &[usize]) -> Result<usize> {
    let tag = read_u8(data, at)? as usize;
    let width = widths
        .get(tag)
        .copied()
        .ok_or(AeraError::OracleAccountMalformed)?;
    at.checked_add(1)
        .and_then(|next| next.checked_add(width))
        .ok_or(AeraError::OracleAccountMalformed.into())
}

/// `FutureEpoch<T>`: None | One(T) | Two(T).
fn skip_future_fee(data: &[u8], at: usize) -> Result<usize> {
    skip_tagged(data, at, &[0, FEE_LEN, FEE_LEN])
}

/// `Option<Pubkey>`: None | Some(Pubkey).
fn skip_option_pubkey(data: &[u8], at: usize) -> Result<usize> {
    skip_tagged(data, at, &[0, 32])
}

// ---------------------------------------------------------------------------
// The parsed view
// ---------------------------------------------------------------------------

/// Everything Aera reads out of a stake pool. Nothing else is parsed.
#[derive(Clone, Copy, Debug)]
pub struct StakePoolView {
    pub pool_mint: Pubkey,
    pub token_program: Pubkey,
    pub total_lamports: u64,
    pub pool_token_supply: u64,
    pub last_update_epoch: u64,
    pub stake_withdrawal_fee_bps: u16,
    pub sol_withdrawal_fee_bps: u16,

    /// The pool's own figures from the previous epoch.
    ///
    /// These sit at the very end of the struct and are the reason the layout
    /// walk goes all the way through the variable-width tail rather than
    /// stopping at the withdrawal fee. They are one epoch of history that the
    /// pool publishes about itself, which is exactly what a first observation
    /// otherwise lacks: something unforgeable to compare against.
    pub last_epoch_total_lamports: u64,
    pub last_epoch_pool_token_supply: u64,
}

impl StakePoolView {
    /// The fee a redeemer actually faces.
    ///
    /// The larger of the two withdrawal paths. `WithdrawStake` and
    /// `WithdrawSol` are priced separately and either may be the one available
    /// to a liquidator, so the conservative reading is the worse of them. They
    /// are equal (2%) on the deployed pool today; taking the maximum means that
    /// staying true is not something Aera depends on.
    pub fn withdrawal_fee_bps(&self) -> u16 {
        self.stake_withdrawal_fee_bps
            .max(self.sol_withdrawal_fee_bps)
    }
}

/// Parse a stake-pool account. Structure only -- no policy, no bounds on the
/// resulting rate. The caller applies those.
pub fn parse_stake_pool(data: &[u8]) -> Result<StakePoolView> {
    // Exact, not a minimum. See STAKE_POOL_LEN.
    require!(
        data.len() == STAKE_POOL_LEN,
        AeraError::OracleAccountMalformed
    );
    require!(
        read_u8(data, OFF_ACCOUNT_TYPE)? == ACCOUNT_TYPE_STAKE_POOL,
        AeraError::OracleAccountMalformed
    );

    let pool_mint = read_pubkey(data, OFF_POOL_MINT)?;
    let token_program = read_pubkey(data, OFF_TOKEN_PROGRAM)?;
    let total_lamports = read_u64(data, OFF_TOTAL_LAMPORTS)?;
    let pool_token_supply = read_u64(data, OFF_POOL_TOKEN_SUPPLY)?;
    let last_update_epoch = read_u64(data, OFF_LAST_UPDATE_EPOCH)?;

    // ---- the variable-width tail ----
    let mut at = OFF_VARIABLE;
    at = skip_future_fee(data, at)?; // next_epoch_fee
    at = skip_option_pubkey(data, at)?; // preferred_deposit_validator_vote_address
    at = skip_option_pubkey(data, at)?; // preferred_withdraw_validator_vote_address
    at = at
        .checked_add(FEE_LEN)
        .ok_or(AeraError::OracleAccountMalformed)?; // stake_deposit_fee

    let stake_withdrawal_fee_bps = read_fee_bps(data, at)?;
    at = at
        .checked_add(FEE_LEN)
        .ok_or(AeraError::OracleAccountMalformed)?;

    at = skip_future_fee(data, at)?; // next_stake_withdrawal_fee
    at = at.checked_add(1).ok_or(AeraError::OracleAccountMalformed)?; // stake_referral_fee: u8
    at = skip_option_pubkey(data, at)?; // sol_deposit_authority
    at = at
        .checked_add(FEE_LEN)
        .ok_or(AeraError::OracleAccountMalformed)?; // sol_deposit_fee
    at = at.checked_add(1).ok_or(AeraError::OracleAccountMalformed)?; // sol_referral_fee: u8
    at = skip_option_pubkey(data, at)?; // sol_withdraw_authority

    let sol_withdrawal_fee_bps = read_fee_bps(data, at)?;
    at = at
        .checked_add(FEE_LEN)
        .ok_or(AeraError::OracleAccountMalformed)?;
    at = skip_future_fee(data, at)?; // next_sol_withdrawal_fee

    // The walk lands exactly here on a well-formed account.
    let last_epoch_pool_token_supply = read_u64(data, at)?;
    let last_epoch_total_lamports = read_u64(
        data,
        at.checked_add(8).ok_or(AeraError::OracleAccountMalformed)?,
    )?;

    Ok(StakePoolView {
        pool_mint,
        token_program,
        total_lamports,
        pool_token_supply,
        last_update_epoch,
        stake_withdrawal_fee_bps,
        sol_withdrawal_fee_bps,
        last_epoch_total_lamports,
        last_epoch_pool_token_supply,
    })
}

/// The rate the pool itself reports for the previous epoch, if it has one.
///
/// `None` before the pool has completed an epoch, which is the only case where
/// a bootstrap genuinely has nothing to check itself against.
pub fn previous_epoch_rate(view: &StakePoolView) -> Result<Option<u128>> {
    if view.last_epoch_pool_token_supply == 0 || view.last_epoch_total_lamports == 0 {
        return Ok(None);
    }
    Ok(Some(mul_div_floor(
        view.last_epoch_total_lamports as u128,
        FIXED_POINT_SCALE,
        view.last_epoch_pool_token_supply as u128,
    )?))
}

/// The gross exchange rate, FIXED_POINT_SCALE-scaled.
///
/// Floors: collateral value benefits the borrower, and the convention in
/// `math.rs` is that quantities favourable to the user round down.
pub fn gross_rate(view: &StakePoolView) -> Result<u128> {
    require!(view.pool_token_supply > 0, AeraError::OracleZeroSupply);
    require!(view.total_lamports > 0, AeraError::OracleZeroBacking);

    mul_div_floor(
        view.total_lamports as u128,
        FIXED_POINT_SCALE,
        view.pool_token_supply as u128,
    )
}

/// Bounds a caller must supply, from the reserve's oracle configuration.
///
/// Passed in rather than read from constants so a second market could be
/// configured differently without a program upgrade, and so the tests can drive
/// the boundaries directly.
#[derive(Clone, Copy, Debug)]
pub struct NativeOracleBounds {
    /// The stake-pool program that must own the account.
    pub expected_program: Pubkey,
    /// The exact pool account the reserve is configured against.
    pub expected_pool: Pubkey,
    /// The mint the pool must issue, i.e. the collateral asset.
    pub expected_pool_mint: Pubkey,
    /// Past this, the observation is refused. Stops the pool operator
    /// consuming Aera's risk margin by raising their own fee.
    pub max_withdrawal_fee_bps: u16,
    /// A staking receipt cannot be worth less than the asset it wraps.
    pub rate_floor: u128,
    /// Guards a redefined or corrupted field.
    pub rate_ceiling: u128,

    /// The `ProgramData` deploy slot this reserve was configured against.
    ///
    /// Owning-program validation proves who owns the account's bytes. It does
    /// not prove what the code behind that id does, because the code can be
    /// replaced -- and on Cookie Chain it can be replaced by a single wallet
    /// key. Pinning the deploy slot is what turns "the right program owns it"
    /// into "the right *deployment* owns it".
    pub expected_deploy_slot: u64,
    /// The upgrade authority at configuration time, or
    /// [`crate::oracle::deployment::NO_UPGRADE_AUTHORITY`] if the source
    /// program was already immutable.
    pub expected_upgrade_authority: Pubkey,
}

/// Read, validate and normalize one observation from a stake-pool account.
///
/// This is the whole of the native source. It performs every check in
/// `docs/ORACLE_V0_2.md` section 8 except the circuit breaker, which needs
/// stored state and lives in `breaker.rs`.
pub fn observe(
    account: &AccountInfo,
    program_data: &AccountInfo,
    bounds: &NativeOracleBounds,
    slot: u64,
    unix_timestamp: i64,
) -> Result<PriceObservation> {
    /*
     * The deployment, before anything else.
     *
     * If the program behind this account has been redeployed, nothing parsed
     * out of the account below can be trusted to mean what it meant when the
     * reserve was configured -- the layout can be identical and the semantics
     * different. Checking it first means a redeploy is refused before a single
     * economic field is read.
     */
    let observed = deployment::read_deployment(program_data, &bounds.expected_program)?;
    deployment::verify_matches_pin(
        &observed,
        bounds.expected_deploy_slot,
        bounds.expected_upgrade_authority,
    )?;

    // 1. The account is the one configuration names. Checked before anything is
    //    read, so a substituted account cannot even be parsed.
    require_keys_eq!(
        *account.key,
        bounds.expected_pool,
        AeraError::OracleAccountMismatch
    );

    // 2. Owned by the stake-pool program. An account with the right address but
    //    the wrong owner is not the pool.
    require_keys_eq!(
        *account.owner,
        bounds.expected_program,
        AeraError::OracleOwnerMismatch
    );

    let data = account.try_borrow_data()?;
    let view = parse_stake_pool(&data)?;

    // 3. The pool issues the asset Aera is pricing. Without this, a different
    //    pool of the right shape would price bCOOK off unrelated backing.
    require_keys_eq!(
        view.pool_mint,
        bounds.expected_pool_mint,
        AeraError::OracleMintMismatch
    );

    // 4. The fee is within the bound Aera agreed to tolerate.
    let withdrawal_fee_bps = view.withdrawal_fee_bps();
    require!(
        withdrawal_fee_bps <= bounds.max_withdrawal_fee_bps,
        AeraError::OracleWithdrawalFeeTooHigh
    );

    // 5. The rate itself.
    let gross = gross_rate(&view)?;
    let effective = apply_withdrawal_fee(gross, withdrawal_fee_bps)?;

    // 6. Absolute sanity. These are not the circuit breaker -- they are the
    //    bounds outside which the number cannot be a bCOOK/COOK rate at all.
    require!(gross >= bounds.rate_floor, AeraError::OracleRateBelowFloor);
    require!(
        gross <= bounds.rate_ceiling,
        AeraError::OracleRateAboveCeiling
    );
    require!(effective > 0, AeraError::InvalidOraclePrice);

    Ok(PriceObservation {
        gross_rate: gross,
        withdrawal_fee_bps,
        effective_rate: effective,
        source_epoch: view.last_update_epoch,
        slot,
        unix_timestamp,
        source: OracleSourceKind::NativeExchangeRate,
        previous_epoch_rate: previous_epoch_rate(&view)?,
    })
}

// ---------------------------------------------------------------------------
// Tests over the parser, against bytes shaped like the real account
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a stake-pool account body matching the deployed pool's shape.
    ///
    /// Every variable-width tag is `None`/`One` explicitly so the walk is
    /// exercised rather than assumed, and the total is padded to the exact
    /// 611 bytes the real account occupies.
    fn pool_bytes(
        total_lamports: u64,
        pool_token_supply: u64,
        stake_withdrawal_bps: u16,
        sol_withdrawal_bps: u16,
    ) -> Vec<u8> {
        let mut data = vec![0u8; STAKE_POOL_LEN];
        data[OFF_ACCOUNT_TYPE] = ACCOUNT_TYPE_STAKE_POOL;
        data[OFF_TOTAL_LAMPORTS..OFF_TOTAL_LAMPORTS + 8]
            .copy_from_slice(&total_lamports.to_le_bytes());
        data[OFF_POOL_TOKEN_SUPPLY..OFF_POOL_TOKEN_SUPPLY + 8]
            .copy_from_slice(&pool_token_supply.to_le_bytes());
        data[OFF_LAST_UPDATE_EPOCH..OFF_LAST_UPDATE_EPOCH + 8]
            .copy_from_slice(&51u64.to_le_bytes());

        let write_fee = (|data: &mut Vec<u8>, at: usize, bps: u16| {
            data[at..at + 8].copy_from_slice(&10_000u64.to_le_bytes()); // denominator
            data[at + 8..at + 16].copy_from_slice(&(bps as u64).to_le_bytes()); // numerator
        }) as fn(&mut Vec<u8>, usize, u16);

        write_fee(&mut data, OFF_EPOCH_FEE, 1);

        let mut at = OFF_VARIABLE;
        data[at] = 0; // next_epoch_fee: None
        at += 1;
        data[at] = 0; // preferred_deposit_validator: None
        at += 1;
        data[at] = 0; // preferred_withdraw_validator: None
        at += 1;
        write_fee(&mut data, at, 50); // stake_deposit_fee 0.5%
        at += FEE_LEN;
        write_fee(&mut data, at, stake_withdrawal_bps);
        at += FEE_LEN;
        data[at] = 0; // next_stake_withdrawal_fee: None
        at += 1;
        data[at] = 0; // stake_referral_fee
        at += 1;
        data[at] = 0; // sol_deposit_authority: None
        at += 1;
        write_fee(&mut data, at, 50); // sol_deposit_fee
        at += FEE_LEN;
        data[at] = 0; // sol_referral_fee
        at += 1;
        data[at] = 0; // sol_withdraw_authority: None
        at += 1;
        write_fee(&mut data, at, sol_withdrawal_bps);

        data
    }

    /// The figures actually observed on Cookie Chain at epoch 51.
    const LIVE_LAMPORTS: u64 = 126_736_122_176_510_689;
    const LIVE_SUPPLY: u64 = 97_450_941_583_896_663;

    #[test]
    fn parses_the_live_pool_shape() {
        let data = pool_bytes(LIVE_LAMPORTS, LIVE_SUPPLY, 200, 200);
        let view = parse_stake_pool(&data).unwrap();

        assert_eq!(view.total_lamports, LIVE_LAMPORTS);
        assert_eq!(view.pool_token_supply, LIVE_SUPPLY);
        assert_eq!(view.last_update_epoch, 51);
        assert_eq!(
            view.stake_withdrawal_fee_bps, 200,
            "2% stake withdrawal fee"
        );
        assert_eq!(view.sol_withdrawal_fee_bps, 200, "2% sol withdrawal fee");
    }

    #[test]
    fn reproduces_the_rate_measured_on_chain() {
        let data = pool_bytes(LIVE_LAMPORTS, LIVE_SUPPLY, 200, 200);
        let view = parse_stake_pool(&data).unwrap();
        let gross = gross_rate(&view).unwrap();

        // scripts/discover-bcook-oracle.ts read exactly this from the chain.
        assert_eq!(gross, 1_300_512_033_199_823_618);
    }

    #[test]
    fn the_worse_of_the_two_withdrawal_paths_is_used() {
        let data = pool_bytes(LIVE_LAMPORTS, LIVE_SUPPLY, 200, 900);
        let view = parse_stake_pool(&data).unwrap();
        assert_eq!(view.withdrawal_fee_bps(), 900, "must take the worse path");

        let data = pool_bytes(LIVE_LAMPORTS, LIVE_SUPPLY, 900, 200);
        let view = parse_stake_pool(&data).unwrap();
        assert_eq!(view.withdrawal_fee_bps(), 900);
    }

    #[test]
    fn a_wrong_length_account_is_refused() {
        let mut data = pool_bytes(LIVE_LAMPORTS, LIVE_SUPPLY, 200, 200);
        data.push(0);
        assert!(parse_stake_pool(&data).is_err(), "longer must be refused");

        data.truncate(STAKE_POOL_LEN - 1);
        assert!(parse_stake_pool(&data).is_err(), "shorter must be refused");
    }

    #[test]
    fn a_wrong_account_type_is_refused() {
        let mut data = pool_bytes(LIVE_LAMPORTS, LIVE_SUPPLY, 200, 200);
        data[OFF_ACCOUNT_TYPE] = 2; // ValidatorList
        assert!(parse_stake_pool(&data).is_err());
    }

    #[test]
    fn a_corrupt_variable_tag_aborts_rather_than_skipping() {
        let mut data = pool_bytes(LIVE_LAMPORTS, LIVE_SUPPLY, 200, 200);
        data[OFF_VARIABLE] = 7; // not a FutureEpoch variant
        assert!(
            parse_stake_pool(&data).is_err(),
            "an unknown tag must not be treated as zero-width"
        );
    }

    #[test]
    fn zero_supply_and_zero_backing_are_refused() {
        let view = parse_stake_pool(&pool_bytes(LIVE_LAMPORTS, 0, 200, 200)).unwrap();
        assert!(gross_rate(&view).is_err(), "no shares means no rate");

        let view = parse_stake_pool(&pool_bytes(0, LIVE_SUPPLY, 200, 200)).unwrap();
        assert!(gross_rate(&view).is_err(), "no backing means no rate");
    }

    #[test]
    fn a_fee_numerator_above_its_denominator_is_malformed() {
        let mut data = pool_bytes(LIVE_LAMPORTS, LIVE_SUPPLY, 200, 200);
        // Walk to stake_withdrawal_fee the same way the parser does.
        let at = OFF_VARIABLE + 3 + FEE_LEN;
        data[at..at + 8].copy_from_slice(&10u64.to_le_bytes()); // denominator 10
        data[at + 8..at + 16].copy_from_slice(&11u64.to_le_bytes()); // numerator 11
        assert!(
            parse_stake_pool(&data).is_err(),
            "a fee above 100% is nonsense"
        );
    }

    #[test]
    fn the_rate_does_not_overflow_at_absurd_magnitudes() {
        let view = parse_stake_pool(&pool_bytes(u64::MAX, 1, 200, 200)).unwrap();
        // u64::MAX * 1e18 fits in u128, so this must succeed rather than wrap.
        let gross = gross_rate(&view).unwrap();
        assert_eq!(gross, (u64::MAX as u128) * FIXED_POINT_SCALE);
    }

    #[test]
    fn a_fee_rounds_up_so_collateral_is_never_overvalued() {
        // 1 / 3 of a percent: 33.33 bps must become 34, not 33.
        let mut data = pool_bytes(LIVE_LAMPORTS, LIVE_SUPPLY, 200, 200);
        let at = OFF_VARIABLE + 3 + FEE_LEN;
        data[at..at + 8].copy_from_slice(&3u64.to_le_bytes()); // denominator 3
        data[at + 8..at + 16].copy_from_slice(&1u64.to_le_bytes()); // numerator 1
        let view = parse_stake_pool(&data).unwrap();
        assert_eq!(view.stake_withdrawal_fee_bps, 3_334, "1/3 rounds up");
    }
}
