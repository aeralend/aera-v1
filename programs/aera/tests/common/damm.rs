//! Fabricated Meteora DAMM v2 pools, for testing the Tier 3 oracle.
//!
//! The program decodes four fields out of a real pool account by byte offset,
//! because Meteora publishes no IDL on Cookie Chain. That decoder is the part
//! most likely to be wrong in a way nothing notices, so these helpers build
//! accounts with exactly the shape it expects — and, more usefully, accounts
//! with deliberately wrong shapes, so the failure paths are exercised too.
//!
//! Offsets are stated once here and once in the program. They are duplicated on
//! purpose: if someone changes the program's constants, these tests should stop
//! agreeing with it rather than silently follow along.
//!
//! Everything a test might want to get wrong is a parameter — the owner, the
//! length, the orientation, the vault addresses, the mints, the balances — so a
//! test says what it is testing rather than assembling bytes.

use anchor_lang::prelude::Pubkey;
use litesvm::LiteSVM;
use solana_account::Account as SvmAccount;

/// The AMM these mock pools claim to belong to.
///
/// Synthetic, like `TEST_STAKE_POOL_PROGRAM`: the oracle is configured with
/// whatever program it should trust, and the harness configures this one. The
/// real deployment on Cookie Chain is
/// `DAMMjDCEFTDkt7ywazZS8GoaLtjb3HaJo3pLbf64xrPY`, ProgramData
/// `6ztSm42C4aAAqZeEcAshzHDUUAxRL4T9ejZf5tEWPrgS`, deploy slot 3,176,591,
/// upgrade authority `HGSGbiM3tMvbX8cxitEgzbQv53M4rFcsE1gn7fvrHrkN` -- a key
/// Aera does not control, which is why the deploy slot is pinned at all.
pub const DAMM_PROGRAM: Pubkey = Pubkey::new_from_array([9u8; 32]);

pub const POOL_LEN: usize = 1112;
pub const OFF_MINT_A: usize = 168;
pub const OFF_MINT_B: usize = 200;
pub const OFF_VAULT_A: usize = 232;
pub const OFF_VAULT_B: usize = 264;

/// Which side of the pool holds the collateral asset.
///
/// Both orientations occur in the wild: the two live COOKHOUSE pools disagree
/// about which token is A. Any test that only exercises one of these is only
/// testing half the market.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Orientation {
    /// Collateral is token A, quote is token B.
    CollateralFirst,
    /// Collateral is token B, quote is token A.
    QuoteFirst,
}

/// A fabricated pool and the vaults it names.
#[derive(Clone, Copy, Debug)]
pub struct MockPool {
    pub pool: Pubkey,
    pub collateral_vault: Pubkey,
    pub quote_vault: Pubkey,
    pub collateral_mint: Pubkey,
    pub quote_mint: Pubkey,
    pub orientation: Orientation,
}

/// How a pool should be built. Defaults are valid; tests override one field.
pub struct PoolSpec {
    pub pool: Pubkey,
    pub collateral_mint: Pubkey,
    pub quote_mint: Pubkey,
    pub collateral_reserve: u64,
    pub quote_reserve: u64,
    pub orientation: Orientation,

    // --- the things a test breaks on purpose ---
    /// Defaults to `DAMM_PROGRAM`. Set to something else to test the owner check.
    pub owner: Pubkey,
    /// Defaults to `POOL_LEN`. Any other value must fail closed.
    pub length: usize,
    /// Write a different mint at the collateral offset.
    pub wrong_collateral_mint: Option<Pubkey>,
    /// Write a different mint at the quote offset.
    pub wrong_quote_mint: Option<Pubkey>,
    /// Name a vault the caller will not pass, or a vault holding the wrong mint.
    pub override_collateral_vault: Option<Pubkey>,
    pub override_quote_vault: Option<Pubkey>,
    /// Name the same account for both vaults.
    pub duplicate_vaults: bool,
}

impl PoolSpec {
    pub fn new(
        pool: Pubkey,
        collateral_mint: Pubkey,
        quote_mint: Pubkey,
        collateral_reserve: u64,
        quote_reserve: u64,
    ) -> Self {
        Self {
            pool,
            collateral_mint,
            quote_mint,
            collateral_reserve,
            quote_reserve,
            orientation: Orientation::CollateralFirst,
            owner: DAMM_PROGRAM,
            length: POOL_LEN,
            wrong_collateral_mint: None,
            wrong_quote_mint: None,
            override_collateral_vault: None,
            override_quote_vault: None,
            duplicate_vaults: false,
        }
    }

