//! STEAL-044/045/049, 060-065, 080-084 — account substitution.
//!
//! The other way a vault dies. Every one of these builds a *raw* instruction
//! with one account swapped for something the attacker controls, then checks the
//! program refuses and the vault is untouched.
//!
//! These do not go through the SDK on purpose. The SDK addresses accounts
//! correctly by construction, so testing through it would only prove the SDK
//! agrees with itself. The question here is whether the **program** validates
//! what it is handed, because that is all a hostile client has to respect.

mod common;

use anchor_lang::solana_program::instruction::Instruction;
use anchor_lang::{InstructionData, ToAccountMetas};
use common::audit::*;
use common::*;
use solana_keypair::Keypair;

fn actor(env: &mut Env, mint: Pubkey, amount: u64) -> Keypair {
    let user = env.create_user();
    env.fund(&user, mint, amount);
    user
}

/// A funded vault with one honest supplier, so there is something to steal.
fn seeded() -> (Env, ReserveHandle, ReserveHandle, Keypair) {
    let (mut env, cook, bcook) = Env::core(1_000);
    let supplier = actor(&mut env, cook.mint, tokens(100_000));
    env.supply(&supplier, &cook, tokens(100_000));
    (env, cook, bcook, supplier)
}

// ---------------------------------------------------------------------------
// STEAL-045/049 — point the vault at an account the attacker owns
// ---------------------------------------------------------------------------

/// The vault is a `has_one` on the reserve, so a substituted vault must fail.
///
/// If it did not, withdraw would pay out of an account the attacker filled with
/// nothing, and the real vault would never be debited.
#[test]
fn steal_045_substituting_the_liquidity_vault_is_refused() {
    let (mut env, cook, bcook, _supplier) = seeded();
    let attacker = actor(&mut env, cook.mint, tokens(10));
    env.supply(&attacker, &cook, tokens(10));

    let attacker_vault = ata(&attacker.pubkey(), &cook.mint);
    let before = env.solvency(&cook);

    let instruction = Instruction {
        program_id: aera::id(),
        accounts: aera::accounts::Withdraw {
            global: env.global,
            reserve: cook.reserve,
            liquidity_mint: cook.mint,
            // The lie: pay me out of my own account, not the vault.
            liquidity_vault: attacker_vault,
            share_mint: cook.share_mint,
            user_liquidity: ata(&attacker.pubkey(), &cook.mint),
            user_share: share_ata(&attacker.pubkey(), &cook.share_mint),
            supply_position: env.supply_position_address(&cook, attacker.pubkey()),
            owner: attacker.pubkey(),
            liquidity_token_program: TOKEN_PROGRAM_ID,
            share_token_program: TOKEN_2022_PROGRAM_ID,
            system_program: anchor_lang::system_program::ID,
        }
        .to_account_metas(None),
        data: aera::instruction::Withdraw {
            share_amount: tokens(10),
        }
        .data(),
    };

    let result = env.send_raw(vec![env.accrue_ix(&cook), instruction], &[&attacker]);
    assert!(
        result.is_err(),
        "a substituted liquidity vault was accepted — the vault can be bypassed"
    );

    let after = env.solvency(&cook);
    assert_eq!(before.vault_tokens, after.vault_tokens, "vault moved");
    assert_solvent(&env, &cook, "STEAL-045");
    let _ = bcook;
}

