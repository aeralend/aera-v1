//! Pinning the price source's *program deployment*, not just its program id.
//!
//! Validating that the stake-pool account is owned by
//! `GZgs5uREPp6BvDt8eysmhavQPAHBAtjePgV4zfhgd9pH` proves who owns the bytes. It
//! does not prove what the code behind that id does, because the code can be
//! replaced. Measured on Cookie Chain:
//!
//! ```text
//!   loader              BPFLoaderUpgradeab1e11111111111111111111111
//!   ProgramData         6Dsx1cdbzEsNaV4BKzJhEvTpuSGf4CcVH3UCt45mERTF
//!   last deploy slot    5504973
//!   upgrade authority   GSPUoahS7jSQUEAEkjaejsN9vo2w4B2NYHZ9oJSMm45p
//!   authority kind      system-owned, 0 bytes of data -- a SINGLE KEY
//! ```
//!
//! So one wallet can redeploy the program that every bCOOK valuation in Aera
//! depends on, keeping the account layout intact while changing what the
//! numbers mean.
//!
//! ## What this module does about it
//!
//! It pins the deployment. `ProgramData` carries the last deploy slot and the
//! current upgrade authority in a 45-byte header, and the loader bumps that
//! slot on **every** redeploy. Comparing 8 bytes and 32 bytes at fixed offsets
//! costs a few hundred compute units, so it can run on every observation.
//!
//! A redeploy therefore cannot go unnoticed. Aera stops accepting new
//! observations, borrowing freezes, and repayment and liquidation continue
//! against the last reference the breaker accepted.
//!
//! ## What it deliberately does not do
//!
//! Hash the binary. The ELF is 427,112 bytes and hashing it on chain would cost
//! roughly one compute unit per byte against a 1.4M budget the same transaction
//! also needs for accrual, an obligation refresh and the action itself. It
//! would also add nothing: the loader cannot change those bytes without
//! bumping the slot, so the slot is the cheap equivalent of the hash.
//!
//! ## What it cannot do
//!
//! Tell a benign redeploy from a hostile one, or stop one happening. It can
//! only refuse to keep lending across a change no human has looked at. The
//! strongest configuration remains the one Aera cannot bring about on its own:
//! BakeYourStake revoking their upgrade authority.

use anchor_lang::prelude::*;

use crate::errors::AeraError;

/// `BPFLoaderUpgradeab1e11111111111111111111111`.
///
/// Written out rather than imported so the constant is visible next to the
/// layout it describes.
pub const BPF_LOADER_UPGRADEABLE: Pubkey = anchor_lang::solana_program::bpf_loader_upgradeable::ID;

/// `UpgradeableLoaderState::ProgramData` — the fourth variant, so 3.
const PROGRAM_DATA_VARIANT: u32 = 3;

/// Bytes before the ELF: 4-byte bincode discriminant, 8-byte slot, then an
/// `Option<Pubkey>` as a 1-byte tag and 32 bytes.
const HEADER_LEN: usize = 4 + 8 + 1 + 32;

/// Sentinel for "the program is immutable".
///
/// `Pubkey::default()` is all zeros and is not a key anyone holds, so it
/// doubles as `None` without needing an extra byte in `OracleState`.
pub const NO_UPGRADE_AUTHORITY: Pubkey = Pubkey::new_from_array([0u8; 32]);

/// What was read out of a `ProgramData` account.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Deployment {
    pub deploy_slot: u64,
    /// [`NO_UPGRADE_AUTHORITY`] when the program has been made immutable.
    pub upgrade_authority: Pubkey,
}

impl Deployment {
    pub fn is_immutable(&self) -> bool {
        self.upgrade_authority == NO_UPGRADE_AUTHORITY
    }
}

/// The `ProgramData` address for a program, derived rather than accepted.
///
/// It is a PDA of `[program_id]` under the loader — verified against Cookie
/// Chain, where `GZgs5u…` derives to `6Dsx1c…` at bump 253. Deriving it means
/// a caller cannot point Aera at some other account's header.
pub fn program_data_address(program_id: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[program_id.as_ref()], &BPF_LOADER_UPGRADEABLE).0
}

