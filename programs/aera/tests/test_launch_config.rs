//! One launch configuration, emitted from the program and checked against the
//! file every other tool reads.
//!
//! The same figures used to be written out in four places -- the program's
//! constants, `sdk/src/params.ts`, the deployment scripts and the keeper
//! configuration -- and they had already drifted. The SDK carried
//! `slots_per_year = 67_609_680` while the program carried `70_881_876`, a 4.8%
//! disagreement in the divisor that turns an APR into a per-slot rate. Since
//! `init_reserve` takes its configuration from whoever calls it, the SDK's copy
//! was what a deployment would actually have written on chain.
//!
//! So: the program is the source, `config/aera.launch.json` is generated from
//! it, and this file fails if the committed JSON has drifted from the constants
//! it claims to mirror.
//!
//! ```text
//!   AERA_WRITE_LAUNCH_CONFIG=1 cargo test --test test_launch_config
//! ```
//!
//! regenerates the file. Without that variable the test only compares, so CI
//! cannot quietly rewrite the thing it is supposed to be checking.

mod common;

use aera::launch::{bcook_reserve_config, chain, cook_reserve_config, measured};
use aera::state::ReserveConfig;
use common::*;
use std::fmt::Write as _;

/// Where the generated file lives.
///
/// Anchored to `CARGO_MANIFEST_DIR` rather than to the working directory:
/// `cargo test` runs from the package root, not the repository root, so a
/// relative path writes the file three levels deeper than intended and the
/// comparison then reads a file nothing else can see.
const CONFIG_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../config/aera.launch.json"
);

/// The same configuration, as a TypeScript module the SDK can import.
///
/// Two outputs rather than one because the SDK's `tsconfig` sets
/// `rootDir: "src"`, so it cannot import a JSON file from outside its own
/// package without either widening that or copying the file in at build time.
/// Both of those introduce a second place the values live. Emitting a typed
/// module from the same generator does not: if either output drifts, the same
/// test fails.
///
/// The JSON stays canonical for everything that is not TypeScript -- the
/// deployment scripts, `preflight.sh`, anything reading it with `jq`.
const TS_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../sdk/src/launch.generated.ts"
);

/// Render a reserve config as JSON.
///
/// Written by hand rather than through serde so the emitted key names are a
/// deliberate contract with the TypeScript that reads them, not whatever a
/// derive happens to produce from Rust field names.
fn reserve_json(config: &ReserveConfig, indent: &str) -> String {
    let mut out = String::new();
    let i = indent;
    writeln!(out, "{{").unwrap();
    writeln!(
        out,
        "{i}  \"loanToValueBps\": {},",
        config.loan_to_value_bps
    )
    .unwrap();
    writeln!(
        out,
        "{i}  \"liquidationThresholdBps\": {},",
        config.liquidation_threshold_bps
    )
    .unwrap();
    writeln!(
        out,
        "{i}  \"liquidationBonusBps\": {},",
        config.liquidation_bonus_bps
    )
    .unwrap();
    writeln!(out, "{i}  \"closeFactorBps\": {},", config.close_factor_bps).unwrap();
    writeln!(
        out,
        "{i}  \"collateralHaircutBps\": {},",
        config.collateral_haircut_bps
    )
    .unwrap();
    writeln!(
        out,
        "{i}  \"optimalUtilizationBps\": {},",
        config.optimal_utilization_bps
    )
    .unwrap();
    writeln!(
        out,
        "{i}  \"minBorrowRateBps\": {},",
        config.min_borrow_rate_bps
    )
    .unwrap();
    writeln!(
        out,
        "{i}  \"optimalBorrowRateBps\": {},",
        config.optimal_borrow_rate_bps
    )
    .unwrap();
    writeln!(
        out,
        "{i}  \"maxBorrowRateBps\": {},",
        config.max_borrow_rate_bps
    )
    .unwrap();
    writeln!(
        out,
        "{i}  \"reserveFactorBps\": {},",
        config.reserve_factor_bps
    )
    .unwrap();
    writeln!(
        out,
        "{i}  \"originationFeeBps\": {},",
        config.origination_fee_bps
    )
    .unwrap();
    writeln!(out, "{i}  \"supplyCap\": \"{}\",", config.supply_cap).unwrap();
    writeln!(out, "{i}  \"borrowCap\": \"{}\",", config.borrow_cap).unwrap();
    writeln!(
        out,
        "{i}  \"perWalletSupplyCap\": \"{}\",",
        config.per_wallet_supply_cap
    )
    .unwrap();
    writeln!(out, "{i}  \"borrowEnabled\": {},", config.borrow_enabled).unwrap();
    writeln!(
        out,
        "{i}  \"collateralEnabled\": {},",
        config.collateral_enabled
    )
    .unwrap();
    writeln!(out, "{i}  \"isolated\": {},", config.isolated).unwrap();
    writeln!(out, "{i}  \"slotsPerYear\": \"{}\"", config.slots_per_year).unwrap();
    write!(out, "{i}}}").unwrap();
    out
}

