//! Thin `haiderd` entry point; lifecycle ownership lives in `haider-daemon`.
//!
//! Exit codes: 0 graceful drain, 130 forced termination (128 + SIGINT
//! convention), otherwise `DaemonError::exit_code` (sysexits; 75 = a daemon
//! for this profile is already running).

use haider_daemon::{
    DaemonConfig, DaemonDependencies, ProviderFactory, ProviderFactoryConfig, ResolvedTurnProvider,
    ShutdownOutcome, run_with_signals_and_dependencies,
};
use haider_protocol::error::HaiderError;
use haider_protocol::session::SessionMetadataV1;
use haider_provider::FakeProvider;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

/// TEST-ONLY seam, OFF by default (W3c3 M3).
///
/// When set to a `FakeProvider` step script (the same JSON the CLI's
/// `HAIDER_FAKE_SCRIPT_JSON` takes), the daemon resolves EVERY turn to that
/// deterministic provider instead of the account store. This exists for one
/// reason: `scripts/tui-probes/pty-probe-live.py` drives the REAL `haider`
/// binary against the REAL daemon over a real socket, and §6.4 requires
/// that path to pass "deterministically with `FakeProvider` and no network".
///
/// It is never read unless the variable is present, and it can only ever
/// SUBSTITUTE a provider implementation: it grants no capability, relaxes
/// no auth, and touches no credential path. `resolve_for_turn` echoes the
/// SESSION's own provider name, so the worker's factory check still fires.
///
/// It does NOT narrow `session.create`'s whitelist to `{"fake"}`: every
/// turn resolves to the fake regardless of what the session was created
/// with, so the release default is accepted too (otherwise a client using
/// its own profile's provider is rejected for a provider the daemon was
/// never going to call). The containment that DOES hold is announcement,
/// not restriction — see below.
///
/// Because an auto-spawned daemon inherits the launcher's environment, a
/// variable left exported in a shell, a `.envrc` or a CI runner would
/// otherwise produce a lingering singleton answering every later turn from
/// a fake, with nothing on screen to say so. Two guards close that: the
/// daemon refuses to start unless the value parses as a script (a typo
/// exits 64 rather than degrading), and it announces itself loudly on
/// stderr AND in its log so the profile's daemon.log names the condition.
const FAKE_PROVIDER_ENV: &str = "HAIDER_TEST_FAKE_PROVIDER";
#[cfg(windows)]
const EX_SOFTWARE: u8 = 70;

#[cfg(not(windows))]
#[tokio::main]
async fn main() -> ExitCode {
    dispatch().await
}

// `run_with_signals_and_dependencies` owns the whole daemon lifecycle and
// produces a large debug future. Poll it on an explicitly sized Windows
// entry thread; Unix retains its ordinary Tokio main path.
#[cfg(windows)]
fn main() -> ExitCode {
    let launched = std::thread::Builder::new()
        .name("haiderd-main".into())
        .stack_size(8 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map(|runtime| runtime.block_on(dispatch()))
        });
    match launched {
        Ok(thread) => match thread.join() {
            Ok(Ok(code)) => code,
            Ok(Err(error)) => {
                eprintln!("haiderd: could not start async runtime: {error}");
                ExitCode::from(EX_SOFTWARE)
            }
            Err(_) => {
                eprintln!("haiderd: main runtime thread panicked");
                ExitCode::from(EX_SOFTWARE)
            }
        },
        Err(error) => {
            eprintln!("haiderd: could not start main runtime thread: {error}");
            ExitCode::from(EX_SOFTWARE)
        }
    }
}

