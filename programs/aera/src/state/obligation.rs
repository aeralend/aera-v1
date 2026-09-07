//! Domain 4 of 4: **Obligation**.
//!
//! A borrower's position in one market: the share-token collateral posted and
//! the liquidity borrowed, plus the cached valuations `refresh_obligation`
//! recomputes.
//!
//! `SupplyPosition` also lives here: it is the other per-wallet account, and it
//! exists only so the per-wallet supply cap is a program rule rather than a
//! front-end suggestion.

use anchor_lang::prelude::*;

use crate::constants::{FIXED_POINT_SCALE, MAX_OBLIGATION_RESERVES, OBLIGATION_SEED};
use crate::errors::AeraError;
use crate::math::mul_div_floor;

/// Signer seeds for an obligation PDA, authority over its collateral vaults.
pub fn obligation_signer_seeds<'a>(
    market: &'a Pubkey,
    owner: &'a Pubkey,
    bump: &'a [u8; 1],
) -> [&'a [u8]; 4] {
    [OBLIGATION_SEED, market.as_ref(), owner.as_ref(), bump]
}

#[account]
#[derive(InitSpace)]
pub struct Obligation {
    pub market: Pubkey,
    pub owner: Pubkey,

    pub last_update_slot: u64,

    /// Set whenever deposits/borrows change; cleared by `refresh_obligation`.
    /// Health-dependent handlers reject a stale obligation so they never act on
    /// cached values a prior instruction in the same transaction invalidated.
    pub stale: bool,

    /// True when any feed this obligation depends on had its circuit breaker
    /// latched at the last refresh — the collateral's feed as much as the
    /// debt's. `borrow` refuses while this is set.
    ///
    /// The flag lives here rather than being read straight from a feed because
    /// `borrow` only carries the *borrowed* asset's feed in its accounts; the
    /// collateral's feed is seen by `refresh_obligation` alone, and a 25% move
    /// in the collateral is precisely the case worth stopping.
    pub prices_stressed: bool,

    /// Σ every deposit's market value at face, FIXED_POINT_SCALE-scaled.
    /// This is what the UI shows as "collateral value"; it is *not* what backs
    /// the borrow limit — see `effective_collateral_value`.
    pub deposited_value: u128,

    /// Σ every deposit's value *after* its haircut. This is the spec's
    /// `collateral_value`, and the basis for both limits below.
    pub effective_collateral_value: u128,

    /// Σ every borrow's market value, FIXED_POINT_SCALE-scaled.
    pub borrowed_value: u128,

    /// Σ (effective deposit value * reserve LTV). Borrows may not exceed this.
    pub allowed_borrow_value: u128,

    /// Σ (effective deposit value * reserve liquidation threshold). Above this
    /// the obligation is liquidatable.
    pub unhealthy_borrow_value: u128,

    #[max_len(MAX_OBLIGATION_RESERVES)]
    pub deposits: Vec<ObligationCollateral>,

    #[max_len(MAX_OBLIGATION_RESERVES)]
    pub borrows: Vec<ObligationLiquidity>,

    pub bump: u8,
}

#[derive(InitSpace, Clone, Copy, AnchorSerialize, AnchorDeserialize, Debug, Default)]
pub struct ObligationCollateral {
    pub reserve: Pubkey,
    pub deposited_shares: u64,
    /// Face value, before haircut.
    pub market_value: u128,
}

#[derive(InitSpace, Clone, Copy, AnchorSerialize, AnchorDeserialize, Debug, Default)]
pub struct ObligationLiquidity {
    pub reserve: Pubkey,
    /// Principal scaled by the reserve's index at borrow time, so live debt
    /// grows automatically as that index advances.
    pub borrowed_principal: u128,
    pub market_value: u128,
}

impl Obligation {
    /// Reject a health-dependent action when the obligation has not been
    /// refreshed in this same transaction.
    pub fn require_refreshed(&self) -> Result<()> {
        require!(!self.stale, AeraError::ObligationStale);
        require_eq!(
            self.last_update_slot,
            Clock::get()?.slot,
            AeraError::ObligationStale
        );
        Ok(())
    }