/// The whole file.
///
/// Caps and `slotsPerYear` are strings because they exceed 2^53 in base units
/// and JavaScript's `JSON.parse` would round them. The SDK parses them with
/// `BigInt`, which is the only correct way to carry a `u64` through JSON.
fn render() -> String {
    let cook = cook_reserve_config();
    let bcook = bcook_reserve_config();

    let mut out = String::new();
    writeln!(out, "{{").unwrap();
    writeln!(
        out,
        "  \"$comment\": \"GENERATED from programs/aera/src/launch.rs by tests/test_launch_config.rs. Do not edit by hand: the test fails if this file and the program's constants disagree. Regenerate with AERA_WRITE_LAUNCH_CONFIG=1 cargo test --test test_launch_config.\","
    )
    .unwrap();
    writeln!(out, "  \"version\": \"0.2\",").unwrap();
    writeln!(out, "  \"programId\": \"{}\",", aera::id()).unwrap();
    writeln!(out, "  \"marketId\": 0,").unwrap();
    writeln!(out, "  \"marketName\": \"Aera Core\",").unwrap();
    writeln!(out).unwrap();

    writeln!(out, "  \"chain\": {{").unwrap();
    writeln!(out, "    \"genesis\": \"{}\",", chain::GENESIS).unwrap();
    writeln!(out, "    \"decimals\": {},", chain::DECIMALS).unwrap();
    writeln!(out, "    \"cookMint\": \"{}\",", chain::WCOOK_MINT).unwrap();
    writeln!(out, "    \"bcookMint\": \"{}\"", chain::BCOOK_MINT).unwrap();
    writeln!(out, "  }},").unwrap();
    writeln!(out).unwrap();

    writeln!(out, "  \"stakePool\": {{").unwrap();
    writeln!(out, "    \"program\": \"{}\",", chain::STAKE_POOL_PROGRAM).unwrap();
    writeln!(
        out,
        "    \"programData\": \"{}\",",
        chain::STAKE_POOL_PROGRAM_DATA
    )
    .unwrap();
    writeln!(
        out,
        "    \"deploySlot\": {},",
        chain::STAKE_POOL_DEPLOY_SLOT
    )
    .unwrap();
    writeln!(
        out,
        "    \"upgradeAuthority\": \"{}\",",
        chain::STAKE_POOL_UPGRADE_AUTHORITY
    )
    .unwrap();
    writeln!(out, "    \"pool\": \"{}\"", chain::STAKE_POOL).unwrap();
    writeln!(out, "  }},").unwrap();
    writeln!(out).unwrap();

    writeln!(out, "  \"oracle\": {{").unwrap();
    writeln!(
        out,
        "    \"maxWithdrawalFeeBps\": {},",
        aera::constants::DEFAULT_MAX_WITHDRAWAL_FEE_BPS
    )
    .unwrap();
    writeln!(
        out,
        "    \"rateFloor\": \"{}\",",
        aera::constants::DEFAULT_RATE_FLOOR
    )
    .unwrap();
    writeln!(
        out,
        "    \"rateCeiling\": \"{}\",",
        aera::constants::DEFAULT_RATE_CEILING
    )
    .unwrap();
    writeln!(
        out,
        "    \"maxUpBpsPerEpoch\": {},",
        aera::constants::DEFAULT_MAX_UP_BPS_PER_EPOCH
    )
    .unwrap();
    writeln!(
        out,
        "    \"maxDownBpsPerEpoch\": {},",
        aera::constants::DEFAULT_MAX_DOWN_BPS_PER_EPOCH
    )
    .unwrap();
    writeln!(
        out,
        "    \"emergencyDeviationBps\": {},",
        aera::constants::DEFAULT_EMERGENCY_DEVIATION_BPS
    )
    .unwrap();
    writeln!(
        out,
        "    \"staleEpochsWarning\": {}",
        aera::oracle::breaker::STALE_EPOCHS_WARNING
    )
    .unwrap();
    writeln!(out, "  }},").unwrap();
    writeln!(out).unwrap();

    writeln!(out, "  \"reserves\": {{").unwrap();
    writeln!(out, "    \"cook\": {},", reserve_json(&cook, "    ")).unwrap();
    writeln!(out, "    \"bcook\": {}", reserve_json(&bcook, "    ")).unwrap();
    writeln!(out, "  }},").unwrap();
    writeln!(out).unwrap();

    writeln!(out, "  \"slotTime\": {{").unwrap();
    writeln!(
        out,
        "    \"$comment\": \"Measured, not applied. slotsPerYear above is what the program deploys; changing it changes the economics of every position and is an operator decision made through set_params with the timelock. See LAUNCH_CHECKLIST.md.\","
    )
    .unwrap();
    writeln!(out, "    \"measuredOn\": \"{}\",", measured::MEASURED_ON).unwrap();
    writeln!(out, "    \"meanSlotMs\": {},", measured::SLOT_MS_MEASURED).unwrap();
    writeln!(
        out,
        "    \"slotsPerYearMeasured\": \"{}\",",
        measured::SLOTS_PER_YEAR_MEASURED
    )
    .unwrap();
    writeln!(
        out,
        "    \"deployedDriftBps\": {}",
        measured::DEPLOYED_DRIFT_BPS
    )
    .unwrap();
    writeln!(out, "  }}").unwrap();
    write!(out, "}}").unwrap();
    out.push('\n');
    out
}

