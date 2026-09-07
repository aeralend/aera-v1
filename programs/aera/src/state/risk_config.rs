//! Per-reserve risk limits that arrived after v0.2, in their own account.
//!
//! # Why this is not a field on `ReserveConfig`
//!
//! It was, briefly. Adding `per_wallet_borrow_cap` to `ReserveConfig` grows
//! `Reserve` by eight bytes, and `migrate.rs:145` records what that costs:
//!
//! > A v0.1 reserve that v0.2 cannot deserialise cannot be passed to `accrue`,
//! > and every repayment needs an accrued reserve -- so a longer `Reserve` traps
//! > borrowers for the whole duration of a migration.
//!
//! `test_half_migrated::half_06` caught that during the v0.2 work. Growing the
//! struct again reproduces it exactly: between a v0.3 upgrade and the last
//! reserve being reallocated, every v0.2 reserve is short, `accrue` fails on it,
//! and a borrower cannot repay. Repayment is the one action Aera never blocks.
//!
//! So the limit lives beside the reserve rather than inside it. `Reserve` keeps
//! its v0.2 layout byte for byte, no migration is needed, and Core is untouched.
//!
//! # Absent means unlimited
//!
//! A reserve with no `RiskConfig` is unconstrained, which is what every existing
//! Aera cap means by zero (`supply_cap`, `borrow_cap`, `per_wallet_supply_cap`).
//! Core therefore needs no account created for it at all -- the control exists
//! for volatile collateral, and Core's is a stake-pool rate that cannot be moved
//! by trading.
//!
//! The account is optional in `borrow`. An attacker cannot evade the cap by
//! omitting it: the address is a PDA of the reserve, so the program derives the
//! expected key and refuses a substitute, and a reserve that *should* have one
//! is opted in by `requires_risk_config` on the account itself.

use anchor_lang::prelude::*;

/// Risk limits for one reserve, added in v0.3.
///
/// Seeded `["risk_config", reserve]`. Created only for reserves that need one.
#[account]
#[derive(InitSpace)]
pub struct RiskConfig {
    /// The reserve these limits apply to. Checked against the passed account.
    pub reserve: Pubkey,

    /// The most one wallet may owe in this reserve. `0` is unlimited.
    ///
    /// Zero means unlimited to match every other cap in the protocol. Two caps
    /// that meant opposite things by zero would be a configuration footgun of
    /// exactly the kind found in production rather than in review.
    ///
    /// ## What this is, and what it is not
    ///
    /// It is **not** Sybil resistance. One person can open a second wallet and
    /// nothing here stops them. Any description of it as a per-person limit is
    /// wrong.
    ///
    /// It bounds the size of a *single liquidation*, which matters because
    /// liquidator economics depend on how the seized collateral is sold.
    ///
    /// Against COOKHOUSE's measured book, a liquidator routing across both
    /// Meteora pools slips under 4% at every size the caps permit and profits
    /// monotonically. One selling into the thinner pool alone slips 9.10% at
    /// 75,000 of debt closed, peaks in profit near 50,000, and is underwater
    /// past roughly 90,000.
    ///
    /// The cap is sized against the second case, because a liquidator with one
    /// venue integrated is a liquidator who exists and the protocol does not
    /// choose which kind turns up. A market-wide borrow cap does not stop one
    /// borrower reaching that size alone. This does.
    ///
    /// `tools/liquidation-economics.ts` computes both columns from the
    /// observation log rather than from a figure copied forward.
    ///
    /// The obligation PDA is `["obligation", market, owner]` opened with `init`,
    /// so a wallet holds exactly one obligation per market and cannot split debt
    /// across several to evade the check.
    pub per_wallet_borrow_cap: u64,

    /// Aera's share of the liquidation bonus, in basis points of value repaid.
    ///
    /// **Carved out of the existing bonus, never added to it.** With a 1200 bps
    /// total bonus and this at 150, the borrower still loses exactly 12% and the
    /// liquidator receives 10.5% instead of 12%. A borrower's penalty does not
    /// depend on this field at all — `risk::split_seized_shares` divides an
    /// already-computed seizure rather than enlarging it.
    ///
    /// Zero means Aera takes nothing, which is Core's setting and the default
    /// for a reserve with no `RiskConfig` at all. Bounded by
    /// [`crate::constants::MAX_PROTOCOL_LIQUIDATION_SHARE_BPS`] and separately
    /// by the collateral reserve's own `liquidation_bonus_bps`, since a share
    /// larger than the bonus would take from the liquidator's principal.
    ///
    /// Lives on the **collateral** reserve, beside the bonus it splits.
    /// `liquidation_bonus_bps` is a property of the asset being seized, not of
    /// the debt being repaid, and putting the two halves of one number in
    /// different places would be a reliable source of misconfiguration.
    pub protocol_liquidation_share_bps: u16,

    /// A queued loosening, and when it may be applied.
    ///
    /// Raising a cap is a loosening and waits out `Global::param_timelock_seconds`,
    /// exactly as `ReserveConfig` loosenings do. Lowering one is a tightening and
    /// lands immediately -- reducing what a wallet may owe can never make the
    /// market less safe, and an operator responding to an incident should not
    /// have to wait a day to do it.
    ///
    /// `eta == 0` means nothing is queued.
    pub pending_per_wallet_borrow_cap: u64,

    /// The queued share. Raising Aera's cut is a loosening and waits; lowering
    /// it lands immediately, because a smaller protocol cut always leaves the
    /// liquidator more and can never make the market less safe.
    pub pending_protocol_liquidation_share_bps: u16,

    pub pending_eta: i64,

    pub bump: u8,
}

impl RiskConfig {
    /// Is `next` no looser than `current`? `0` is unlimited, so it is loosest.
    ///
    /// Mirrors `reserve::cap_is_tighter`. Kept as its own function rather than
    /// imported so the two cannot silently diverge on what zero means.
    pub fn cap_is_tighter(current: u64, next: u64) -> bool {
        match (current, next) {
            (0, 0) => true,
            (0, _) => true,  // unlimited -> limited is a tightening
            (_, 0) => false, // limited -> unlimited is a loosening
            (a, b) => b <= a,
        }
    }
}

impl RiskConfig {
    pub const SEED: &'static [u8] = b"risk_config";
}