/// Read a `ProgramData` header, having first proved it is the right one.
pub fn read_deployment(account: &AccountInfo, program_id: &Pubkey) -> Result<Deployment> {
    // 1. The account is the one this program's id derives to.
    require_keys_eq!(
        *account.key,
        program_data_address(program_id),
        AeraError::OracleProgramDataMismatch
    );

    // 2. Owned by the loader. An account at the right address that the loader
    //    does not own is not loader state.
    require_keys_eq!(
        *account.owner,
        BPF_LOADER_UPGRADEABLE,
        AeraError::OracleProgramDataMismatch
    );

    let data = account.try_borrow_data()?;
    require!(
        data.len() >= HEADER_LEN,
        AeraError::OracleProgramDataMismatch
    );

    // 3. It really is ProgramData and not, say, a Buffer.
    let variant = u32::from_le_bytes(
        data[0..4]
            .try_into()
            .map_err(|_| AeraError::OracleProgramDataMismatch)?,
    );
    require!(
        variant == PROGRAM_DATA_VARIANT,
        AeraError::OracleProgramDataMismatch
    );

    let deploy_slot = u64::from_le_bytes(
        data[4..12]
            .try_into()
            .map_err(|_| AeraError::OracleProgramDataMismatch)?,
    );

    let upgrade_authority = match data[12] {
        0 => NO_UPGRADE_AUTHORITY,
        1 => {
            let bytes: [u8; 32] = data[13..45]
                .try_into()
                .map_err(|_| AeraError::OracleProgramDataMismatch)?;
            let key = Pubkey::from(bytes);
            // A stored authority of all-zeros would be indistinguishable from
            // "immutable", which would let a redeploy to that key look like a
            // revocation. Refuse rather than resolve the ambiguity.
            require!(
                key != NO_UPGRADE_AUTHORITY,
                AeraError::OracleProgramDataMismatch
            );
            key
        }
        _ => return err!(AeraError::OracleProgramDataMismatch),
    };

    Ok(Deployment {
        deploy_slot,
        upgrade_authority,
    })
}

