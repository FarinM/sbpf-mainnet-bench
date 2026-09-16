//! Offline benchmark of real mainnet program invocations.
//!
//! Each fixture under `fixtures/data` is a real transaction: its top-level
//! instructions, every account they reference, and the program ELFs involved.
//! The runner loads those programs into an SVM program cache and executes the
//! whole fixture through Agave's SVM as a single message, the way the runtime
//! does. Only the time spent inside `InvokeContext::process_message` is
//! measured -- no bank, no fees, no signature verification, no account loading.

use std::{
    collections::{HashMap, HashSet},
    env,
    fs,
    path::{Path, PathBuf},
    process,
    str::FromStr,
    time::{Duration, Instant},
};

use base64::{
    engine::general_purpose::{STANDARD as BASE64_PADDED, STANDARD_NO_PAD as BASE64},
    Engine as _,
};
use serde::Deserialize;
use serde_json::json;
use solana_account::{Account, AccountSharedData};
use solana_compute_budget::compute_budget::ComputeBudget;
use solana_hash::Hash;
use solana_instruction::{AccountMeta, BorrowedAccountMeta, BorrowedInstruction, Instruction};
use solana_message::{LegacyMessage, Message, SanitizedMessage};
use solana_program_runtime::{
    invoke_context::{EnvironmentConfig, InvokeContext},
    loaded_programs::{ProgramCacheForTxBatch, ProgramRuntimeEnvironments},
    sysvar_cache::SysvarCache,
};
use solana_pubkey::Pubkey;
use solana_svm::conformance::{
    callback::ConformanceCallback,
    programs::{
        add_program_to_program_cache, keyed_account_for_builtin_pubkey,
        new_program_cache_with_builtins,
    },
    setup::{compute_budget, program_runtime_environments},
};
use solana_svm_feature_set::SVMFeatureSet;
use solana_svm_log_collector::LogCollector;
use solana_svm_timings::ExecuteTimings;
use solana_svm_transaction::svm_message::SVMStaticMessage;
use solana_transaction_context::transaction::TransactionContext;

#[derive(Deserialize)]
struct Fixture {
    name: String,
    signature: String,
    slot: u64,
    #[serde(default)]
    compute_units_consumed: Option<u64>,
    #[serde(default)]
    trimmed_instructions: Option<usize>,
    #[serde(default)]
    recent_blockhash: Option<String>,
    instructions: Vec<FixtureInstruction>,
    accounts: Vec<FixtureAccount>,
    programs: Vec<FixtureProgram>,
    #[serde(default)]
    native_programs: Vec<String>,
}

#[derive(Deserialize)]
struct FixtureInstruction {
    program_id: String,
    accounts: Vec<FixtureMeta>,
    data: String,
}

#[derive(Deserialize)]
struct FixtureMeta {
    pubkey: String,
    #[serde(default)]
    signer: bool,
    #[serde(default)]
    writable: bool,
}

#[derive(Deserialize)]
struct FixtureAccount {
    pubkey: String,
    #[serde(default, rename = "virtual")]
    virtual_kind: Option<String>,
    #[serde(default)]
    lamports: Option<u64>,
    #[serde(default)]
    owner: Option<String>,
    #[serde(default)]
    rent_epoch: Option<u64>,
    #[serde(default)]
    data: Option<String>,
}

#[derive(Deserialize)]
struct FixtureProgram {
    pubkey: String,
    path: String,
}

struct Options {
    fixtures: PathBuf,
    warmup: usize,
    iterations: usize,
    only: Option<String>,
    json: Option<PathBuf>,
    virtual_address_space_adjustments: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            fixtures: PathBuf::from("../fixtures/data"),
            warmup: 5,
            iterations: 100,
            only: None,
            json: None,
            virtual_address_space_adjustments: false,
        }
    }
}

fn parse_args() -> Options {
    let mut options = Options::default();
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--fixtures" => options.fixtures = args.next().expect("missing --fixtures value").into(),
            "--warmup" => options.warmup = args.next().expect("missing --warmup value").parse().expect("invalid --warmup"),
            "--iterations" => options.iterations = args.next().expect("missing --iterations value").parse().expect("invalid --iterations"),
            "--only" => options.only = Some(args.next().expect("missing --only value")),
            "--json" => options.json = Some(args.next().expect("missing --json value").into()),
            "--virtual-address-space-adjustments" => options.virtual_address_space_adjustments = true,
            "--help" | "-h" => {
                println!("usage: runner [--fixtures DIR] [--warmup N] [--iterations N] [--only NAME] [--json PATH]");
                process::exit(0);
            }
            other => panic!("unknown argument {other}"),
        }
    }
    options
}