    pub fn orientation(mut self, orientation: Orientation) -> Self {
        self.orientation = orientation;
        self
    }
    pub fn owner(mut self, owner: Pubkey) -> Self {
        self.owner = owner;
        self
    }
    pub fn length(mut self, length: usize) -> Self {
        self.length = length;
        self
    }
    pub fn wrong_collateral_mint(mut self, mint: Pubkey) -> Self {
        self.wrong_collateral_mint = Some(mint);
        self
    }
    pub fn wrong_quote_mint(mut self, mint: Pubkey) -> Self {
        self.wrong_quote_mint = Some(mint);
        self
    }
    pub fn override_collateral_vault(mut self, vault: Pubkey) -> Self {
        self.override_collateral_vault = Some(vault);
        self
    }
    pub fn duplicate_vaults(mut self) -> Self {
        self.duplicate_vaults = true;
        self
    }
}

/// An SPL token account's bytes: mint, owner, amount, and the state flag.
///
/// Only the four fields the program reads are meaningful; the rest is zero.
/// Written here rather than through spl-token so a test can produce a malformed
/// one on purpose.
fn token_account_bytes(mint: Pubkey, owner: Pubkey, amount: u64) -> Vec<u8> {
    let mut data = vec![0u8; 165];
    data[0..32].copy_from_slice(mint.as_ref());
    data[32..64].copy_from_slice(owner.as_ref());
    data[64..72].copy_from_slice(&amount.to_le_bytes());
    data[108] = 1; // AccountState::Initialized
    data
}