/// Pay the withdrawal into someone else's account.
///
/// The program lets a depositor name where their own funds go, so this is not
/// itself theft - it is a check that the *share* burn is tied to the signer, not
/// to whoever the destination belongs to.
#[test]
fn steal_065_withdrawing_to_another_account_still_burns_the_signers_shares() {
    let (mut env, cook, bcook, supplier) = seeded();
    let attacker = actor(&mut env, cook.mint, tokens(10));
    env.supply(&attacker, &cook, tokens(10));

    let victim_shares_before = env.balance(&share_ata(&supplier.pubkey(), &cook.share_mint));
    let attacker_shares_before = env.balance(&share_ata(&attacker.pubkey(), &cook.share_mint));

    // Attacker withdraws, but points `user_share` at the victim's account so the
    // victim's shares are burned instead of theirs.
    let instruction = Instruction {
        program_id: aera::id(),
        accounts: aera::accounts::Withdraw {
            global: env.global,
            reserve: cook.reserve,
            liquidity_mint: cook.mint,
            liquidity_vault: cook.liquidity_vault,
            share_mint: cook.share_mint,
            user_liquidity: ata(&attacker.pubkey(), &cook.mint),
            // The lie: burn the victim's shares, pay me.
            user_share: share_ata(&supplier.pubkey(), &cook.share_mint),
            supply_position: env.supply_position_address(&cook, attacker.pubkey()),
            owner: attacker.pubkey(),
            liquidity_token_program: TOKEN_PROGRAM_ID,
            share_token_program: TOKEN_2022_PROGRAM_ID,
            system_program: anchor_lang::system_program::ID,
        }
        .to_account_metas(None),
        data: aera::instruction::Withdraw {
            share_amount: tokens(1_000),
        }
        .data(),
    };

    let result = env.send_raw(vec![env.accrue_ix(&cook), instruction], &[&attacker]);
    assert!(
        result.is_err(),
        "burned another account's shares — the burn authority is not the signer"
    );

    assert_eq!(
        env.balance(&share_ata(&supplier.pubkey(), &cook.share_mint)),
        victim_shares_before,
        "victim shares were burned"
    );
    assert_eq!(
        env.balance(&share_ata(&attacker.pubkey(), &cook.share_mint)),
        attacker_shares_before
    );
    assert_solvent(&env, &cook, "STEAL-065");
    let _ = bcook;
}

// ---------------------------------------------------------------------------
// STEAL-062/063 — right seeds, wrong mint; right mint, wrong owner
// ---------------------------------------------------------------------------

/// Supply COOK but claim the bCOOK reserve, so bCOOK shares mint for COOK.
#[test]
fn steal_081_swapping_the_reserve_for_the_other_one_is_refused() {
    let (mut env, cook, bcook, _supplier) = seeded();
    let attacker = actor(&mut env, cook.mint, tokens(1_000));
    env.ensure_share_ata(&attacker, bcook.share_mint);

    let instruction = Instruction {
        program_id: aera::id(),
        accounts: aera::accounts::Supply {
            global: env.global,
            // The lie: the bCOOK reserve, but every token account is COOK's.
            reserve: bcook.reserve,
            liquidity_mint: cook.mint,
            liquidity_vault: cook.liquidity_vault,
            share_mint: bcook.share_mint,
            user_liquidity: ata(&attacker.pubkey(), &cook.mint),
            user_share: share_ata(&attacker.pubkey(), &bcook.share_mint),
            supply_position: env.supply_position_address(&bcook, attacker.pubkey()),
            owner: attacker.pubkey(),
            liquidity_token_program: TOKEN_PROGRAM_ID,
            share_token_program: TOKEN_2022_PROGRAM_ID,
            system_program: anchor_lang::system_program::ID,
        }
        .to_account_metas(None),
        data: aera::instruction::Supply {
            liquidity_amount: tokens(1_000),
        }
        .data(),
    };

    let result = env.send_raw(vec![env.accrue_ix(&bcook), instruction], &[&attacker]);
    assert!(
        result.is_err(),
        "minted bCOOK shares by supplying COOK — the reserve's has_one bindings do not hold"
    );

    assert_solvent(&env, &cook, "STEAL-081 cook");
    assert_solvent(&env, &bcook, "STEAL-081 bcook");
}