/// Mainnet has not activated `virtual_address_space_adjustments` (verified
/// against the feature account), and Agave derives both `aligned_memory_mapping`
/// and `enable_stack_frame_gaps` from that flag. Leaving it enabled would select
/// the unaligned mapping and skip the translation path under test entirely.
fn feature_set(virtual_address_space_adjustments: bool) -> SVMFeatureSet {
    let mut set = SVMFeatureSet::all_enabled();
    set.virtual_address_space_adjustments = virtual_address_space_adjustments;
    set
}

fn parse_pubkey(value: &str) -> Result<Pubkey, String> {
    Pubkey::from_str(value).map_err(|err| format!("invalid pubkey {value}: {err}"))
}

/// The RPC does not guarantee base64 padding.
fn decode_base64(value: &str) -> Result<Vec<u8>, String> {
    BASE64
        .decode(value)
        .or_else(|_| BASE64_PADDED.decode(value))
        .map_err(|err| format!("invalid base64: {err}"))
}

/// The conformance harness helper for this is gated behind its `conformance`
/// feature, so fill the cache here: real sysvar accounts from the fixture when
/// present, defaults for the handful a program can read without an account.
fn sysvar_cache_from_accounts(accounts: &[(Pubkey, Account)]) -> SysvarCache {
    let mut cache = SysvarCache::default();
    cache.fill_missing_entries(|pubkey, set_sysvar| {
        if let Some((_, account)) = accounts
            .iter()
            .find(|(key, account)| key == pubkey && account.lamports > 0)
        {
            set_sysvar(&account.data);
        } else if pubkey == &solana_sdk_ids::sysvar::rent::id() {
            set_sysvar(&bincode::serialize(&solana_rent::Rent::default()).unwrap());
        } else if pubkey == &solana_sdk_ids::sysvar::clock::id() {
            set_sysvar(&bincode::serialize(&solana_clock::Clock::default()).unwrap());
        } else if pubkey == &solana_sdk_ids::sysvar::epoch_schedule::id() {
            set_sysvar(&bincode::serialize(&solana_epoch_schedule::EpochSchedule::default()).unwrap());
        }
    });
    cache
}

fn build_accounts(fixture: &Fixture, loader_key: &Pubkey) -> Result<Vec<(Pubkey, Account)>, String> {
    let mut accounts: HashMap<Pubkey, Account> = HashMap::new();
    for account in &fixture.accounts {
        let key = parse_pubkey(&account.pubkey)?;
        if account.virtual_kind.is_some() {
            // Patched with real instructions sysvar data in `run_fixture`.
            accounts.insert(
                key,
                Account {
                    lamports: 1,
                    data: Vec::new(),
                    owner: solana_sdk_ids::sysvar::id(),
                    executable: false,
                    rent_epoch: 0,
                },
            );
            continue;
        }
        let owner = parse_pubkey(account.owner.as_deref().ok_or("account without owner")?)?;
        let data = decode_base64(account.data.as_deref().unwrap_or_default())
            .map_err(|err| format!("invalid account data for {key}: {err}"))?;
        accounts.insert(
            key,
            Account {
                lamports: account.lamports.unwrap_or(0),
                data,
                owner,
                executable: false,
                rent_epoch: account.rent_epoch.unwrap_or(0),
            },
        );
    }
    for program in &fixture.programs {
        accounts.insert(
            parse_pubkey(&program.pubkey)?,
            Account {
                lamports: 1,
                data: Vec::new(),
                owner: *loader_key,
                executable: true,
                rent_epoch: u64::MAX,
            },
        );
    }
    for program_id in &fixture.native_programs {
        let key = parse_pubkey(program_id)?;
        match keyed_account_for_builtin_pubkey(&key) {
            Some((key, account)) => {
                accounts.insert(key, account);
            }
            None => {
                accounts.insert(
                    key,
                    Account {
                        lamports: 1,
                        data: Vec::new(),
                        owner: solana_sdk_ids::native_loader::id(),
                        executable: true,
                        rent_epoch: u64::MAX,
                    },
                );
            }
        }
    }
    Ok(accounts.into_iter().collect())
}