/// The generated TypeScript module.
///
/// A second output rather than one because the SDK's tsconfig sets
/// `rootDir: "src"`, so it cannot import JSON from outside its own package
/// without widening that or copying the file in at build time -- both of which
/// create a second place the values live. Emitting a typed module from the
/// same generator does not: if either output drifts, this same test fails.
///
/// The JSON stays canonical for everything that is not TypeScript: the
/// deployment scripts, preflight, anything reading it with `jq`.
fn render_ts(json: &str) -> String {
    // A raw string, so the emitted file is not indented by this file's own
    // indentation -- which is what a backslash-continued string literal does,
    // and it produced a header indented nine spaces on the first attempt.
    const HEADER: &str = r#"/**
 * Aera Core's launch configuration.
 *
 * GENERATED from `programs/aera/src/launch.rs` by
 * `programs/aera/tests/test_launch_config.rs`. Do not edit by hand: the test
 * fails if this file and the program's constants disagree.
 *
 * Regenerate with:
 *
 *   AERA_WRITE_LAUNCH_CONFIG=1 cargo test --test test_launch_config
 *
 * Caps and slot counts are strings: they are u64 base units and `JSON.parse`
 * would round them. Parse with `BigInt`, never `Number`.
 */

"#;

    let mut out = String::from(HEADER);
    out.push_str("export const LAUNCH = ");
    out.push_str(json.trim_end());
    out.push_str(
        " as const;

export type LaunchConfig = typeof LAUNCH;
",
    );
    out
}

