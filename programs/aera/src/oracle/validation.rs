//! Account checks every oracle source needs, independent of what it parses.
//!
//! These sit here rather than inside `native_bcook` because they are properties
//! of *reading an external account safely*, not of the stake-pool format. A
//! second source would need the same ones, and a check that exists in two
//! places eventually exists in one and a half.

use anchor_lang::prelude::*;

use crate::errors::AeraError;

/// An account passed in `remaining_accounts` is only usable if it is the exact
/// account configuration names, owned by the exact program configuration names.
///
/// Both halves matter and neither implies the other:
///
/// - **address alone** is not enough, because an attacker who can create an
///   account at a colliding address, or a program that closes and recreates
///   one, changes what those bytes mean;
/// - **owner alone** is not enough, because the stake-pool program owns every
///   stake pool, including ones with unrelated backing that would price bCOOK
///   off the wrong assets entirely.
pub fn require_configured_account(
    account: &AccountInfo,
    expected_key: &Pubkey,
    expected_owner: &Pubkey,
) -> Result<()> {
    require_keys_eq!(
        *account.key,
        *expected_key,
        AeraError::OracleAccountMismatch
    );
    require_keys_eq!(
        *account.owner,
        *expected_owner,
        AeraError::OracleOwnerMismatch
    );
    Ok(())
}

/// Refuse an account that is writable when it has no business being.
///
/// An oracle source is read. Passing it writable is either a mistake or an
/// attempt to have it mutated by something else in the same transaction, and
/// neither is a state Aera should price collateral from.
pub fn require_read_only(account: &AccountInfo) -> Result<()> {
    require!(!account.is_writable, AeraError::OracleAccountMalformed);
    Ok(())
}

/// Refuse a duplicated account in a set that must be distinct.
///
/// The classic shape this defends against is passing the same account twice
/// where the handler assumes two, so that one write lands where the code
/// believes two independent ones did. For the oracle the risk is narrower --
/// the same pool standing in for two different collateral reserves -- but the
/// check is cheap and the failure is silent without it.
pub fn require_distinct(keys: &[Pubkey]) -> Result<()> {
    for (index, key) in keys.iter().enumerate() {
        if keys[index + 1..].contains(key) {
            return err!(AeraError::OracleAccountMismatch);
        }
    }
    Ok(())
}

/// Refuse an account with no lamports.
///
/// A zero-lamport account is scheduled for deletion at the end of the
/// transaction; its data is not something to make a lending decision from.
pub fn require_alive(account: &AccountInfo) -> Result<()> {
    require!(
        **account.try_borrow_lamports()? > 0,
        AeraError::OracleAccountMalformed
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinct_accepts_a_set_with_no_repeats() {
        let keys = [
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        ];
        assert!(require_distinct(&keys).is_ok());
    }

    #[test]
    fn distinct_refuses_any_repeat_wherever_it_sits() {
        let a = Pubkey::new_unique();
        let b = Pubkey::new_unique();

        assert!(require_distinct(&[a, a]).is_err(), "adjacent");
        assert!(require_distinct(&[a, b, a]).is_err(), "separated");
        assert!(require_distinct(&[b, a, a]).is_err(), "at the end");
    }

    #[test]
    fn distinct_is_trivially_true_for_zero_or_one() {
        assert!(require_distinct(&[]).is_ok());
        assert!(require_distinct(&[Pubkey::new_unique()]).is_ok());
    }
}
