//! Domain 3 of 4: **Reserve**.
//!
//! One asset's pool. Suppliers deposit `liquidity_mint` and receive share
//! tokens (aCOOK for the COOK reserve); the share-to-liquidity rate rises as
//! borrowers pay interest. Borrowers draw `liquidity_mint` against collateral.
//!
//! Aera Core runs two reserves:
//!   * COOK  — `borrow_enabled = true`,  `collateral_enabled = false`
//!   * bCOOK — `borrow_enabled = false`, `collateral_enabled = true`
//!
//! `collateral_enabled = false` on the COOK reserve is what makes "aCOOK is not
//! accepted as collateral" a program rule rather than a UI convention. Because
//! the bCOOK reserve is never borrowed from, its index stays at 1.0 forever and
//! its share token is 1:1 with bCOOK — so posting collateral behaves exactly
//! like locking bCOOK directly, while still using the fork's vault/share
//! pattern.

use anchor_lang::prelude::*;

use crate::constants::{
    BPS_DENOMINATOR, FIXED_POINT_SCALE, MAX_ADMIN_LIQUIDATION_BONUS_BPS, MAX_ADMIN_LTV_BPS,
    MAX_ADMIN_ORIGINATION_FEE_BPS, MAX_ADMIN_RESERVE_FACTOR_BPS, MIN_ADMIN_COLLATERAL_HAIRCUT_BPS,
    RESERVE_SEED,
};
use crate::errors::AeraError;
use crate::math::{mul_div_floor, Rounding};

/// Signer seeds for a reserve PDA, which is the authority over its liquidity
/// vault and the mint authority of its share token.
pub fn reserve_signer_seeds<'a>(
    market: &'a Pubkey,
    liquidity_mint: &'a Pubkey,
    bump: &'a [u8; 1],
) -> [&'a [u8]; 4] {
    [RESERVE_SEED, market.as_ref(), liquidity_mint.as_ref(), bump]
}

#[account]
#[derive(InitSpace)]
pub struct Reserve {
    pub market: Pubkey,
    pub liquidity_mint: Pubkey,

    /// Program-owned token account holding un-borrowed liquidity. Its authority
    /// is this reserve PDA.
    pub liquidity_vault: Pubkey,

    /// Share-token mint (aCOOK / abCOOK). Mint authority is this reserve PDA.
    pub share_mint: Pubkey,

    /// The [`OracleState`] this reserve prices from.
    ///
    /// Renamed from `price_feed` in v0.2. Same offset, same width, so the
    /// account layout is unchanged and existing reserves need no realloc --
    /// migration repoints this at the new oracle account and closes the old
    /// guardian feed.
    pub oracle: Pubkey,

    pub liquidity_decimals: u8,

    /// Base units sitting in `liquidity_vault`. This is the source of truth for
    /// the pool size, not the vault's token balance, so a raw token donation
    /// cannot move the exchange rate.
    pub available_liquidity: u64,

    /// Outstanding share supply, mirrored here so valuations need only the
    /// reserve account. A holder burning shares directly via the token program
    /// makes the real supply drift below this mirror; that drift only lowers
    /// what the burner could redeem, so the pool never pays out more than it
    /// holds.
    pub share_mint_supply: u64,

    /// Total borrowed principal, scaled by the index at borrow time. Live debt
    /// is `borrowed_principal * borrow_index / FIXED_POINT_SCALE`.
    pub borrowed_principal: u128,

    /// Monotonically increasing borrow index, FIXED_POINT_SCALE-scaled. Starts
    /// at 1.0 and only ever multiplies by factors >= 1. This is the spec's
    /// `variableBorrowIndex`.
    pub borrow_index: u128,

    pub last_update_slot: u64,

    /// Interest owed to `Global::fee_destination`. Carved out of
    /// `total_liquidity` so it never inflates the supplier exchange rate.
    pub accrued_fees: u64,

    pub config: ReserveConfig,

    /// A queued *loosening* config change, applied only after its timelock.
    pub pending: PendingConfig,

    pub bump: u8,
}

