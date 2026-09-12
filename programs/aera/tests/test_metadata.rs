//! aCOOK's mint: Token-2022, self-pointing metadata, PDA authorities, no premint.
//!
//! These assert the properties a holder actually depends on. A receipt token
//! whose mint authority is a wallet, or which can be frozen, or which arrives
//! with a premined balance, is a different and much worse instrument than the
//! one PARAMS.md describes — and none of that is visible from the lending math.

mod common;

use common::*;
use spl_token_2022_interface::extension::metadata_pointer::MetadataPointer;
use spl_token_2022_interface::extension::{BaseStateWithExtensions, StateWithExtensions};
use spl_token_2022_interface::state::Mint as Token2022Mint;
use spl_token_metadata_interface::state::TokenMetadata;

/// Parse a Token-2022 mint account, extensions included.
fn read_mint(env: &Env, mint: Pubkey) -> (Token2022Mint, Vec<u8>) {
    let account = env.svm.get_account(&mint).expect("mint account missing");
    let state =
        StateWithExtensions::<Token2022Mint>::unpack(&account.data).expect("not a Token-2022 mint");
    (state.base, account.data.clone())
}

#[test]
fn share_mint_is_owned_by_token_2022() {
    let (env, cook, _) = Env::core(1_000);
    let account = env.svm.get_account(&cook.share_mint).unwrap();
    assert_eq!(
        account.owner, TOKEN_2022_PROGRAM_ID,
        "aCOOK must be Token-2022; legacy SPL cannot carry metadata extensions"
    );
}

#[test]
fn mint_authority_is_the_reserve_pda() {
    let (env, cook, _) = Env::core(1_000);
    let (mint, _) = read_mint(&env, cook.share_mint);

    let authority: Option<Pubkey> = mint.mint_authority.into();
    assert_eq!(
        authority,
        Some(cook.reserve),
        "only the reserve PDA may mint aCOOK — no wallet key, ever"
    );
}

#[test]
fn there_is_no_freeze_authority() {
    let (env, cook, _) = Env::core(1_000);
    let (mint, _) = read_mint(&env, cook.share_mint);

    let freeze: Option<Pubkey> = mint.freeze_authority.into();
    assert_eq!(
        freeze, None,
        "a freeze authority would let the protocol strand a supplier's claim"
    );
}

#[test]
fn nothing_is_preminted() {
    let (env, cook, _) = Env::core(1_000);
    let (mint, _) = read_mint(&env, cook.share_mint);
    assert_eq!(mint.supply, 0, "aCOOK exists only against supplied COOK");
}

#[test]
fn share_mint_decimals_match_the_liquidity_mint() {
    let (env, cook, bcook) = Env::core(1_000);
    for handle in [cook, bcook] {
        let (mint, _) = read_mint(&env, handle.share_mint);
        assert_eq!(
            mint.decimals, handle.decimals,
            "a share token with different decimals from its asset misreads every balance"
        );
    }
}

#[test]
fn metadata_pointer_aims_at_the_mint_itself() {
    let (env, cook, _) = Env::core(1_000);
    let account = env.svm.get_account(&cook.share_mint).unwrap();
    let state = StateWithExtensions::<Token2022Mint>::unpack(&account.data).unwrap();

    let pointer = state
        .get_extension::<MetadataPointer>()
        .expect("MetadataPointer extension missing");

    let target: Option<Pubkey> = pointer.metadata_address.into();
    assert_eq!(
        target,
        Some(cook.share_mint),
        "the metadata lives in the mint, so there is no second account to forge"
    );

    let authority: Option<Pubkey> = pointer.authority.into();
    assert_eq!(authority, Some(cook.reserve));
}

#[test]
fn token_metadata_is_present_and_owned_by_the_pda() {
    let (env, cook, _) = Env::core(1_000);
    let account = env.svm.get_account(&cook.share_mint).unwrap();
    let state = StateWithExtensions::<Token2022Mint>::unpack(&account.data).unwrap();

    let metadata = state
        .get_variable_len_extension::<TokenMetadata>()
        .expect("TokenMetadata extension missing");

    assert_eq!(metadata.mint, cook.share_mint);
    assert!(
        !metadata.name.is_empty(),
        "a nameless receipt is an anonymous mint"
    );
    assert!(!metadata.symbol.is_empty());
    assert_eq!(metadata.uri, "https://aera.io/acook.json");

    let update_authority: Option<Pubkey> = metadata.update_authority.into();
    assert_eq!(
        update_authority,
        Some(cook.reserve),
        "no user key may rewrite what aCOOK claims to be"
    );
}