fn build_instructions(fixture: &Fixture) -> Result<Vec<Instruction>, String> {
    fixture
        .instructions
        .iter()
        .map(|instruction| {
            Ok(Instruction {
                program_id: parse_pubkey(&instruction.program_id)?,
                accounts: instruction
                    .accounts
                    .iter()
                    .map(|meta| {
                        Ok(AccountMeta {
                            pubkey: parse_pubkey(&meta.pubkey)?,
                            is_signer: meta.signer,
                            is_writable: meta.writable,
                        })
                    })
                    .collect::<Result<Vec<_>, String>>()?,
                data: decode_base64(&instruction.data)
                    .map_err(|err| format!("invalid instruction data: {err}"))?,
            })
        })
        .collect()
}

/// The runtime writes the current instruction index into the instructions
/// sysvar and programs read other instructions from it, so it needs the real
/// serialized instruction list instead of an empty placeholder.
fn instructions_sysvar_data(instructions: &[Instruction]) -> Result<Vec<u8>, String> {
    let borrowed: Vec<BorrowedInstruction> = instructions
        .iter()
        .map(|instruction| BorrowedInstruction {
            program_id: &instruction.program_id,
            accounts: instruction
                .accounts
                .iter()
                .map(|meta| BorrowedAccountMeta {
                    pubkey: &meta.pubkey,
                    is_signer: meta.is_signer,
                    is_writable: meta.is_writable,
                })
                .collect(),
            data: &instruction.data,
        })
        .collect();
    solana_instructions_sysvar::construct_instructions_data(&borrowed)
        .map_err(|err| format!("instructions sysvar: {err:?}"))
}

struct TransactionRun {
    elapsed: Duration,
    compute_units: u64,
    result: Result<(), String>,
    logs: Vec<String>,
}

#[allow(clippy::too_many_arguments)]
fn run_transaction(
    instructions: &[Instruction],
    accounts: &[(Pubkey, Account)],
    recent_blockhash: Option<&str>,
    feature_set: &SVMFeatureSet,
    program_cache: &mut ProgramCacheForTxBatch,
    sysvar_cache: &SysvarCache,
    environments: &ProgramRuntimeEnvironments,
    compute_budget: &ComputeBudget,
    callback: &ConformanceCallback,
) -> TransactionRun {
    let message = Message::new(instructions, None);
    let sanitized_message = SanitizedMessage::Legacy(LegacyMessage::new(message, &HashSet::new()));
    let lookup: HashMap<Pubkey, Account> = accounts.iter().cloned().collect();
    let transaction_accounts = sanitized_message
        .account_keys()
        .iter()
        .map(|key| (*key, AccountSharedData::from(lookup.get(key).cloned().unwrap_or_default())))
        .collect();
    let rent = sysvar_cache.get_rent().expect("rent sysvar");
    let mut transaction_context = TransactionContext::new(
        transaction_accounts,
        (*rent).clone(),
        compute_budget.max_instruction_stack_depth,
        compute_budget.max_instruction_trace_length,
        sanitized_message.num_instructions(),
    );
    let log_collector = LogCollector::new_ref();
    // Use the transaction's own blockhash; durable nonce programs compare it.
    let blockhash = recent_blockhash
        .as_deref()
        .and_then(|value| Pubkey::from_str(value).ok())
        .map(|key| Hash::new_from_array(key.to_bytes()))
        .unwrap_or_default();
    let environment_config = EnvironmentConfig::new(
        blockhash,
        5000,
        false,
        callback,
        feature_set,
        environments,
        sysvar_cache,
    );

    let mut timings = ExecuteTimings::default();
    let mut compute_units = 0u64;
    let (result, elapsed) = {
        let mut invoke_context = InvokeContext::new(
            &mut transaction_context,
            program_cache,
            environment_config,
            Some(log_collector.clone()),
            compute_budget.to_budget(),
            compute_budget.to_cost(),
        );
        let started = Instant::now();
        let result = invoke_context.process_message(
            &sanitized_message,
            &mut timings,
            &mut compute_units,
        );
        (result, started.elapsed())
    };

    let logs = std::rc::Rc::try_unwrap(log_collector)
        .ok()
        .map(|cell| cell.into_inner().into_messages())
        .unwrap_or_default();
    TransactionRun {
        elapsed,
        compute_units,
        result: result.map_err(|(_, err)| format!("{err:?}")),
        logs,
    }
}