/// Risk, interest and cap parameters. All ratios are basis points.
#[derive(InitSpace, Clone, Copy, AnchorSerialize, AnchorDeserialize, Debug, Default, PartialEq)]
pub struct ReserveConfig {
    // --- risk ---
    /// Fraction of (post-haircut) collateral value a borrower may draw.
    pub loan_to_value_bps: u16,
    /// Above this fraction the obligation may be liquidated.
    pub liquidation_threshold_bps: u16,
    /// Extra collateral a liquidator receives, as a fraction of value repaid.
    pub liquidation_bonus_bps: u16,
    /// Fraction of a borrow one liquidation may close while HF >= 0.95.
    pub close_factor_bps: u16,
    /// Discount applied to this asset's collateral value before LTV and LT.
    /// 500 (5%) for bCOOK; 0 for an asset marked at face value.
    pub collateral_haircut_bps: u16,

    // --- interest ---
    /// Utilization at which the borrow rate reaches `optimal_borrow_rate_bps`.
    pub optimal_utilization_bps: u16,
    /// Borrow APR at 0% utilization.
    pub min_borrow_rate_bps: u16,
    /// Borrow APR at the kink.
    pub optimal_borrow_rate_bps: u16,
    /// Borrow APR at 100% utilization.
    pub max_borrow_rate_bps: u16,

    // --- fees ---
    /// Share of interest kept by the protocol, all of it accruing to
    /// `Global::fee_destination`. The rest lifts the supplier exchange rate.
    pub reserve_factor_bps: u16,

    /// One-off fee charged when liquidity is drawn, in basis points of the
    /// amount borrowed. Ships at 0 and is capped at
    /// `MAX_ADMIN_ORIGINATION_FEE_BPS`.
    ///
    /// The borrower owes the full amount and receives the amount minus this
    /// fee, which is retained in the vault and recognised immediately as
    /// protocol revenue. Suppliers' claim on the pool is unchanged by it — the
    /// fee comes from the borrower, not from them.
    pub origination_fee_bps: u16,

    // --- caps ---
    /// Maximum liquidity the reserve will hold. 0 means "no cap".
    pub supply_cap: u64,
    /// Maximum liquidity that may be borrowed out. 0 means "no cap".
    pub borrow_cap: u64,
    /// Maximum a single wallet may supply. 0 means "no cap".
    pub per_wallet_supply_cap: u64,

    // --- switches ---
    /// May liquidity be borrowed from this reserve?
    pub borrow_enabled: bool,
    /// May this reserve's share token be posted as collateral?
    pub collateral_enabled: bool,
    /// Isolated assets may only be paired with other isolated-compatible
    /// collateral. Reserved for a second collateral listing; enforced in
    /// `risk::check_isolation`.
    pub isolated: bool,

    /// Slots in a year: the divisor turning the APR fields into a per-slot
    /// rate. This is the cluster's slot time expressed as a count, so it is
    /// configuration, not a constant — Cookie Chain's slot time can change, and
    /// a stale value here charges borrowers at the wrong wall-clock rate while
    /// every other number still reads correctly.
    pub slots_per_year: u64,
}

/// A queued loosening change. `eta == 0` means "nothing pending".
#[derive(InitSpace, Clone, Copy, AnchorSerialize, AnchorDeserialize, Debug, Default, PartialEq)]
pub struct PendingConfig {
    pub config: ReserveConfig,
    /// Unix timestamp from which `apply_pending_params` may execute.
    pub eta: i64,
}

