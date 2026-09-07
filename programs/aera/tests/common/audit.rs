//! The value function and the solvency checker.
//!
//! Every steal test is the same shape: snapshot what everyone owns, let the
//! attacker try something, snapshot again, and assert nobody gained value they
//! were not entitled to and the vault can still pay everyone.
//!
//! "Blocked" is not the assertion that matters. An attack can be permitted by
//! the program and still be a theft, and an attack can fail and still have left
//! the book insolvent. So the tests here measure **value and solvency**, not
//! whether an instruction returned an error.

use super::*;
use anchor_lang::solana_program::instruction::Instruction;

/// One actor's total claim on the protocol, denominated in COOK.
///
/// `native` is deliberately absent: LiteSVM lamports move with rent and fees,
/// which would swamp the signal. Every steal test measures token value, and the
/// wrap-residue tests check lamports separately where that is the point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Value {
    /// Unlocked COOK held directly.
    pub cook: u64,
    /// Share tokens for the COOK reserve.
    pub acook: u64,
    /// Unlocked bCOOK held directly.
    pub bcook: u64,
    /// bCOOK share tokens posted as collateral.
    pub locked: u64,
    /// Live debt owed to the protocol.
    pub debt: u64,
}

impl Value {
    /// Total claim in COOK, at the given bCOOK price and share exchange rate.
    ///
    /// Both rates are passed in rather than read, so a caller can value a
    /// before-snapshot and an after-snapshot at the *same* rate. Valuing each at
    /// its own rate would hide a theft that worked by moving the rate.
    pub fn total(&self, bcook_price_scaled: u128, acook_rate_scaled: u128) -> u128 {
        let acook = (self.acook as u128) * acook_rate_scaled / FIXED_POINT;
        let bcook =
            ((self.bcook as u128) + (self.locked as u128)) * bcook_price_scaled / FIXED_POINT;
        (self.cook as u128) + acook + bcook - (self.debt as u128)
    }
}

pub const FIXED_POINT: u128 = 1_000_000_000_000_000_000;

impl Env {
    /// Everything `owner` owns, across both reserves.
    pub fn value_of(
        &self,
        owner: &Pubkey,
        cook: &ReserveHandle,
        bcook: &ReserveHandle,
        obligation: Option<Pubkey>,
    ) -> Value {
        let locked = obligation
            .map(|address| {
                let account = self.read_obligation(address);
                account
                    .deposits
                    .iter()
                    .find(|d| d.reserve == bcook.reserve)
                    .map(|d| d.deposited_shares)
                    .unwrap_or(0)
            })
            .unwrap_or(0);

        let debt = obligation
            .map(|address| {
                let account = self.read_obligation(address);
                let reserve = self.read_reserve(cook);
                account
                    .borrows
                    .iter()
                    .find(|b| b.reserve == cook.reserve)
                    .map(|b| {
                        // Debt is principal scaled by the index, rounded up: the
                        // borrower owes the ceiling, never the floor.
                        let product = b.borrowed_principal * reserve.borrow_index;
                        let floor = product / FIXED_POINT;
                        u64::try_from(if product.is_multiple_of(FIXED_POINT) {
                            floor
                        } else {
                            floor + 1
                        })
                        .unwrap_or(u64::MAX)
                    })
                    .unwrap_or(0)
            })
            .unwrap_or(0);

        Value {
            cook: self.balance_or_zero(&ata(owner, &cook.mint)),
            acook: self.balance_or_zero(&share_ata(owner, &cook.share_mint)),
            bcook: self.balance_or_zero(&ata(owner, &bcook.mint)),
            locked,
            debt,
        }
    }

    /// Send instructions exactly as written, with no correction.
    ///
    /// Every other send path in the harness builds accounts correctly, which is
    /// right for testing behaviour and useless for testing validation. An
    /// attacker writes the account list by hand, so the audit suite has to as
    /// well.
    pub fn send_raw(
        &mut self,
        instructions: Vec<Instruction>,
        signers: &[&Keypair],
    ) -> Result<(), String> {
        let payer = signers[0].pubkey();
        solana_kite::send_transaction_from_instructions(
            &mut self.svm,
            instructions,
            signers,
            &payer,
        )
        .map(|_| ())
        .map_err(|thrown| format!("{thrown:?}"))
    }

    /// A copy of the real Global with `paused` cleared, owned by the program.
    ///
    /// Used to check that Global is bound to the market rather than merely
    /// deserialised: if the program accepts any account of the right shape, a
    /// pause can be stepped around by supplying a forgery.
    pub fn clone_global_with_pause_cleared(&mut self) -> Pubkey {
        let real = self.svm.get_account(&self.global).expect("global exists");
        let mut data = real.data.clone();

        // Global: 8-byte discriminator, admin, fee_destination, then `paused`.
        let paused_offset = 8 + 32 + 32;
        data[paused_offset] = 0;
        data[paused_offset + 1] = 0; // borrow_paused too

        let forged = Pubkey::new_unique();
        self.svm
            .set_account(
                forged,
                solana_account::Account {
                    lamports: real.lamports,
                    data,
                    owner: real.owner,
                    executable: false,
                    rent_epoch: 0,
                },
            )
            .expect("set forged global");
        forged
    }

    /// `(mint_authority, freeze_authority)` for a mint.
    ///
    /// Read positionally: COption<Pubkey> is a 4-byte tag then the key, and the
    /// layout is identical in both token programs.
    pub fn mint_authorities(&self, mint: &Pubkey) -> (Option<Pubkey>, Option<Pubkey>) {
        let account = self.svm.get_account(mint).expect("mint exists");
        let data = &account.data;

        let read = |tag: usize, key: usize| -> Option<Pubkey> {
            if u32::from_le_bytes(data[tag..tag + 4].try_into().unwrap()) == 1 {
                Some(Pubkey::new_from_array(
                    data[key..key + 32].try_into().unwrap(),
                ))
            } else {
                None
            }
        };

        (read(0, 4), read(46, 50))
    }