/// Compare an observed deployment against the pinned one.
///
/// The slot must match exactly: it changes on every redeploy, which is the
/// whole signal.
///
/// The authority may **become** [`NO_UPGRADE_AUTHORITY`] without failing. That
/// transition is a revocation — the program becoming permanently immutable —
/// and refusing to keep operating because the source got safer would be
/// perverse. Every other authority change fails, including a revoked pin
/// growing an authority again.
///
/// Note the two are independent: a redeploy followed by a revocation still
/// fails, because the slot moved.
pub fn verify_matches_pin(
    observed: &Deployment,
    pinned_slot: u64,
    pinned_authority: Pubkey,
) -> Result<()> {
    require!(
        observed.deploy_slot == pinned_slot,
        AeraError::OracleProgramUpgraded
    );

    if observed.upgrade_authority == pinned_authority {
        return Ok(());
    }
    if observed.is_immutable() {
        // Some(pinned) -> None. Strictly safer; accept.
        return Ok(());
    }
    err!(AeraError::OracleAuthorityChanged)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The address Cookie Chain actually reports for the bCOOK stake pool.
    #[test]
    fn program_data_is_derived_the_way_the_loader_derives_it() {
        let program: Pubkey = "GZgs5uREPp6BvDt8eysmhavQPAHBAtjePgV4zfhgd9pH"
            .parse()
            .unwrap();
        let expected: Pubkey = "6Dsx1cdbzEsNaV4BKzJhEvTpuSGf4CcVH3UCt45mERTF"
            .parse()
            .unwrap();
        assert_eq!(
            program_data_address(&program),
            expected,
            "the derivation no longer matches what is on chain"
        );
    }

    fn pinned() -> (u64, Pubkey) {
        (5_504_973, Pubkey::new_unique())
    }

    /// Assert a result failed with exactly this Aera error.
    ///
    /// `is_err()` is not enough here. Every check in this module refuses, so a
    /// test that only asserts refusal would pass if the wrong check fired --
    /// and the two errors mean very different things to an operator: one says
    /// the code was replaced, the other says the account is not what it claims.
    fn assert_error<T: core::fmt::Debug>(result: Result<T>, expected: AeraError) {
        // Compared by code, not by the whole error: Anchor attaches the source
        // line and the compared pubkeys, which differ per call site and say
        // nothing about which rule fired.
        let wanted = expected as u32 + anchor_lang::error::ERROR_CODE_OFFSET;
        match result {
            Ok(value) => panic!("expected {expected:?}, but it succeeded: {value:?}"),
            Err(Error::AnchorError(actual)) => assert_eq!(
                actual.error_code_number, wanted,
                "refused for the wrong reason: {actual:?}"
            ),
            Err(other) => panic!("expected {expected:?}, got a program error: {other:?}"),
        }
    }

    #[test]
    fn an_unchanged_deployment_passes() {
        let (slot, authority) = pinned();
        let observed = Deployment {
            deploy_slot: slot,
            upgrade_authority: authority,
        };
        assert!(verify_matches_pin(&observed, slot, authority).is_ok());
    }

    #[test]
    fn a_redeploy_fails_even_with_the_same_authority() {
        let (slot, authority) = pinned();
        let observed = Deployment {
            deploy_slot: slot + 1,
            upgrade_authority: authority,
        };
        assert_error(
            verify_matches_pin(&observed, slot, authority),
            AeraError::OracleProgramUpgraded,
        );
    }

    #[test]
    fn revoking_the_authority_is_accepted() {
        let (slot, authority) = pinned();
        let observed = Deployment {
            deploy_slot: slot,
            upgrade_authority: NO_UPGRADE_AUTHORITY,
        };
        assert!(
            verify_matches_pin(&observed, slot, authority).is_ok(),
            "the source becoming immutable must not freeze Aera"
        );
    }

    #[test]
    fn transferring_the_authority_fails() {
        let (slot, authority) = pinned();
        let observed = Deployment {
            deploy_slot: slot,
            upgrade_authority: Pubkey::new_unique(),
        };
        assert_error(
            verify_matches_pin(&observed, slot, authority),
            AeraError::OracleAuthorityChanged,
        );
    }

    #[test]
    fn a_revoked_pin_growing_an_authority_fails() {
        let slot = 5_504_973;
        let observed = Deployment {
            deploy_slot: slot,
            upgrade_authority: Pubkey::new_unique(),
        };
        // An immutable program cannot sprout an authority.
        assert_error(
            verify_matches_pin(&observed, slot, NO_UPGRADE_AUTHORITY),
            AeraError::OracleAuthorityChanged,
        );
    }

    /// Build a `ProgramData` account body, independently of the reader.
    fn header(slot: u64, authority: Option<Pubkey>) -> Vec<u8> {
        let mut data = vec![0u8; HEADER_LEN];
        data[0..4].copy_from_slice(&PROGRAM_DATA_VARIANT.to_le_bytes());
        data[4..12].copy_from_slice(&slot.to_le_bytes());
        match authority {
            Some(key) => {
                data[12] = 1;
                data[13..45].copy_from_slice(key.as_ref());
            }
            None => data[12] = 0,
        }
        data
    }

    /// Run `read_deployment` against a synthetic account.
    ///
    /// `AccountInfo` borrows, so the caller owns the parts and this only ties
    /// them together for the duration of the read.
    fn read(
        key: &Pubkey,
        owner: &Pubkey,
        data: &mut [u8],
        lamports: &mut u64,
        program_id: &Pubkey,
    ) -> Result<Deployment> {
        let account = AccountInfo::new(key, false, false, lamports, data, owner, false);
        read_deployment(&account, program_id)
    }

    #[test]
    fn a_well_formed_header_reads_back_exactly() {
        let program = Pubkey::new_unique();
        let key = program_data_address(&program);
        let authority = Pubkey::new_unique();
        let mut data = header(5_504_973, Some(authority));
        let mut lamports = 1u64;

        let observed = read(
            &key,
            &BPF_LOADER_UPGRADEABLE,
            &mut data,
            &mut lamports,
            &program,
        )
        .expect("a well-formed ProgramData must read");
        assert_eq!(observed.deploy_slot, 5_504_973);
        assert_eq!(observed.upgrade_authority, authority);

        // And a real ELF after the header changes nothing: the reader takes a
        // fixed prefix and ignores the 427 KB the loader keeps behind it.
        let mut long = header(5_504_973, Some(authority));
        long.extend_from_slice(&[0xAAu8; 512]);
        let observed = read(
            &key,
            &BPF_LOADER_UPGRADEABLE,
            &mut long,
            &mut lamports,
            &program,
        )
        .expect("trailing program bytes must not disturb the header");
        assert_eq!(observed.deploy_slot, 5_504_973);
    }

    #[test]
    fn an_immutable_header_reads_as_the_sentinel() {
        let program = Pubkey::new_unique();
        let key = program_data_address(&program);
        let mut data = header(5_504_973, None);
        let mut lamports = 1u64;

        let observed = read(
            &key,
            &BPF_LOADER_UPGRADEABLE,
            &mut data,
            &mut lamports,
            &program,
        )
        .expect("an immutable ProgramData must read");
        assert!(observed.is_immutable());
    }

    #[test]
    fn an_account_at_another_address_is_refused() {
        let program = Pubkey::new_unique();
        let elsewhere = Pubkey::new_unique();
        let mut data = header(5_504_973, Some(Pubkey::new_unique()));
        let mut lamports = 1u64;

        assert_error(
            read(
                &elsewhere,
                &BPF_LOADER_UPGRADEABLE,
                &mut data,
                &mut lamports,
                &program,
            ),
            AeraError::OracleProgramDataMismatch,
        );
    }

    #[test]
    fn an_account_not_owned_by_the_loader_is_refused() {
        let program = Pubkey::new_unique();
        let key = program_data_address(&program);
        let stranger = Pubkey::new_unique();
        let mut data = header(5_504_973, Some(Pubkey::new_unique()));
        let mut lamports = 1u64;

        assert_error(
            read(&key, &stranger, &mut data, &mut lamports, &program),
            AeraError::OracleProgramDataMismatch,
        );
    }

    #[test]
    fn the_program_variant_is_not_mistaken_for_program_data() {
        let program = Pubkey::new_unique();
        let key = program_data_address(&program);
        // UpgradeableLoaderState::Program { programdata_address }, variant 2.
        // Same owner, same address, 36 bytes. Only the discriminant differs --
        // and reading it as ProgramData would take part of the 32-byte pointer
        // as a deploy slot.
        let mut data = vec![0u8; 36];
        data[0..4].copy_from_slice(&2u32.to_le_bytes());
        data[4..36].copy_from_slice(key.as_ref());
        let mut lamports = 1u64;

        assert_error(
            read(
                &key,
                &BPF_LOADER_UPGRADEABLE,
                &mut data,
                &mut lamports,
                &program,
            ),
            AeraError::OracleProgramDataMismatch,
        );
    }

    #[test]
    fn a_truncated_header_is_refused_rather_than_read_short() {
        let program = Pubkey::new_unique();
        let key = program_data_address(&program);
        let mut data = header(5_504_973, Some(Pubkey::new_unique()));
        data.truncate(HEADER_LEN - 1);
        let mut lamports = 1u64;

        assert_error(
            read(
                &key,
                &BPF_LOADER_UPGRADEABLE,
                &mut data,
                &mut lamports,
                &program,
            ),
            AeraError::OracleProgramDataMismatch,
        );
    }

    #[test]
    fn an_out_of_range_option_tag_is_refused() {
        let program = Pubkey::new_unique();
        let key = program_data_address(&program);
        let mut data = header(5_504_973, Some(Pubkey::new_unique()));
        data[12] = 2; // neither None (0) nor Some (1)
        let mut lamports = 1u64;

        assert_error(
            read(
                &key,
                &BPF_LOADER_UPGRADEABLE,
                &mut data,
                &mut lamports,
                &program,
            ),
            AeraError::OracleProgramDataMismatch,
        );
    }

    /// `Some(all zeros)` is refused rather than resolved into "immutable".
    ///
    /// Otherwise a redeploy that set the authority to the zero key would look
    /// exactly like a revocation, and `verify_matches_pin` would wave it
    /// through as the one authority change it is meant to allow.
    #[test]
    fn a_some_authority_of_all_zeros_is_refused() {
        let program = Pubkey::new_unique();
        let key = program_data_address(&program);
        let mut data = header(5_504_973, Some(NO_UPGRADE_AUTHORITY));
        let mut lamports = 1u64;

        assert_error(
            read(
                &key,
                &BPF_LOADER_UPGRADEABLE,
                &mut data,
                &mut lamports,
                &program,
            ),
            AeraError::OracleProgramDataMismatch,
        );
    }

    #[test]
    fn a_redeploy_followed_by_revocation_still_fails() {
        let (slot, authority) = pinned();
        let observed = Deployment {
            deploy_slot: slot + 1,
            upgrade_authority: NO_UPGRADE_AUTHORITY,
        };
        assert!(
            verify_matches_pin(&observed, slot, authority).is_err(),
            "revoking after a redeploy must not launder the redeploy"
        );
    }
}