impl ReserveConfig {
    /// Invariants that hold for every reserve, at init and on every update.
    pub fn validate(&self) -> Result<()> {
        let within_bps = |value: u16| (value as u128) <= BPS_DENOMINATOR;
        require!(
            within_bps(self.loan_to_value_bps)
                && within_bps(self.liquidation_threshold_bps)
                && within_bps(self.liquidation_bonus_bps)
                && within_bps(self.close_factor_bps)
                && within_bps(self.collateral_haircut_bps)
                && within_bps(self.reserve_factor_bps)
                && within_bps(self.optimal_utilization_bps),
            AeraError::InvalidConfig
        );

        // Hard maxima. No admin, timelock or not, can cross these — they are
        // checked here so both `init_reserve` and `apply_pending_params` are
        // covered by one rule.
        require!(
            self.loan_to_value_bps <= MAX_ADMIN_LTV_BPS,
            AeraError::LtvAboveHardMax
        );
        require!(
            self.liquidation_bonus_bps <= MAX_ADMIN_LIQUIDATION_BONUS_BPS,
            AeraError::BonusAboveHardMax
        );
        require!(
            self.reserve_factor_bps <= MAX_ADMIN_RESERVE_FACTOR_BPS,
            AeraError::ReserveFactorAboveHardMax
        );
        require!(
            self.origination_fee_bps <= MAX_ADMIN_ORIGINATION_FEE_BPS,
            AeraError::OriginationFeeAboveHardMax
        );

        /*
         * A hard *minimum*, which is the only one in this list.
         *
         * The haircut is Aera's own risk margin, and it is deliberately not the
         * same thing as the source's redemption fee -- that is applied inside
         * the oracle, before this ever sees a rate, and cannot be configured
         * here at all. This floor exists so the margin on top cannot be tuned
         * to nothing.
         *
         * At zero, Aera would lend against the full redeemable value of an
         * asset whose price it does not control, issued by a program that can
         * be upgraded by a key Aera does not hold. The floor is low enough to
         * leave real room to tune (the launch value is 5%) and high enough that
         * "no margin at all" is not reachable by any admin action.
         *
         * It only binds on a reserve that actually takes collateral; a
         * borrow-only reserve has no haircut to speak of.
         */
        if self.collateral_enabled {
            require!(
                self.collateral_haircut_bps >= MIN_ADMIN_COLLATERAL_HAIRCUT_BPS,
                AeraError::HaircutBelowHardMin
            );
        }

        // A zero close factor would make every liquidation a no-op.
        require!(self.close_factor_bps > 0, AeraError::InvalidConfig);

        // The kink must be strictly inside (0, 100%) so neither slope divides
        // by zero.
        require!(
            self.optimal_utilization_bps > 0
                && (self.optimal_utilization_bps as u128) < BPS_DENOMINATOR,
            AeraError::InvalidConfig
        );

        // You may not be allowed to borrow past the point you'd be liquidated.
        require!(
            self.loan_to_value_bps <= self.liquidation_threshold_bps,
            AeraError::InvalidConfig
        );

        require!(
            self.min_borrow_rate_bps <= self.optimal_borrow_rate_bps
                && self.optimal_borrow_rate_bps <= self.max_borrow_rate_bps,
            AeraError::InvalidConfig
        );

        // Zero would divide by zero converting APR to a per-slot rate.
        require!(self.slots_per_year > 0, AeraError::InvalidConfig);

        // A borrow cap above the supply cap is meaningless (you cannot borrow
        // liquidity that was never allowed in) and hides the real ceiling.
        if self.supply_cap > 0 && self.borrow_cap > self.supply_cap {
            return err!(AeraError::InvalidConfig);
        }
        Ok(())
    }

    /// True when `next` is no looser than `self` on every risk dimension, i.e.
    /// it may be applied immediately with no timelock.
    ///
    /// "Tighter" means: lower LTV, lower liquidation threshold, lower caps,
    /// bigger haircut, and switches turning off rather than on. Interest-rate
    /// and fee changes are treated as loosening (they are not risk-reducing for
    /// users) unless identical, so they always take the timelock.
    #[allow(clippy::nonminimal_bool)]
    pub fn is_tightening_from(&self, next: &ReserveConfig) -> bool {
        next.loan_to_value_bps <= self.loan_to_value_bps
            && next.liquidation_threshold_bps <= self.liquidation_threshold_bps
            && next.collateral_haircut_bps >= self.collateral_haircut_bps
            && cap_is_tighter(self.supply_cap, next.supply_cap)
            && cap_is_tighter(self.borrow_cap, next.borrow_cap)
            && cap_is_tighter(self.per_wallet_supply_cap, next.per_wallet_supply_cap)
            // Written as "not (turning it on when it was off)" rather than
            // clippy's `!next.x || self.x`, because that is the rule: a
            // tightening may not enable something that was disabled. The
            // de Morgan form is equivalent and states nothing.
            && !(next.borrow_enabled && !self.borrow_enabled)
            && !(next.collateral_enabled && !self.collateral_enabled)
            // Untouched elsewhere: rates, fees and the liquidation bonus must be
            // unchanged for the fast path to apply.
            && next.liquidation_bonus_bps == self.liquidation_bonus_bps
            && next.close_factor_bps == self.close_factor_bps
            && next.optimal_utilization_bps == self.optimal_utilization_bps
            && next.min_borrow_rate_bps == self.min_borrow_rate_bps
            && next.optimal_borrow_rate_bps == self.optimal_borrow_rate_bps
            && next.max_borrow_rate_bps == self.max_borrow_rate_bps
            && next.reserve_factor_bps == self.reserve_factor_bps
            // A lower origination fee is a strict improvement for borrowers, so
            // cutting it is allowed to land immediately; raising it waits.
            && next.origination_fee_bps <= self.origination_fee_bps
            && next.isolated == self.isolated
            && next.slots_per_year == self.slots_per_year
    }
}

