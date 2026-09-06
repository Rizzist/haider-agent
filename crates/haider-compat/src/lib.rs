//! Compatibility with updaters that only accept haider + haiderd archives.
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

/// --version and self-test are the old updater's staging probes. Neither may
/// mutate the install. All other invocations finish the exact embedded bundle
/// through the shared install transaction before forwarding original argv.
pub fn run(thin: &[u8], payload: &[u8]) -> ExitCode {
    let words: Vec<String> = std::env::args().skip(1).collect();
    if matches!(
        haider_cli::routing::parse_command(&words),
        Ok(haider_cli::routing::Command::Version)
    ) {
        println!("haider {}", haider_cli::VERSION);
        return ExitCode::SUCCESS;
    }
    let result = (|| {
        let executable = std::env::current_exe()?;
        let install_dir = executable
            .parent()
            .ok_or_else(|| io::Error::other("compatibility executable has no parent"))?;
        let stage = embedded_sources(thin, payload, &install_dir.join(binary_name("haiderd")))?;
        if matches!(
            haider_cli::routing::parse_command(&words),
            Ok(haider_cli::routing::Command::SelfTest)
        ) {
            let output = complete_bundle_self_test(stage.path())?;
            io::stdout().write_all(&output.stdout)?;
            io::stderr().write_all(&output.stderr)?;
            return Ok(output.status.code().unwrap_or(70));
        }
        haider_cli::update::install_bundle_from_directory(stage.path(), install_dir)
            .map_err(io::Error::other)?;
        let mut command = Command::new(install_dir.join(binary_name("haider")));
        command.args(std::env::args_os().skip(1));
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.arg0(
                std::env::args_os()
                    .next()
                    .unwrap_or_else(|| "haider".into()),
            );
            // Remove the temporary source before replacing this process.
            stage.close()?;
            Err(command.exec())
        }
        #[cfg(not(unix))]
        {
            Ok(command.status()?.code().unwrap_or(70))
        }
    })();
    match result {
        Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(70)),
        Err(error) => {
            eprintln!("haider: compatibility bundle installation failed: {error}");
            ExitCode::from(74)
        }
    }
}

fn embedded_sources(thin: &[u8], payload: &[u8], daemon: &Path) -> io::Result<tempfile::TempDir> {
    let stage = tempfile::tempdir()?;
    for (name, bytes) in [("haider", thin), ("haider-tui", payload)] {
        let path = stage.path().join(binary_name(name));
        std::fs::write(&path, bytes)?;
        make_executable(&path)?;
    }
    std::fs::copy(daemon, stage.path().join(binary_name("haiderd")))?;
    Ok(stage)
}

/// Historical updaters run this probe before acquiring their transaction.
/// Validate the complete embedded build in temporary directories: corrupt or
/// mismatched thin/payload bytes must fail before a compatibility executable
/// can replace the old working installation. The shared verifier checks OS
/// signatures, every --version, and the real thin-client offline self-test.
fn complete_bundle_self_test(source: &Path) -> io::Result<std::process::Output> {
    use haider_cli::update::members::BundleMember;
    let verification = tempfile::tempdir()?;
    let bundle = haider_cli::update::staging::stage_directory_bundle(
        source,
        verification.path(),
        haider_cli::VERSION,
    )
    .map_err(io::Error::other)?;
    Command::new(bundle.path(BundleMember::Daemon))
        .arg("--client-self-test")
        .arg("--payload")
        .arg(bundle.path(BundleMember::Tui))
        .output()
}

fn binary_name(name: &str) -> PathBuf {
    PathBuf::from(format!("{name}{}", std::env::consts::EXE_SUFFIX))
}

#[cfg(unix)]
fn make_executable(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
}
#[cfg(not(unix))]
fn make_executable(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    // Real native fixture executables go through the production copy, signing
    // (on macOS), exact-version and smoke path. No verifier is injected. These
    // fixtures do not claim to be the actual daemon or interactive application.
    fn executable_fixture(root: &Path, version: &str, label: &str) -> Vec<u8> {
        let source = root.join(format!("{label}.rs"));
        let executable = root.join(format!("{label}{}", std::env::consts::EXE_SUFFIX));
        let code = r#"
fn main() {
    const VERSION: &str = __VERSION__;
    std::hint::black_box(concat!("haider.payload.v1\0", __VERSION__, "\0"));
    let path = std::env::current_exe().unwrap();
    let name = path.file_stem().unwrap().to_str().unwrap();
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args == ["--version"] { println!("{name} {VERSION}"); return; }
    if (name == "haider" && args == ["self-test"]) ||
       (name == "haiderd" && args.first().map(String::as_str) == Some("--client-self-test")) {
        println!("{{\"schema\":\"haider.selftest.v0\",\"version\":\"{VERSION}\",\"ok\":true,\"checks\":[]}}");
        return;
    }
    std::process::exit(2);
}
"#.replace("__VERSION__", &format!("{version:?}"));
        std::fs::write(&source, code).expect("fixture source");
        let output = Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
            .arg("--edition=2024")
            .arg(&source)
            .arg("-o")
            .arg(&executable)
            .output()
            .expect("compile native fixture");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        std::fs::read(executable).expect("fixture bytes")
    }

    /// MUTATION CHECK: only execute the daemon report and skip embedded CLI or
    /// payload verification. Corrupt/wrong-version embedded binaries would then
    /// pass the old updater's staging probe and break the next invocation.
    #[test]
    fn legacy_self_test_checks_corrupt_and_wrong_version_embedded_members_before_install() {
        let fixtures = tempfile::tempdir().expect("native fixtures");
        let good = executable_fixture(fixtures.path(), haider_cli::VERSION, "good");
        let wrong = executable_fixture(fixtures.path(), "9.9.9", "wrong");
        let old_install = tempfile::tempdir().expect("watched old installation");
        let installed_cli = old_install.path().join(binary_name("haider"));
        let installed_daemon = old_install.path().join(binary_name("haiderd"));
        std::fs::write(&installed_cli, b"old-working-cli").expect("old CLI");
        std::fs::write(&installed_daemon, &good).expect("matching daemon source");
        make_executable(&installed_daemon).expect("daemon executable");
        for (thin, payload) in [
            (&b"corrupt-thin"[..], good.as_slice()),
            (wrong.as_slice(), good.as_slice()),
            (good.as_slice(), wrong.as_slice()),
            (good.as_slice(), &b"corrupt-payload"[..]),
        ] {
            let source =
                embedded_sources(thin, payload, &installed_daemon).expect("embedded extraction");
            assert!(complete_bundle_self_test(source.path()).is_err());
            assert_eq!(
                std::fs::read(&installed_cli).expect("old CLI unchanged"),
                b"old-working-cli"
            );
            assert_eq!(
                std::fs::read(&installed_daemon).expect("old daemon unchanged"),
                good
            );
            assert!(!old_install.path().join(binary_name("haider-tui")).exists());
            assert_eq!(
                std::fs::read_dir(old_install.path())
                    .expect("old install entries")
                    .count(),
                2
            );
        }
        let source =
            embedded_sources(&good, &good, &installed_daemon).expect("matching embedded bundle");
        let report =
            complete_bundle_self_test(source.path()).expect("complete fixture verification");
        assert!(report.status.success());
        assert!(
            String::from_utf8(report.stdout)
                .expect("JSON output")
                .contains("\"ok\":true")
        );
        assert_eq!(
            std::fs::read_dir(old_install.path())
                .expect("old install entries")
                .count(),
            2
        );
    }
}