/// The committed JSON and TypeScript both match the program's constants.
#[test]
fn the_launch_config_matches_the_program() {
    let expected = render();
    let expected_ts = render_ts(&expected);

    if std::env::var("AERA_WRITE_LAUNCH_CONFIG").is_ok() {
        std::fs::create_dir_all(concat!(env!("CARGO_MANIFEST_DIR"), "/../../../config"))
            .expect("could not create config/");
        std::fs::write(CONFIG_PATH, &expected).expect("could not write the launch config");
        std::fs::write(TS_PATH, &expected_ts).expect("could not write the TypeScript module");
        println!("wrote {CONFIG_PATH}");
        println!("wrote {TS_PATH}");
        return;
    }

    /*
     * Both consumers are outside this crate, and one repository does not have
     * them.
     *
     * `config/aera.launch.json` and `sdk/src/launch.generated.ts` are generated
     * FROM the constants below, and live in the monorepo beside the tools that
     * read them. `aera-v1` — the published repository — contains the program
     * and nothing else, so neither path exists there.
     *
     * Absent, they are skipped with a note. Present, the comparison is exactly
     * what it was: this test is the only thing standing between the program's
     * constants and a hand-typed parameter in the interface, and it has caught
     * real drift. A missing file is a different repository; a *differing* file
     * is still a failure.
     */
    if !std::path::Path::new(TS_PATH).exists() && !std::path::Path::new(CONFIG_PATH).exists() {
        println!("no generated consumers in this repository; checked the program's constants only");
        return;
    }

    let actual_ts = std::fs::read_to_string(TS_PATH).unwrap_or_else(|error| {
        panic!(
            "{TS_PATH} is missing ({error}). The SDK imports it. Regenerate with:
             
    AERA_WRITE_LAUNCH_CONFIG=1 cargo test --test test_launch_config
"
        )
    });
    assert_eq!(
        // Line endings normalised, so the file compares the same on Windows and
        // Linux. Written as escapes: an editing pass once expanded these into
        // real newlines, which compiles, does nothing, and reads as deliberate.
        actual_ts.replace("\r\n", "\n"),
        expected_ts,
        "sdk/src/launch.generated.ts has drifted from the program's constants"
    );

    let actual = std::fs::read_to_string(CONFIG_PATH).unwrap_or_else(|error| {
        panic!(
            "{CONFIG_PATH} is missing ({error}).\n\
             Every tool outside the program reads it. Regenerate with:\n\
             \n    AERA_WRITE_LAUNCH_CONFIG=1 cargo test --test test_launch_config\n"
        )
    });

    if actual.replace("\r\n", "\n") != expected {
        // Show the first differing line rather than two hundred lines of JSON.
        let a: Vec<&str> = actual.lines().collect();
        let b: Vec<&str> = expected.lines().collect();
        let at = a.iter().zip(b.iter()).position(|(x, y)| x != y);
        let detail = match at {
            Some(index) => format!(
                "first difference at line {}:\n  committed: {}\n  program:   {}",
                index + 1,
                a.get(index).unwrap_or(&"<end of file>"),
                b.get(index).unwrap_or(&"<end of file>")
            ),
            None => format!(
                "the files agree line by line but differ in length: {} vs {} lines",
                a.len(),
                b.len()
            ),
        };
        panic!(
            "{CONFIG_PATH} has drifted from the program's constants.\n\n{detail}\n\n\
             The program is the source of truth. If the constants changed on purpose:\n\
             \n    AERA_WRITE_LAUNCH_CONFIG=1 cargo test --test test_launch_config\n"
        );
    }
}

/// The launch caps are exactly the figures the brief specifies.
///
/// Named separately from the drift check because these three numbers are a
/// decision, not a derivation. A future edit to `constants.rs` would regenerate
/// the JSON and the test above would happily pass; this one would not.
#[test]
fn the_launch_caps_are_what_was_agreed() {
    const ONE: u64 = 1_000_000_000;
    let cook = cook_reserve_config();

    assert_eq!(cook.supply_cap, 1_000_000 * ONE, "COOK supply cap");
    assert_eq!(cook.borrow_cap, 600_000 * ONE, "COOK borrow cap");
    assert_eq!(
        cook.per_wallet_supply_cap,
        250_000 * ONE,
        "per-wallet supply cap"
    );
}