async fn dispatch() -> ExitCode {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if matches!(args.as_slice(), [argument] if argument == "--version") {
        println!("haiderd {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    let config = match parse_args(args.into_iter()) {
        Ok(config) => config,
        Err(message) => {
            eprintln!("haiderd: {message}");
            return ExitCode::from(64);
        }
    };
    let dependencies = match test_dependencies() {
        Ok(dependencies) => dependencies,
        Err(message) => {
            eprintln!("haiderd: {message}");
            return ExitCode::from(64);
        }
    };
    match run_with_signals_and_dependencies(config, dependencies).await {
        Ok(ShutdownOutcome::Graceful) => ExitCode::SUCCESS,
        Ok(ShutdownOutcome::Forced) => ExitCode::from(130),
        Err(error) => {
            eprintln!("haiderd: {error}");
            ExitCode::from(error.exit_code())
        }
    }
}

/// Production dependencies, unless the test seam above is armed.
fn test_dependencies() -> Result<DaemonDependencies, String> {
    let Some(script) = std::env::var_os(FAKE_PROVIDER_ENV) else {
        return Ok(DaemonDependencies::default());
    };
    let script = script
        .into_string()
        .map_err(|_| format!("{FAKE_PROVIDER_ENV} is not valid UTF-8"))?;
    let fake = FakeProvider::from_json(&script)
        .map_err(|error| format!("{FAKE_PROVIDER_ENV} is not a fake-provider script: {error}"))?;
    eprintln!("haiderd: TEST MODE — every turn resolves to the injected fake provider");
    // Every turn resolves to the fake regardless of what the session was
    // created with, so the creatable set includes the release default too:
    // otherwise a client using the profile's own provider is rejected at
    // `session.create` for a provider the daemon was never going to call.
    let mut providers = haider_provider::BUILTIN_PROVIDER_NAMES
        .into_iter()
        .map(str::to_owned)
        .collect::<std::collections::BTreeSet<_>>();
    providers.insert("fake".to_owned());
    Ok(DaemonDependencies {
        provider_factory: ProviderFactoryConfig::Injected {
            factory: Arc::new(FakeFactory {
                fake: Arc::new(fake),
            }),
            providers,
        },
        ..DaemonDependencies::default()
    })
}

/// The injected factory: one deterministic provider for every turn.
struct FakeFactory {
    fake: Arc<FakeProvider>,
}

#[async_trait::async_trait]
impl ProviderFactory for FakeFactory {
    async fn resolve_for_turn(
        &self,
        metadata: &SessionMetadataV1,
    ) -> Result<ResolvedTurnProvider, HaiderError> {
        Ok(ResolvedTurnProvider {
            provider: self.fake.clone(),
            // Echo the SESSION's own provider: the worker rejects a
            // factory that returns a different provider than the session
            // was created with, and the point of this seam is to substitute
            // the IMPLEMENTATION, not to rewrite the session.
            provider_name: metadata.provider.clone(),
            model: metadata.model.clone(),
            context_window: None,
            account_alias: None,
            initial_rotation: None,
            rotation_budget_consumed: false,
            attempt_resolver: None,
        })
    }
}

/// `haiderd [--profile <id> --store-dir <path> --runtime-dir <path>]`.
///
/// All three identity flags together, or none: a bare `haiderd` resolves the
/// SAME shared profile `haider` resolves (R8's one-resolver law), while a
/// spawning `haider` passes the exact resolved values explicitly. A partial
/// set is refused — mixing resolved and explicit identity could bind a
/// socket for one profile against another profile's store. Every other knob
/// keeps its `DaemonConfig` default.
fn parse_args(args: impl Iterator<Item = String>) -> Result<DaemonConfig, String> {
    let mut profile = None;
    let mut store_dir = None;
    let mut runtime_dir = None;
    let mut args = args;
    while let Some(argument) = args.next() {
        let value = args
            .next()
            .ok_or_else(|| format!("missing value for `{argument}`"))?;
        match argument.as_str() {
            "--profile" => profile = Some(value),
            "--store-dir" => store_dir = Some(PathBuf::from(value)),
            "--runtime-dir" => runtime_dir = Some(PathBuf::from(value)),
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    let mut config = match (profile, store_dir, runtime_dir) {
        (Some(profile), Some(store_dir), Some(runtime_dir)) => {
            let env = haider_client::ProfileEnv::capture();
            // The identity flags are explicit, but the release-owned default
            // model still resolves through the ONE shared precedence
            // (HAIDER_MODEL, then <store_dir>/config.json, then packaged).
            let default_model = haider_client::resolve_default_model_for(&store_dir, &env)
                .map_err(|error| format!("cannot resolve default model: {error}"))?;
            let mut config = DaemonConfig::new(profile, store_dir, runtime_dir);
            config.default_model = default_model;
            config
        }
        (None, None, None) => {
            let env = haider_client::ProfileEnv::capture();
            let resolved = haider_client::resolve_profile(&env)
                .map_err(|error| format!("cannot resolve profile: {error}"))?;
            let mut config = DaemonConfig::new(
                resolved.profile_id,
                resolved.store_dir,
                resolved.runtime_dir,
            );
            config.default_model = resolved.default_model;
            config
        }
        _ => {
            return Err(
                "--profile, --store-dir, and --runtime-dir must be given together (or all \
                 omitted to resolve the shared default profile)"
                    .into(),
            );
        }
    };
    // `HAIDER_DISCOVERY_DISABLED` (any value but `0`): never probe first-party
    // device credential stores. Tests and CI set this so startup
    // auto-adoption cannot read the HOST machine's real codex/Claude/kimi
    // credentials into a throwaway profile.
    if std::env::var_os("HAIDER_DISCOVERY_DISABLED").is_some_and(|value| value != "0") {
        config.discovery_disabled = true;
    }
    Ok(config)
}
