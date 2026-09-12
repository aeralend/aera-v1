use anchor_lang::prelude::*;
use anchor_lang::system_program::{create_account, CreateAccount};
use anchor_spl::token_2022::spl_token_2022::extension::ExtensionType;
use anchor_spl::token_2022::spl_token_2022::state::Mint as Token2022Mint;
use anchor_spl::token_2022::{initialize_mint2, InitializeMint2, Token2022};
use anchor_spl::token_2022_extensions::{
    metadata_pointer_initialize, token_metadata_initialize, MetadataPointerInitialize,
    TokenMetadataInitialize,
};
use anchor_spl::token_interface::{Mint, TokenAccount, TokenInterface};
use spl_pod::optional_keys::OptionalNonZeroPubkey;
use spl_token_metadata_interface::state::TokenMetadata;

use crate::constants::{
    FIXED_POINT_SCALE, LIQUIDITY_VAULT_SEED, MAX_SHARE_NAME, MAX_SHARE_SYMBOL, MAX_SHARE_URI,
    ORACLE_SEED, RESERVE_SEED, SHARE_MINT_SEED,
};
use crate::errors::AeraError;
use crate::state::{
    reserve_signer_seeds, Global, Market, OracleState, PendingConfig, Reserve, ReserveConfig,
};

/// Name, symbol and URI for a reserve's share token.
///
/// Passed in rather than derived so the COOK reserve can mint "Aera COOK"
/// (`aCOOK`) and a second reserve can carry its own identity, without the
/// program shipping a table of strings it would have to be upgraded to change.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct ShareMetadata {
    pub name: String,
    pub symbol: String,
    pub uri: String,
}

impl ShareMetadata {
    pub(crate) fn validate(&self) -> Result<()> {
        require!(
            !self.name.is_empty() && self.name.len() <= MAX_SHARE_NAME,
            AeraError::InvalidConfig
        );
        require!(
            !self.symbol.is_empty() && self.symbol.len() <= MAX_SHARE_SYMBOL,
            AeraError::InvalidConfig
        );
        require!(self.uri.len() <= MAX_SHARE_URI, AeraError::InvalidConfig);
        Ok(())
    }
}

/// Create a reserve, its liquidity vault, and its share mint.
///
/// The share mint is **Token-2022** carrying MetadataPointer (aimed at itself)
/// and TokenMetadata, so aCOOK arrives with a name, symbol and URI rather than
/// as an anonymous mint a wallet renders as its own address. Both the mint
/// authority and the metadata update authority are the reserve PDA — no wallet
/// key can mint aCOOK or rewrite what it claims to be, and there is no premint.
///
/// Freeze authority is deliberately `None`. A freeze authority on a receipt
/// token would let the protocol strand a supplier's claim, which is the one
/// thing a receipt must never allow.
///
/// The liquidity mint is whatever the asset already is — wCOOK and bCOOK are
/// both legacy SPL Token on Cookie — so this instruction carries two token
/// programs and does not assume they are the same.
pub fn handle_init_reserve(
    context: Context<InitReserve>,
    config: ReserveConfig,
    metadata: ShareMetadata,
) -> Result<()> {
    config.validate()?;
    metadata.validate()?;

    let reserve_key = context.accounts.reserve.key();
    let mint_key = context.accounts.share_mint.key();
    let decimals = context.accounts.liquidity_mint.decimals;

    // --- size the mint ---
    //
    // The mint is created holding only the MetadataPointer extension; the
    // TokenMetadata TLV is appended by `token_metadata_initialize`, which
    // reallocs. That realloc cannot add lamports, so the account is funded for
    // the final size up front and left rent-exempt afterwards.
    let base_len = ExtensionType::try_calculate_account_len::<Token2022Mint>(&[
        ExtensionType::MetadataPointer,
    ])
    .map_err(|_| AeraError::MathOverflow)?;

    let metadata_len = TokenMetadata {
        update_authority: OptionalNonZeroPubkey::try_from(Some(reserve_key))
            .map_err(|_| AeraError::InvalidConfig)?,
        mint: mint_key,
        name: metadata.name.clone(),
        symbol: metadata.symbol.clone(),
        uri: metadata.uri.clone(),
        additional_metadata: Vec::new(),
    }
    .tlv_size_of()
    .map_err(|_| AeraError::MathOverflow)?;

    let funded_len = base_len
        .checked_add(metadata_len)
        .ok_or(AeraError::MathOverflow)?;
    let lamports = Rent::get()?.minimum_balance(funded_len);

    let mint_bump = [context.bumps.share_mint];
    let mint_seeds: [&[u8]; 3] = [SHARE_MINT_SEED, reserve_key.as_ref(), &mint_bump];

    create_account(
        CpiContext::new_with_signer(
            context.accounts.system_program.key(),
            CreateAccount {
                from: context.accounts.admin.to_account_info(),
                to: context.accounts.share_mint.to_account_info(),
            },
            &[&mint_seeds],
        ),
        lamports,
        base_len as u64,
        &context.accounts.share_token_program.key(),
    )?;

    // --- extensions, then the mint, then the metadata ---
    //
    // Order is not negotiable: extensions must be initialised on an allocated
    // but uninitialised mint, and `initialize_mint2` must run before the
    // metadata that points at it.
    metadata_pointer_initialize(
        CpiContext::new(
            context.accounts.share_token_program.key(),
            MetadataPointerInitialize {
                token_program_id: context.accounts.share_token_program.to_account_info(),
                mint: context.accounts.share_mint.to_account_info(),
            },
        ),
        Some(reserve_key),
        // The mint points at itself: the metadata lives in the mint account, so
        // there is no second account that could be swapped for a forgery.
        Some(mint_key),
    )?;

    initialize_mint2(
        CpiContext::new(
            context.accounts.share_token_program.key(),
            InitializeMint2 {
                mint: context.accounts.share_mint.to_account_info(),
            },
        ),
        decimals,
        &reserve_key,
        // No freeze authority, ever. See the doc comment.
        None,
    )?;

    let reserve_bump = [context.bumps.reserve];
    let market_key = context.accounts.market.key();
    let liquidity_mint_key = context.accounts.liquidity_mint.key();
    let reserve_seeds = reserve_signer_seeds(&market_key, &liquidity_mint_key, &reserve_bump);

    token_metadata_initialize(
        CpiContext::new_with_signer(
            context.accounts.share_token_program.key(),
            TokenMetadataInitialize {
                program_id: context.accounts.share_token_program.to_account_info(),
                metadata: context.accounts.share_mint.to_account_info(),
                update_authority: context.accounts.reserve.to_account_info(),
                mint_authority: context.accounts.reserve.to_account_info(),
                mint: context.accounts.share_mint.to_account_info(),
            },
            &[&reserve_seeds],
        ),
        metadata.name.clone(),
        metadata.symbol.clone(),
        metadata.uri.clone(),
    )?;

    // --- reserve state ---
    let reserve = &mut context.accounts.reserve;
    reserve.market = market_key;
    reserve.liquidity_mint = liquidity_mint_key;
    reserve.liquidity_vault = context.accounts.liquidity_vault.key();
    reserve.share_mint = mint_key;
    reserve.oracle = context.accounts.oracle.key();
    reserve.liquidity_decimals = decimals;
    reserve.available_liquidity = 0;
    reserve.share_mint_supply = 0;
    reserve.borrowed_principal = 0;
    reserve.borrow_index = FIXED_POINT_SCALE;
    reserve.last_update_slot = Clock::get()?.slot;
    reserve.accrued_fees = 0;
    reserve.config = config;
    reserve.pending = PendingConfig::default();
    reserve.bump = context.bumps.reserve;

    emit!(ReserveCreated {
        reserve: reserve_key,
        liquidity_mint: liquidity_mint_key,
        share_mint: mint_key,
        symbol: metadata.symbol,
    });
    Ok(())
}