/// A share mint that is not this reserve's must be refused.
///
/// If it were not, a supplier could mint abCOOK - the collateral share token -
/// by depositing COOK, and then post it as collateral.
#[test]
fn steal_062_a_foreign_share_mint_is_refused() {
    let (mut env, cook, bcook, _supplier) = seeded();
    let attacker = actor(&mut env, cook.mint, tokens(1_000));
    env.ensure_share_ata(&attacker, bcook.share_mint);

    let instruction = Instruction {
        program_id: aera::id(),
        accounts: aera::accounts::Supply {
            global: env.global,
            reserve: cook.reserve,
            liquidity_mint: cook.mint,
            liquidity_vault: cook.liquidity_vault,
            // The lie: pay me in the collateral share token.
            share_mint: bcook.share_mint,
            user_liquidity: ata(&attacker.pubkey(), &cook.mint),
            user_share: share_ata(&attacker.pubkey(), &bcook.share_mint),
            supply_position: env.supply_position_address(&cook, attacker.pubkey()),
            owner: attacker.pubkey(),
            liquidity_token_program: TOKEN_PROGRAM_ID,
            share_token_program: TOKEN_2022_PROGRAM_ID,
            system_program: anchor_lang::system_program::ID,
        }
        .to_account_metas(None),
        data: aera::instruction::Supply {
            liquidity_amount: tokens(1_000),
        }
        .data(),
    };

    let result = env.send_raw(vec![env.accrue_ix(&cook), instruction], &[&attacker]);
    assert!(
        result.is_err(),
        "minted the collateral share token by supplying COOK"
    );
    assert_solvent(&env, &cook, "STEAL-062");
    let _ = bcook;
}

// ---------------------------------------------------------------------------
// STEAL-083 — a Global from somewhere else
// ---------------------------------------------------------------------------

/// A forged Global with `paused = false` must not unpause a paused protocol.
#[test]
fn steal_083_a_foreign_global_cannot_unpause_the_protocol() {
    let (mut env, cook, bcook, _supplier) = seeded();
    let attacker = actor(&mut env, cook.mint, tokens(1_000));

    env.pause_all();

    // Copy the real Global's bytes into an account the attacker owns, with the
    // pause flag cleared, and pass that instead.
    let forged = env.clone_global_with_pause_cleared();

    let instruction = Instruction {
        program_id: aera::id(),
        accounts: aera::accounts::Supply {
            global: forged,
            reserve: cook.reserve,
            liquidity_mint: cook.mint,
            liquidity_vault: cook.liquidity_vault,
            share_mint: cook.share_mint,
            user_liquidity: ata(&attacker.pubkey(), &cook.mint),
            user_share: share_ata(&attacker.pubkey(), &cook.share_mint),
            supply_position: env.supply_position_address(&cook, attacker.pubkey()),
            owner: attacker.pubkey(),
            liquidity_token_program: TOKEN_PROGRAM_ID,
            share_token_program: TOKEN_2022_PROGRAM_ID,
            system_program: anchor_lang::system_program::ID,
        }
        .to_account_metas(None),
        data: aera::instruction::Supply {
            liquidity_amount: tokens(1_000),
        }
        .data(),
    };

    let result = env.send_raw(vec![env.accrue_ix(&cook), instruction], &[&attacker]);
    assert!(
        result.is_err(),
        "a forged Global bypassed the pause — Global is not bound to the market"
    );
    assert_solvent(&env, &cook, "STEAL-083");
    let _ = bcook;
}

// ---------------------------------------------------------------------------
// STEAL-066/067 — the share mint's own authorities
// ---------------------------------------------------------------------------

/// aCOOK must have no freeze authority, and its mint authority must be the
/// reserve PDA.
///
/// A freeze authority on a receipt token would let whoever holds it strand every
/// supplier's claim without touching the protocol at all.
#[test]
fn steal_066_acook_has_no_freeze_authority_and_the_reserve_mints_it() {
    let (env, cook, bcook, _supplier) = seeded();

    for handle in [&cook, &bcook] {
        let (mint_authority, freeze_authority) = env.mint_authorities(&handle.share_mint);
        assert_eq!(
            mint_authority,
            Some(handle.reserve),
            "share mint authority is not the reserve PDA"
        );
        assert_eq!(
            freeze_authority, None,
            "share mint has a freeze authority — supplier claims can be frozen"
        );
    }
}