/// Supplying mints against the mint; the on-chain supply and the reserve's
/// mirror of it must agree.
#[test]
fn supply_mints_and_the_mirror_agrees() {
    let (mut env, cook, _) = Env::core(1_000);

    let user = env.create_user();
    env.fund(&user, cook.mint, tokens(1_000));
    env.supply(&user, &cook, tokens(400));

    let (mint, _) = read_mint(&env, cook.share_mint);
    let reserve = env.read_reserve(&cook);
    assert_eq!(mint.supply, tokens(400));
    assert_eq!(reserve.share_mint_supply, mint.supply);
}

/// Withdrawing burns. The mint's supply must fall with it, not just the mirror.
#[test]
fn withdraw_burns_and_the_mirror_agrees() {
    let (mut env, cook, _) = Env::core(1_000);

    let user = env.create_user();
    env.fund(&user, cook.mint, tokens(1_000));
    env.supply(&user, &cook, tokens(400));
    env.try_withdraw(&user, &cook, tokens(150)).unwrap();

    let (mint, _) = read_mint(&env, cook.share_mint);
    let reserve = env.read_reserve(&cook);
    assert_eq!(mint.supply, tokens(250));
    assert_eq!(reserve.share_mint_supply, mint.supply);
}

/// The two reserves get distinct share mints. Sharing one would let bCOOK
/// collateral be redeemed against the COOK vault.
#[test]
fn each_reserve_has_its_own_share_mint() {
    let (_env, cook, bcook) = Env::core(1_000);
    assert_ne!(cook.share_mint, bcook.share_mint);
}

// ---------------------------------------------------------------------------
// set_share_metadata
//
// The URI written at `init_reserve` used to be permanent: its update authority
// is the reserve PDA, so only this program can sign a change, and no
// instruction did. Both deployed mints therefore named a domain Aera does not
// own, which answers 200 with a suspended-account page -- a wallet asking aCOOK
// what it is received HTML where JSON should be.
//
// These cover the two questions that matter about the instruction that fixes
// it: does it actually rewrite the field, and can anyone but the admin reach it.
// ---------------------------------------------------------------------------

#[test]
fn the_admin_can_rewrite_the_share_metadata() {
    let (mut env, cook, _) = Env::core(1_000);

    let (name, symbol, before) = env.share_metadata(&cook);
    assert_ne!(
        before, "https://aeralend.app/acook.json",
        "nothing to prove otherwise"
    );

    env.try_set_share_metadata(
        &cook,
        &env.admin.insecure_clone(),
        &name,
        &symbol,
        "https://aeralend.app/acook.json",
    )
    .expect("the admin may rewrite metadata");

    let (after_name, after_symbol, after_uri) = env.share_metadata(&cook);
    assert_eq!(
        after_uri, "https://aeralend.app/acook.json",
        "the URI moved"
    );
    assert_eq!(after_name, name, "the name was not disturbed");
    assert_eq!(after_symbol, symbol, "the symbol was not disturbed");
}

/// A longer URI needs a bigger mint account, and Token-2022 will not fund it.
///
/// The handler transfers the shortfall before it writes. Without that the CPI
/// fails on rent exemption -- and the failure would land on exactly the change
/// this instruction exists to make, since `aeralend.app` is longer than the
/// `aera.io` it replaces.
#[test]
fn a_longer_uri_grows_the_mint_and_stays_rent_exempt() {
    let (mut env, cook, _) = Env::core(1_000);

    let long = format!("https://aeralend.app/{}.json", "a".repeat(80));
    let (name, symbol, _) = env.share_metadata(&cook);
    env.try_set_share_metadata(&cook, &env.admin.insecure_clone(), &name, &symbol, &long)
        .expect("a longer URI is allowed");

    let (_, _, uri) = env.share_metadata(&cook);
    assert_eq!(uri, long, "the long URI was stored in full");

    let account = env.svm.get_account(&cook.share_mint).expect("share mint");
    let rent = env
        .svm
        .minimum_balance_for_rent_exemption(account.data.len());
    assert!(
        account.lamports >= rent,
        "the mint must still be rent exempt: {} lamports for {} bytes, needs {}",
        account.lamports,
        account.data.len(),
        rent,
    );
}

#[test]
fn a_stranger_cannot_rewrite_the_share_metadata() {
    let (mut env, cook, _) = Env::core(1_000);
    let stranger = env.create_user();

    let error = env
        .try_set_share_metadata(
            &cook,
            &stranger,
            "Not Aera",
            "SCAM",
            "https://example.invalid/steal.json",
        )
        .expect_err("only the admin may rewrite metadata");
    assert!(
        error.contains("NotAdmin") || error.contains("2001") || error.contains("has_one"),
        "expected an admin refusal, got: {error}"
    );

    let (_, symbol, _) = env.share_metadata(&cook);
    assert_ne!(symbol, "SCAM", "the stranger changed nothing");
}