/// Create a pool and its two vaults in the SVM.
///
/// Returns the identities the oracle configuration needs. Deterministic: the
/// vault addresses are derived from the pool address, so a test can predict
/// them without threading return values everywhere.
pub fn create_mock_damm_pool(svm: &mut LiteSVM, spec: &PoolSpec) -> MockPool {
    let collateral_vault = spec.override_collateral_vault.unwrap_or_else(|| {
        Pubkey::find_program_address(&[b"mock_vault_c", spec.pool.as_ref()], &DAMM_PROGRAM).0
    });
    let quote_vault = if spec.duplicate_vaults {
        collateral_vault
    } else {
        spec.override_quote_vault.unwrap_or_else(|| {
            Pubkey::find_program_address(&[b"mock_vault_q", spec.pool.as_ref()], &DAMM_PROGRAM).0
        })
    };

    // --- the pool account ---
    let mut data = vec![0u8; spec.length];
    let mint_at_collateral = spec.wrong_collateral_mint.unwrap_or(spec.collateral_mint);
    let mint_at_quote = spec.wrong_quote_mint.unwrap_or(spec.quote_mint);

    // Only write the fields if the account is long enough to hold them; a
    // deliberately short account is one of the cases under test.
    if spec.length >= POOL_LEN {
        let (mint_a, mint_b, vault_a, vault_b) = match spec.orientation {
            Orientation::CollateralFirst => (
                mint_at_collateral,
                mint_at_quote,
                collateral_vault,
                quote_vault,
            ),
            Orientation::QuoteFirst => (
                mint_at_quote,
                mint_at_collateral,
                quote_vault,
                collateral_vault,
            ),
        };
        data[OFF_MINT_A..OFF_MINT_A + 32].copy_from_slice(mint_a.as_ref());
        data[OFF_MINT_B..OFF_MINT_B + 32].copy_from_slice(mint_b.as_ref());
        data[OFF_VAULT_A..OFF_VAULT_A + 32].copy_from_slice(vault_a.as_ref());
        data[OFF_VAULT_B..OFF_VAULT_B + 32].copy_from_slice(vault_b.as_ref());
    }

    svm.set_account(
        spec.pool,
        SvmAccount {
            lamports: 10_000_000,
            data,
            owner: spec.owner,
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();

    // --- the vaults ---
    // The pool's own authority owns them, which is what a real DAMM pool does.
    let authority = Pubkey::find_program_address(&[spec.pool.as_ref()], &DAMM_PROGRAM).0;
    set_vault(
        svm,
        collateral_vault,
        spec.collateral_mint,
        authority,
        spec.collateral_reserve,
    );
    if !spec.duplicate_vaults {
        set_vault(
            svm,
            quote_vault,
            spec.quote_mint,
            authority,
            spec.quote_reserve,
        );
    }

    MockPool {
        pool: spec.pool,
        collateral_vault,
        quote_vault,
        collateral_mint: spec.collateral_mint,
        quote_mint: spec.quote_mint,
        orientation: spec.orientation,
    }
}

fn set_vault(svm: &mut LiteSVM, address: Pubkey, mint: Pubkey, owner: Pubkey, amount: u64) {
    svm.set_account(
        address,
        SvmAccount {
            lamports: 2_039_280,
            data: token_account_bytes(mint, owner, amount),
            owner: anchor_spl::token::ID,
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();
}

/// Change a pool's reserves without rebuilding it.
///
/// This is how a test moves the market: a pump, a crash, or liquidity leaving.
pub fn set_pool_reserves(
    svm: &mut LiteSVM,
    pool: &MockPool,
    collateral_reserve: u64,
    quote_reserve: u64,
) {
    let authority = Pubkey::find_program_address(&[pool.pool.as_ref()], &DAMM_PROGRAM).0;
    set_vault(
        svm,
        pool.collateral_vault,
        pool.collateral_mint,
        authority,
        collateral_reserve,
    );
    set_vault(
        svm,
        pool.quote_vault,
        pool.quote_mint,
        authority,
        quote_reserve,
    );
}

/// Move a pool's price by a percentage, holding the constant product.
///
/// A real trade moves both sides: buying the collateral takes it out and puts
/// quote in. Scaling one side alone would produce a price no AMM could be in,
/// and would make a manipulation look cheaper than it is.
pub fn move_pool_price_pct(
    svm: &mut LiteSVM,
    pool: &MockPool,
    collateral: u64,
    quote: u64,
    pct: i64,
) {
    // p' = p * (1 + pct/100); with k fixed, quote' = quote * sqrt(1+r) and
    // collateral' = collateral / sqrt(1+r). Integer sqrt via f64 is fine here:
    // this is test scaffolding setting up a world, not protocol arithmetic.
    let ratio = 1.0 + (pct as f64) / 100.0;
    let root = ratio.sqrt();
    let new_quote = ((quote as f64) * root) as u64;
    let new_collateral = ((collateral as f64) / root) as u64;
    set_pool_reserves(svm, pool, new_collateral.max(1), new_quote.max(1));
}

/// The AMM's `ProgramData`, so the deployment pin can be checked or broken.
pub fn set_amm_program_data(svm: &mut LiteSVM, address: Pubkey, deploy_slot: u64) {
    // UpgradeableLoaderState::ProgramData: tag(4) + slot(8) + Option<authority>
    let mut data = vec![0u8; 45];
    data[0..4].copy_from_slice(&3u32.to_le_bytes());
    data[4..12].copy_from_slice(&deploy_slot.to_le_bytes());
    data[12] = 0; // no upgrade authority recorded in the mock
    svm.set_account(
        address,
        SvmAccount {
            lamports: 1_000_000_000,
            data,
            owner: anchor_lang::solana_program::bpf_loader_upgradeable::ID,
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();
}

/// The price a pool implies, 1e18-scaled, as the program would compute it.
///
/// Mirrors the program's formula so a test can assert on an expected value
/// without reimplementing the decimal normalisation inline each time.
pub fn expected_price(
    collateral_reserve: u64,
    quote_reserve: u64,
    collateral_decimals: u8,
    quote_decimals: u8,
) -> u128 {
    const SCALE: u128 = 1_000_000_000_000_000_000;

    /*
     * The two powers of ten cancel to one factor, applied to whichever side
     * keeps both within u128.
     *
     * The obvious form -- `quote_reserve * 10^cd * SCALE / (collateral_reserve
     * * 10^qd)` -- overflows at any realistic reserve, and this mirror
     * originally had that bug too, which is exactly why it did not catch the
     * same bug in the program. Keep the two in step.
     */
    let (cd, qd) = (collateral_decimals as u32, quote_decimals as u32);
    let mut numerator = (quote_reserve as u128) * SCALE;
    let mut denominator = collateral_reserve as u128;
    if cd >= qd {
        numerator *= 10u128.pow(cd - qd);
    } else {
        denominator *= 10u128.pow(qd - cd);
    }
    numerator / denominator
}