    /// A token balance, treating a missing account as zero.
    ///
    /// `balance` panics on an absent account, which is right for a test that
    /// expects one. Valuing an actor is different: an attacker legitimately has
    /// no bCOOK account before their first bCOOK, and "the account does not
    /// exist" and "the account holds nothing" are the same claim on value.
    pub fn balance_or_zero(&self, token_account: &Pubkey) -> u64 {
        match self.svm.get_account(token_account) {
            Some(account) if account.data.len() >= 72 => self.balance(token_account),
            _ => 0,
        }
    }

    /// COOK per aCOOK, 1e18-scaled. 1.0 when no shares exist.
    pub fn acook_rate(&self, cook: &ReserveHandle) -> u128 {
        let reserve = self.read_reserve(cook);
        if reserve.share_mint_supply == 0 {
            return FIXED_POINT;
        }
        let borrowed = {
            let product = reserve.borrowed_principal * reserve.borrow_index;
            let floor = product / FIXED_POINT;
            if product.is_multiple_of(FIXED_POINT) {
                floor
            } else {
                floor + 1
            }
        };
        let gross = (reserve.available_liquidity as u128) + borrowed;
        let pool = gross.saturating_sub(reserve.accrued_fees as u128);
        pool * FIXED_POINT / (reserve.share_mint_supply as u128)
    }

    /// What the reserve owes everyone, against what it can actually pay.
    ///
    /// Insolvency is the only score that matters: if the claims exceed the
    /// tokens plus recoverable debt, funds have been lost regardless of which
    /// instruction succeeded.
    pub fn solvency(&self, handle: &ReserveHandle) -> Solvency {
        let reserve = self.read_reserve(handle);
        let borrowed = {
            let product = reserve.borrowed_principal * reserve.borrow_index;
            let floor = product / FIXED_POINT;
            if product.is_multiple_of(FIXED_POINT) {
                floor
            } else {
                floor + 1
            }
        };

        Solvency {
            // What the vault actually holds right now.
            vault_tokens: self.balance(&handle.liquidity_vault) as u128,
            // What the program believes it holds.
            tracked_available: reserve.available_liquidity as u128,
            outstanding_debt: borrowed,
            share_supply: reserve.share_mint_supply as u128,
            accrued_fees: reserve.accrued_fees as u128,
            share_rate: self.acook_rate(handle),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Solvency {
    pub vault_tokens: u128,
    pub tracked_available: u128,
    pub outstanding_debt: u128,
    pub share_supply: u128,
    pub accrued_fees: u128,
    pub share_rate: u128,
}

impl Solvency {
    /// What every share holder could redeem, plus what the protocol is owed.
    pub fn total_claims(&self) -> u128 {
        self.share_supply * self.share_rate / FIXED_POINT + self.accrued_fees
    }

    /// What exists to pay those claims.
    pub fn total_assets(&self) -> u128 {
        self.tracked_available + self.outstanding_debt
    }

    /// The vault must hold at least what it says it holds.
    ///
    /// A donation makes `vault_tokens` exceed `tracked_available`, which is fine
    /// and is exactly why donations cannot move the exchange rate. The failure
    /// direction is the other one: tokens missing that the program thinks are
    /// there.
    pub fn vault_backs_tracked(&self) -> bool {
        self.vault_tokens >= self.tracked_available
    }

    /// Claims must not exceed assets by more than integer rounding.
    ///
    /// Two base units of slack: share maths floors in the protocol's favour on
    /// the way in and on the way out, so a unit can be left on either side. More
    /// than that is not rounding.
    pub fn solvent(&self) -> bool {
        self.total_claims() <= self.total_assets() + 2
    }

    pub fn report(&self, label: &str) -> String {
        format!(
            "{label}: vault {} tracked {} debt {} shares {} rate {} fees {} | claims {} assets {}",
            self.vault_tokens,
            self.tracked_available,
            self.outstanding_debt,
            self.share_supply,
            self.share_rate,
            self.accrued_fees,
            self.total_claims(),
            self.total_assets(),
        )
    }
}

/// Assert a reserve is solvent and its vault backs what the program tracks.
///
/// Called at the end of every steal test. A test that only asserts "the attack
/// errored" can pass while the book is broken.
pub fn assert_solvent(env: &Env, handle: &ReserveHandle, label: &str) {
    let solvency = env.solvency(handle);
    assert!(
        solvency.vault_backs_tracked(),
        "VAULT SHORT — {}",
        solvency.report(label)
    );
    assert!(solvency.solvent(), "INSOLVENT — {}", solvency.report(label));
}

/// Assert an actor did not gain value.
///
/// Both snapshots are valued at the *same* rates, so a gain cannot be disguised
/// as a rate movement. `allowed` covers legitimate proceeds - borrow amount, a
/// liquidation bonus - and defaults to zero for pure theft attempts.
pub fn assert_no_profit(
    before: Value,
    after: Value,
    bcook_price: u128,
    acook_rate: u128,
    allowed: u128,
    label: &str,
) {
    let start = before.total(bcook_price, acook_rate);
    let end = after.total(bcook_price, acook_rate);
    let gained = end.saturating_sub(start);
    assert!(
        gained <= allowed,
        "PROFIT — {label}: value {start} -> {end}, gained {gained}, allowed {allowed}\n  before {before:?}\n  after  {after:?}"
    );
}