/// The risk parameters are the validated set.
#[test]
fn the_risk_parameters_are_the_validated_set() {
    let bcook = bcook_reserve_config();

    assert_eq!(bcook.loan_to_value_bps, 5_500, "max LTV 55%");
    assert_eq!(
        bcook.liquidation_threshold_bps, 6_500,
        "liquidation threshold 65%"
    );
    assert_eq!(bcook.liquidation_bonus_bps, 800, "liquidation bonus 8%");
    assert_eq!(bcook.collateral_haircut_bps, 500, "Aera haircut 5%");
    assert_eq!(bcook.reserve_factor_bps, 1_500, "reserve factor 15%");

    // The haircut is Aera's own, and separate from the pool's redemption fee.
    // Merging them is the mistake this assertion exists to prevent: the fee is
    // the stake operator's to set and is taken inside the oracle; the haircut
    // is Aera's and is applied on top.
    assert!(
        bcook.collateral_haircut_bps >= aera::constants::MIN_ADMIN_COLLATERAL_HAIRCUT_BPS,
        "the haircut is below its hard minimum"
    );
}

/// COOK is borrowable and never collateral; bCOOK is the reverse.
///
/// The core asset roles, asserted against the emitted configuration rather than
/// against the program's runtime behaviour, because this is what a deployment
/// would actually write.
#[test]
fn the_asset_roles_are_not_reversed() {
    let cook = cook_reserve_config();
    let bcook = bcook_reserve_config();

    assert!(cook.borrow_enabled, "COOK must be borrowable");
    assert!(
        !cook.collateral_enabled,
        "COOK must never be accepted as collateral"
    );
    assert_eq!(
        cook.loan_to_value_bps, 0,
        "a non-collateral reserve must have no LTV"
    );

    assert!(!bcook.borrow_enabled, "bCOOK must never be borrowable");
    assert!(bcook.collateral_enabled, "bCOOK must be collateral");
}

/// The identities are the ones measured on Cookie Chain.
///
/// These are facts about the network rather than choices, and a deployment
/// against the wrong pool or a redeployed program is exactly the failure the
/// oracle's deployment pin exists to catch. Pinning them here means a typo
/// fails a test rather than a launch.
#[test]
fn the_chain_identities_are_the_measured_ones() {
    assert_eq!(chain::STAKE_POOL_DEPLOY_SLOT, 5_504_973);
    assert_eq!(chain::DECIMALS, 9);

    // The ProgramData address is a PDA of the program id under the loader, so
    // it can be re-derived rather than trusted.
    let program: Pubkey = chain::STAKE_POOL_PROGRAM.parse().unwrap();
    let expected = aera::oracle::deployment::program_data_address(&program);
    assert_eq!(
        expected.to_string(),
        chain::STAKE_POOL_PROGRAM_DATA,
        "the recorded ProgramData address is not what the program id derives to"
    );
}

/// The measurement is recorded, and the deployed value is not silently equal to
/// it.
///
/// `slots_per_year` is the divisor turning an APR into a per-slot rate, so the
/// deployed figure being 2.2% above the measurement means interest accrues 2.2%
/// slower than quoted. That is a real economic fact and it is recorded rather
/// than quietly corrected -- adopting the measurement changes every position
/// and belongs in an operator's hands, behind the timelock.
#[test]
fn the_slot_time_drift_is_recorded_rather_than_applied() {
    let deployed = cook_reserve_config().slots_per_year;
    assert_eq!(deployed, aera::constants::DEFAULT_SLOTS_PER_YEAR);

    let drift_bps = ((deployed as i128 - measured::SLOTS_PER_YEAR_MEASURED as i128) * 10_000)
        / measured::SLOTS_PER_YEAR_MEASURED as i128;

    assert_eq!(
        drift_bps as i64,
        measured::DEPLOYED_DRIFT_BPS,
        "the recorded drift no longer matches the deployed value against the \
         measurement; re-measure and update both, or state why"
    );
}