    /// Health factor in basis points: `unhealthy_borrow_value / borrowed_value`.
    ///
    /// This is the spec's `HF = Σ(collateral_value * LT) / Σ(debt)`. A position
    /// with no debt has no meaningful ratio; `None` is returned so callers
    /// render "∞" rather than a fabricated number.
    pub fn health_factor_bps(&self) -> Result<Option<u128>> {
        if self.borrowed_value == 0 {
            return Ok(None);
        }
        Ok(Some(mul_div_floor(
            self.unhealthy_borrow_value,
            crate::constants::BPS_DENOMINATOR,
            self.borrowed_value,
        )?))
    }

    /// True when the position may be liquidated: debt has passed the
    /// liquidation line. Equivalent to HF < 1.
    pub fn is_liquidatable(&self) -> bool {
        self.borrowed_value > self.unhealthy_borrow_value
    }

    /// Index of the collateral entry for `reserve`, creating an empty one if
    /// there is room.
    pub fn upsert_collateral(&mut self, reserve: Pubkey) -> Result<usize> {
        if let Some(index) = self.deposits.iter().position(|e| e.reserve == reserve) {
            return Ok(index);
        }
        require!(
            self.deposits.len() < MAX_OBLIGATION_RESERVES,
            AeraError::TooManyReserves
        );
        self.deposits.push(ObligationCollateral {
            reserve,
            deposited_shares: 0,
            market_value: 0,
        });
        Ok(self.deposits.len() - 1)
    }

    /// Index of the borrow entry for `reserve`, creating an empty one if there
    /// is room.
    pub fn upsert_borrow(&mut self, reserve: Pubkey) -> Result<usize> {
        if let Some(index) = self.borrows.iter().position(|e| e.reserve == reserve) {
            return Ok(index);
        }
        require!(
            self.borrows.len() < MAX_OBLIGATION_RESERVES,
            AeraError::TooManyReserves
        );
        self.borrows.push(ObligationLiquidity {
            reserve,
            borrowed_principal: 0,
            market_value: 0,
        });
        Ok(self.borrows.len() - 1)
    }

    pub fn find_collateral(&self, reserve: Pubkey) -> Result<usize> {
        self.deposits
            .iter()
            .position(|e| e.reserve == reserve)
            .ok_or(AeraError::ReserveNotFound.into())
    }

    pub fn find_borrow(&self, reserve: Pubkey) -> Result<usize> {
        self.borrows
            .iter()
            .position(|e| e.reserve == reserve)
            .ok_or(AeraError::ReserveNotFound.into())
    }

    /// Live debt for one borrow entry at the reserve's current index.
    pub fn debt_at(&self, index: usize, borrow_index: u128) -> Result<u64> {
        let principal = self.borrows[index].borrowed_principal;
        let exact = principal
            .checked_mul(borrow_index)
            .ok_or(AeraError::MathOverflow)?;
        let floor = exact / FIXED_POINT_SCALE;
        let amount = if exact % FIXED_POINT_SCALE == 0 {
            floor
        } else {
            floor.checked_add(1).ok_or(AeraError::MathOverflow)?
        };
        u64::try_from(amount).map_err(|_| AeraError::MathOverflow.into())
    }
}

/// Per-(reserve, wallet) record of how much liquidity this wallet has supplied
/// through the program. Exists solely to enforce `per_wallet_supply_cap`.
///
/// This tracks supply *through Aera*, not aCOOK held: a wallet that receives
/// aCOOK by transfer is not charged against its cap, and a wallet that sends
/// aCOOK away does not free cap until it withdraws. Nothing here can stop one
/// person using several wallets — the cap is a concentration brake on the
/// honest path, not a proof of identity, and PARAMS.md says so plainly.
#[account]
#[derive(InitSpace)]
pub struct SupplyPosition {
    pub reserve: Pubkey,
    pub owner: Pubkey,
    /// Net liquidity supplied: increased on supply, decreased on withdraw.
    pub supplied_liquidity: u64,
    pub bump: u8,
}
