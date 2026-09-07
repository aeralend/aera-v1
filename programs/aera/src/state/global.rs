//! Domain 1 of 4: **Global**.
//!
//! One account per protocol deployment. Holds the admin key, the protocol-wide
//! pause switches, and the two destinations the reserve factor is split
//! between. Everything else (markets, reserves, obligations) hangs off it.
//!
//! This is the `aera-registry` domain from DESIGN.md, kept as an account domain
//! inside the single `aera` program rather than a separate program — see
//! docs/DECISIONS.md.

use anchor_lang::prelude::*;

use crate::constants::{DEFAULT_PARAM_TIMELOCK_SECONDS, GLOBAL_SEED};
use crate::errors::AeraError;

/// Signer seeds for the global PDA.
pub fn global_signer_seeds(bump: &[u8; 1]) -> [&[u8]; 2] {
    [GLOBAL_SEED, bump]
}

/// Schema version of the `Global` account.
///
/// v0.1 predates this field entirely: its account is one byte shorter and has
/// no version at all. The migration reallocs it and stamps `V2` last, after
/// every other validation has passed, so a half-migrated deployment reads as
/// v0.1 rather than as a v0.2 that happens to be broken.
pub const GLOBAL_VERSION_V1: u8 = 0;
pub const GLOBAL_VERSION_V2: u8 = 2;

/// Serialized length of a v0.1 `Global`: discriminator + 32 + 32 + 1 + 1 + 8 + 1.
///
/// Asserted rather than assumed -- see the compile-time check below. The
/// migration uses it to tell an un-migrated account from a migrated one by
/// length, which no caller can forge.
pub const GLOBAL_V1_LEN: usize = 8 + 32 + 32 + 1 + 1 + 8 + 1;

#[account]
#[derive(InitSpace)]
pub struct Global {
    /// The only key that may create markets/reserves, change parameters, or
    /// pause. Rotating it is a deliberate, separate instruction.
    pub admin: Pubkey,

    /// Where the protocol's whole cut of interest is paid.
    ///
    /// One destination, not two: the reserve factor accrues 100% here. A split
    /// would have to be reconciled at every accrual and every payout, and there
    /// is no second beneficiary to reconcile it for.
    pub fee_destination: Pubkey,

    /// Blocks every state-changing user instruction except `repay`. Repay is
    /// never blocked: refusing repayment while prices move would manufacture
    /// liquidations the borrower could have avoided.
    pub paused: bool,

    /// Blocks new borrows only. Supply, withdraw, repay and liquidate continue.
    /// Set by the admin, and independently by the oracle circuit breaker.
    pub borrow_paused: bool,

    /// Delay applied to *loosening* parameter changes. Tightening changes and
    /// pauses bypass it entirely.
    pub param_timelock_seconds: i64,

    pub bump: u8,

    /// Schema version. Absent in v0.1, so a v0.1 account is exactly one byte
    /// shorter than this struct and reads as [`GLOBAL_VERSION_V1`] once
    /// realloc'd and zero-filled.
    ///
    /// Declared last on purpose: appending keeps every preceding field at the
    /// offset it already occupies, so the realloc adds a byte rather than
    /// moving anything.
    pub version: u8,
}

/// The v0.1 length must be exactly one byte less than v0.2's.
///
/// If a future change adds a field anywhere but the end, this fails to compile
/// rather than silently making the migration's length check meaningless.
const _: () = assert!(GLOBAL_V1_LEN + 1 == 8 + Global::INIT_SPACE);

impl Global {
    pub fn require_admin(&self, signer: &Pubkey) -> Result<()> {
        require_keys_eq!(self.admin, *signer, AeraError::NotAdmin);
        Ok(())
    }

    /// Gate for user instructions that move value in the risky direction.
    pub fn require_not_paused(&self) -> Result<()> {
        require!(!self.paused, AeraError::ProtocolPaused);
        Ok(())
    }

    /// Gate for borrowing specifically. `paused` implies `borrow_paused`.
    pub fn require_borrow_enabled(&self) -> Result<()> {
        self.require_not_paused()?;
        require!(!self.borrow_paused, AeraError::BorrowPaused);
        Ok(())
    }

    pub fn validate_timelock(seconds: i64) -> Result<()> {
        require!(
            (0..=crate::constants::MAX_PARAM_TIMELOCK_SECONDS).contains(&seconds),
            AeraError::InvalidTimelock
        );
        Ok(())
    }

    pub fn default_timelock() -> i64 {
        DEFAULT_PARAM_TIMELOCK_SECONDS
    }
}