/// A cap of 0 means "unlimited", so it is the loosest possible value rather
/// than the tightest. Tightening is: going from unlimited to any limit, or
/// lowering an existing limit.
fn cap_is_tighter(current: u64, next: u64) -> bool {
    match (current, next) {
        (0, 0) => true,
        (0, _) => true,  // unlimited -> limited is a tightening
        (_, 0) => false, // limited -> unlimited is a loosening
        (a, b) => b <= a,
    }
}

impl Reserve {
    /// Live total debt owed to the pool, rounded up (protocol-favourable).
    pub fn current_borrowed_amount(&self) -> Result<u64> {
        let amount = mul_div_floor(
            self.borrowed_principal,
            self.borrow_index,
            FIXED_POINT_SCALE,
        )?;
        // Round up without a second multiply: add one unit if anything was
        // truncated.
        let exact = self
            .borrowed_principal
            .checked_mul(self.borrow_index)
            .ok_or(AeraError::MathOverflow)?;
        let amount = if exact % FIXED_POINT_SCALE == 0 {
            amount
        } else {
            amount.checked_add(1).ok_or(AeraError::MathOverflow)?
        };
        u64::try_from(amount).map_err(|_| AeraError::MathOverflow.into())
    }

    /// Available liquidity plus live debt, before fees are removed. Utilization
    /// is about how much of the pool is lent out, independent of who owns the
    /// interest, so it uses this.
    pub fn gross_liquidity(&self) -> Result<u128> {
        (self.available_liquidity as u128)
            .checked_add(self.current_borrowed_amount()? as u128)
            .ok_or(AeraError::MathOverflow.into())
    }

    /// The pool the share token is a claim on: gross liquidity minus the fees
    /// owed to the protocol, which belong to no supplier.
    pub fn total_liquidity(&self) -> Result<u128> {
        /*
         * Unrecoverable debt does not appear here as a separate term, because
         * `absorb_bad_debt` removes it from `borrowed_principal` outright. See
         * that module for why the loss is not tracked in a field of its own:
         * adding one would grow `Reserve`, and a v0.1 reserve that v0.2 cannot
         * deserialise cannot be accrued, which means it cannot be repaid.
         */
        self.gross_liquidity()?
            .checked_sub(self.accrued_fees as u128)
            .ok_or(AeraError::MathOverflow.into())
    }

    /// Borrowed fraction of the pool, in basis points (0..=10_000).
    pub fn utilization_bps(&self) -> Result<u128> {
        let gross = self.gross_liquidity()?;
        if gross == 0 {
            return Ok(0);
        }
        mul_div_floor(
            self.current_borrowed_amount()? as u128,
            BPS_DENOMINATOR,
            gross,
        )
    }

    /// Borrow APR in bps from the kinked curve. Public so the SDK and the app
    /// can reproduce the same number the program charges.
    ///
    /// Below the kink: `min + (optimal - min) * (u / u*)`
    /// Above the kink: `optimal + (max - optimal) * ((u - u*) / (1 - u*))`
    ///
    /// With Aera's defaults that is exactly the spec's curve:
    /// `u <= 0.60 -> 0.02 + 0.08*(u/0.60)`, `u > 0.60 -> 0.10 + 0.80*((u-0.60)/0.40)`.
    pub fn borrow_rate_bps(&self) -> Result<u128> {
        let utilization = self.utilization_bps()?;
        let optimal_utilization = self.config.optimal_utilization_bps as u128;

        if utilization <= optimal_utilization {
            let rate_range = (self.config.optimal_borrow_rate_bps as u128)
                .checked_sub(self.config.min_borrow_rate_bps as u128)
                .ok_or(AeraError::MathOverflow)?;
            let climbed = mul_div_floor(rate_range, utilization, optimal_utilization)?;
            (self.config.min_borrow_rate_bps as u128)
                .checked_add(climbed)
                .ok_or(AeraError::MathOverflow.into())
        } else {
            let rate_range = (self.config.max_borrow_rate_bps as u128)
                .checked_sub(self.config.optimal_borrow_rate_bps as u128)
                .ok_or(AeraError::MathOverflow)?;
            let above = utilization
                .checked_sub(optimal_utilization)
                .ok_or(AeraError::MathOverflow)?;
            let range = BPS_DENOMINATOR
                .checked_sub(optimal_utilization)
                .ok_or(AeraError::MathOverflow)?;
            let climbed = mul_div_floor(rate_range, above, range)?;
            (self.config.optimal_borrow_rate_bps as u128)
                .checked_add(climbed)
                .ok_or(AeraError::MathOverflow.into())
        }
    }