fn run_fixture(options: &Options, path: &Path) -> Result<serde_json::Value, String> {
    let source = fs::read_to_string(path).map_err(|err| format!("{}: {err}", path.display()))?;
    let fixture: Fixture = serde_json::from_str(&source).map_err(|err| format!("{}: {err}", path.display()))?;

    let feature_set = feature_set(options.virtual_address_space_adjustments);
    let compute_budget = compute_budget(&feature_set);
    let environments = program_runtime_environments(&feature_set, &compute_budget);
    let callback = ConformanceCallback::default();
    let loader_key = solana_sdk_ids::bpf_loader_upgradeable::id();

    let instructions = build_instructions(&fixture)?;
    let mut initial_accounts = build_accounts(&fixture, &loader_key)?;
    let instructions_sysvar_id = solana_sdk_ids::sysvar::instructions::id();
    if let Some((_, account)) = initial_accounts
        .iter_mut()
        .find(|(key, _)| *key == instructions_sysvar_id)
    {
        account.data = instructions_sysvar_data(&instructions)?;
    }
    let sysvar_cache = sysvar_cache_from_accounts(&initial_accounts);

    let mut program_cache = new_program_cache_with_builtins(fixture.slot);
    let mut load_time = Duration::ZERO;
    // Program paths in the fixtures are relative to the fixtures root, which is
    // the parent of the data directory passed via --fixtures.
    let fixtures_root = options.fixtures.parent().unwrap_or(Path::new("."));
    for program in &fixture.programs {
        let elf_path = fixtures_root.join(&program.path);
        let elf = fs::read(&elf_path).map_err(|err| format!("{}: {err}", elf_path.display()))?;
        let started = Instant::now();
        add_program_to_program_cache(
            &mut program_cache,
            &parse_pubkey(&program.pubkey)?,
            &loader_key,
            &elf,
            &feature_set,
        );
        load_time += started.elapsed();
    }

    let run_once = |program_cache: &mut ProgramCacheForTxBatch| {
        run_transaction(
            &instructions,
            &initial_accounts,
            fixture.recent_blockhash.as_deref(),
            &feature_set,
            program_cache,
            &sysvar_cache,
            &environments,
            &compute_budget,
            &callback,
        )
    };

    let verify = run_once(&mut program_cache);
    for _ in 0..options.warmup {
        run_once(&mut program_cache);
    }

    let mut samples: Vec<u128> = Vec::with_capacity(options.iterations);
    for _ in 0..options.iterations {
        samples.push(run_once(&mut program_cache).elapsed.as_nanos());
    }
    samples.sort_unstable();
    let min = samples[0];
    let median = samples[samples.len() / 2];
    let mean = samples.iter().sum::<u128>() / samples.len() as u128;

    if let Err(error) = &verify.result {
        eprintln!("  {} failed: {error}", fixture.name);
        for log in verify.logs.iter().rev().take(4).rev() {
            eprintln!("    log: {log}");
        }
    }

    Ok(json!({
        "fixture": fixture.name,
        "signature": fixture.signature,
        "instructions": fixture.instructions.len(),
        "warmup": options.warmup,
        "iterations": options.iterations,
        "load_ns": load_time.as_nanos() as u64,
        "cu": verify.compute_units,
        "mainnet_cu": fixture.compute_units_consumed,
        "trimmed_instructions": fixture.trimmed_instructions,
        "ns_min": min as u64,
        "ns_median": median as u64,
        "ns_mean": mean as u64,
        "result": match &verify.result {
            Ok(()) => "ok".to_string(),
            Err(error) => format!("error: {error}"),
        },
    }))
}

fn main() {
    let options = parse_args();
    let mut paths: Vec<PathBuf> = fs::read_dir(&options.fixtures)
        .unwrap_or_else(|err| panic!("{}: {err}", options.fixtures.display()))
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|extension| extension == "json"))
        .collect();
    paths.sort();
    if let Some(only) = &options.only {
        paths.retain(|path| path.file_stem().is_some_and(|stem| stem == only.as_str()));
    }

    let mut lines = Vec::new();
    for path in &paths {
        match run_fixture(&options, path) {
            Ok(value) => lines.push(value),
            Err(error) => panic!("{error}"),
        }
    }

    let output = lines.iter().map(|value| value.to_string()).collect::<Vec<_>>().join("\n");
    if let Some(json_path) = &options.json {
        fs::write(json_path, format!("{output}\n")).unwrap_or_else(|err| panic!("{}: {err}", json_path.display()));
    }
    println!("{output}");
}
