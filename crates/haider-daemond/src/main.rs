//! Thin `haiderd` entry point; lifecycle ownership lives in `haider-daemon`.
//!
//! Exit codes: 0 graceful drain, 130 forced termination (128 + SIGINT
//! convention), otherwise `DaemonError::exit_code` (sysexits; 75 = a daemon
//! for this profile is already running).

use haider_daemon::{
    BUILD_UUID, BUILD_VERSION, DaemonConfig, DaemonDependencies, ProviderFactory,
    ProviderFactoryConfig, ResolvedTurnProvider, ShutdownOutcome, process_started_unix_ms,
    run_with_signals_and_dependencies_and_readiness,
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
const EX_SOFTWARE: u8 = 70;
// This mitigation makes Tokio workers match the deliberately large daemon
// entry stack and raises the depth at which accidental recursion overflows.
// It does not make recursion safe or close this failure class: recursive work
// must still be bounded or rewritten iteratively, and every worker reserves
// this much virtual address space while its committed pages grow on demand.
const DAEMON_THREAD_STACK_BYTES: usize = 8 * 1024 * 1024;

#[cfg(not(windows))]
fn main() -> ExitCode {
    initialize_process_diagnostics();
    match daemon_runtime() {
        Ok(runtime) => runtime.block_on(dispatch()),
        Err(error) => {
            eprintln!("haiderd: could not start async runtime: {error}");
            ExitCode::from(EX_SOFTWARE)
        }
    }
}

// `run_with_signals_and_dependencies` owns the whole daemon lifecycle and
// produces a large debug future. Poll it on an explicitly sized Windows
// entry thread; Unix constructs the same explicitly sized Tokio runtime on
// its ordinary process main thread.
#[cfg(windows)]
fn main() -> ExitCode {
    initialize_process_diagnostics();
    let launched = std::thread::Builder::new()
        .name("haiderd-main".into())
        .stack_size(DAEMON_THREAD_STACK_BYTES)
        .spawn(|| daemon_runtime().map(|runtime| runtime.block_on(dispatch())));
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

fn daemon_runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("haiderd-worker")
        .thread_stack_size(DAEMON_THREAD_STACK_BYTES)
        .build()
}

fn initialize_process_diagnostics() {
    let started = process_started_unix_ms();
    std::panic::set_hook(Box::new(move |panic| {
        let thread = std::thread::current();
        let thread = thread.name().unwrap_or("unnamed");
        let (line, column) = panic
            .location()
            .map_or((0, 0), |location| (location.line(), location.column()));
        // This is explicitly an ordinary Rust-panic marker. The payload is
        // intentionally omitted because it can contain prompts, arguments,
        // paths, tokens, or other user data. Direct runtime termination does
        // not run this hook; the pre-dispatch journal covers that class.
        eprintln!(
            "haiderd: diagnostic event=rust_panic_hook build={BUILD_VERSION} \
             build_uuid={BUILD_UUID} pid={} process_started_unix_ms={started} \
             thread={} source_line={line} source_column={column}",
            std::process::id(),
            safe_thread_name(thread),
        );
    }));
}

async fn dispatch() -> ExitCode {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if matches!(args.as_slice(), [argument] if argument == "--version") {
        println!("haiderd {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    let parsed = match parse_args(args.into_iter()) {
        Ok(parsed) => parsed,
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
    match run_with_signals_and_dependencies_and_readiness(
        parsed.config,
        dependencies,
        parsed.readiness,
    )
    .await
    {
        Ok(ShutdownOutcome::Graceful) => ExitCode::SUCCESS,
        Ok(ShutdownOutcome::Forced) => ExitCode::from(130),
        Err(error) => {
            // Every spawned candidate owns a distinct output file. Suppressing
            // repeated lock-contention errors at the profile level therefore
            // creates convincing-but-empty process logs and hides the only
            // event that process experienced.
            eprintln!("haiderd: {error}");
            ExitCode::from(error.exit_code())
        }
    }
}

fn safe_thread_name(name: &str) -> String {
    if name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
    {
        name.to_owned()
    } else {
        "redacted".into()
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
            compaction_promotion: None,
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
struct ParsedArgs {
    config: DaemonConfig,
    readiness: Option<haider_platform::DaemonReadyNotifier>,
}

fn parse_args(args: impl Iterator<Item = String>) -> Result<ParsedArgs, String> {
    let mut profile = None;
    let mut store_dir = None;
    let mut runtime_dir = None;
    let mut readiness = None;
    let mut args = args;
    while let Some(argument) = args.next() {
        let value = args
            .next()
            .ok_or_else(|| format!("missing value for `{argument}`"))?;
        match argument.as_str() {
            "--profile" => profile = Some(value),
            "--store-dir" => store_dir = Some(PathBuf::from(value)),
            "--runtime-dir" => runtime_dir = Some(PathBuf::from(value)),
            haider_platform::DAEMON_READINESS_ARG => readiness = Some(value),
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
    let readiness = readiness
        .map(|token| haider_platform::DaemonReadyNotifier::from_spawn_token(&token))
        .transpose()
        .map_err(|error| format!("invalid startup readiness coordinate: {error}"))?;
    Ok(ParsedArgs { config, readiness })
}

#[cfg(test)]
mod tests {
    use super::*;

    const STACK_PROBE_CHILD: &str = "HAIDER_DAEMON_STACK_PROBE_CHILD";
    const STACK_PROBE_BYTES: usize = 3 * 1024 * 1024;

    #[inline(never)]
    fn consume_worker_stack() -> u8 {
        let mut bytes = [0_u8; STACK_PROBE_BYTES];
        for index in (0..bytes.len()).step_by(4096) {
            bytes[index] = u8::try_from((index / 4096) % 251).unwrap_or_default();
        }
        std::hint::black_box(&bytes);
        bytes[STACK_PROBE_BYTES - 4096]
    }

    /// MUTATION PIN (daemon worker stack): remove the `thread_stack_size`
    /// call from `daemon_runtime`. The isolated child then terminates with
    /// `thread 'haiderd-worker' has overflowed its stack`, while this parent
    /// reports a normal assertion failure instead of aborting the test suite.
    #[test]
    fn daemon_runtime_workers_have_explicit_stack_headroom() {
        if std::env::var_os(STACK_PROBE_CHILD).is_some() {
            let runtime = daemon_runtime()
                .unwrap_or_else(|error| panic!("construct daemon runtime: {error}"));
            let touched = runtime.block_on(async {
                tokio::spawn(async { consume_worker_stack() })
                    .await
                    .unwrap_or_else(|error| panic!("join worker stack probe: {error}"))
            });
            assert_ne!(touched, 255, "probe must touch every stack page");
            return;
        }

        let executable = std::env::current_exe()
            .unwrap_or_else(|error| panic!("locate current test executable: {error}"));
        let output = std::process::Command::new(executable)
            .args([
                "--exact",
                "tests::daemon_runtime_workers_have_explicit_stack_headroom",
                "--nocapture",
            ])
            .env(STACK_PROBE_CHILD, "1")
            .env_remove("TOKIO_WORKER_STACK_SIZE")
            .output()
            .unwrap_or_else(|error| panic!("launch isolated stack probe: {error}"));
        assert!(
            output.status.success(),
            "isolated worker stack probe failed with {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}