    /// Supply APR in bps: `r_borrow * u * (1 - reserve_factor)`. Not used by the
    /// program (suppliers earn through the exchange rate, not a stored rate) but
    /// kept here so the on-chain definition is the authoritative one.
    pub fn supply_rate_bps(&self) -> Result<u128> {
        let borrow_rate = self.borrow_rate_bps()?;
        let utilization = self.utilization_bps()?;
        let after_utilization = mul_div_floor(borrow_rate, utilization, BPS_DENOMINATOR)?;
        let keep = BPS_DENOMINATOR
            .checked_sub(self.config.reserve_factor_bps as u128)
            .ok_or(AeraError::MathOverflow)?;
        mul_div_floor(after_utilization, keep, BPS_DENOMINATOR)
    }

    /// Per-slot borrow rate, FIXED_POINT_SCALE-scaled.
    pub fn borrow_rate_per_slot(&self) -> Result<u128> {
        let apr_bps = self.borrow_rate_bps()?;
        let denominator = BPS_DENOMINATOR
            .checked_mul(self.config.slots_per_year as u128)
            .ok_or(AeraError::MathOverflow)?;
        mul_div_floor(apr_bps, FIXED_POINT_SCALE, denominator)
    }

    /// Advance the borrow index for the slots elapsed since the last accrual and
    /// split the protocol's cut between the two fee destinations.
    ///
    /// `new_index = old_index * (1 + rate_per_slot * elapsed)` — one multiply per
    /// accrual, compounding across accruals.
    pub fn accrue_interest(&mut self, current_slot: u64) -> Result<()> {
        let elapsed = current_slot
            .checked_sub(self.last_update_slot)
            .ok_or(AeraError::MathOverflow)?;

        if elapsed > 0 && self.borrowed_principal > 0 {
            let borrowed_before = self.current_borrowed_amount()?;
            let rate_per_slot = self.borrow_rate_per_slot()?;
            let accrued = rate_per_slot
                .checked_mul(elapsed as u128)
                .ok_or(AeraError::MathOverflow)?;
            let growth = FIXED_POINT_SCALE
                .checked_add(accrued)
                .ok_or(AeraError::MathOverflow)?;
            self.borrow_index = mul_div_floor(self.borrow_index, growth, FIXED_POINT_SCALE)?;

            // Borrowers owe the full interest; the protocol keeps
            // `reserve_factor_bps` of it and the remainder lifts the supplier
            // exchange rate. The cut floors, in the suppliers' favour.
            let interest = self
                .current_borrowed_amount()?
                .saturating_sub(borrowed_before) as u128;

            let fee = mul_div_floor(
                interest,
                self.config.reserve_factor_bps as u128,
                BPS_DENOMINATOR,
            )?;

            self.accrued_fees = self
                .accrued_fees
                .checked_add(u64::try_from(fee).map_err(|_| AeraError::MathOverflow)?)
                .ok_or(AeraError::MathOverflow)?;
        }

        self.last_update_slot = current_slot;
        Ok(())
    }

    /// Reject use of a reserve whose interest has not been accrued this slot.
    pub fn require_accrued(&self) -> Result<()> {
        require_eq!(
            self.last_update_slot,
            Clock::get()?.slot,
            AeraError::ReserveStale
        );
        Ok(())
    }

    /// Liquidity a given share amount is currently worth.
    pub fn shares_to_liquidity(&self, shares: u64, rounding: Rounding) -> Result<u64> {
        let amount = crate::math::mul_div(
            shares as u128,
            self.total_liquidity()?,
            (self.share_mint_supply as u128).max(1),
            rounding,
        )?;
        u64::try_from(amount).map_err(|_| AeraError::MathOverflow.into())
    }

    /// Shares a given liquidity amount currently buys.
    pub fn liquidity_to_shares(&self, liquidity: u64, rounding: Rounding) -> Result<u64> {
        if self.share_mint_supply == 0 {
            return Ok(liquidity);
        }
        let amount = crate::math::mul_div(
            liquidity as u128,
            self.share_mint_supply as u128,
            self.total_liquidity()?.max(1),
            rounding,
        )?;
        u64::try_from(amount).map_err(|_| AeraError::MathOverflow.into())
    }
}