#[event]
pub struct ReserveCreated {
    pub reserve: Pubkey,
    pub liquidity_mint: Pubkey,
    pub share_mint: Pubkey,
    pub symbol: String,
}

// `Reserve` and `PriceFeed` are both large enough that deserializing them onto
// the BPF stack overflows the frame, so every account here is boxed onto the
// heap. Without this the handler fails with an access violation before it runs.
#[derive(Accounts)]
pub struct InitReserve<'info> {
    #[account(has_one = admin @ AeraError::NotAdmin)]
    pub global: Box<Account<'info, Global>>,

    #[account(mut)]
    pub admin: Signer<'info>,

    // The reserve PDA is seeded by this market, so the market is pinned by the
    // seed; we only prove the signer is the protocol admin.
    #[account(has_one = global @ AeraError::GlobalMismatch)]
    pub market: Box<Account<'info, Market>>,

    #[account(
        init,
        payer = admin,
        space = Reserve::DISCRIMINATOR.len() + Reserve::INIT_SPACE,
        seeds = [RESERVE_SEED, market.key().as_ref(), liquidity_mint.key().as_ref()],
        bump,
    )]
    pub reserve: Box<Account<'info, Reserve>>,

    pub liquidity_mint: Box<InterfaceAccount<'info, Mint>>,

    #[account(
        init,
        payer = admin,
        token::mint = liquidity_mint,
        token::authority = reserve,
        token::token_program = liquidity_token_program,
        seeds = [LIQUIDITY_VAULT_SEED, reserve.key().as_ref()],
        bump,
    )]
    pub liquidity_vault: Box<InterfaceAccount<'info, TokenAccount>>,

    /// The share token: aCOOK for the COOK reserve.
    ///
    /// Unchecked and uninitialised here because Anchor's `init` for a mint
    /// allocates exactly a bare mint, with no room for extensions. The handler
    /// creates it, sizes it for MetadataPointer + TokenMetadata, and
    /// initialises all three in order.
    ///
    /// CHECK: created by this handler at a PDA it derives, then handed to
    /// Token-2022 to initialise. It is never read as an account of any type.
    #[account(
        mut,
        seeds = [SHARE_MINT_SEED, reserve.key().as_ref()],
        bump,
    )]
    pub share_mint: UncheckedAccount<'info>,

    // Bound by seeds to this market's oracle for this mint — a reserve can only
    // trust the price its own market derives.
    #[account(
        seeds = [ORACLE_SEED, market.key().as_ref(), liquidity_mint.key().as_ref()],
        bump = oracle.bump,
    )]
    pub oracle: Box<Account<'info, OracleState>>,

    /// Whatever program owns the liquidity mint. wCOOK and bCOOK are both
    /// legacy SPL Token on Cookie; this does not assume it.
    pub liquidity_token_program: Interface<'info, TokenInterface>,

    /// Always Token-2022: the share mint carries extensions that legacy SPL
    /// Token cannot hold.
    pub share_token_program: Program<'info, Token2022>,

    pub system_program: Program<'info, System>,
}
