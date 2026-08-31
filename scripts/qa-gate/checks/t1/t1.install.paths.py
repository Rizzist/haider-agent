"""Install the tagged tarball into a scratch prefix and exercise that pair."""

from __future__ import annotations

from pathlib import Path
import platform
import shutil

from gate import (
    DAEMON_STARTUP,
    DAEMON_STOP,
    ENV_BLOCKED,
    FAIL,
    PASS,
    PROCESS_EXIT_GRACE,
    STATUS_REQUEST,
    VERSION_QUERY,
    BudgetPart,
    Evidence,
)
from gate.context import parse_single_json, wait_pid_gone

id = "t1.install.paths"
tier = "t1"
area = "install"
needs = ("binary", "daemon", "network:github")
script = [{"step": "finish", "reason": "end_turn"}]
turns_expected = 0
timed = True

INSTALL_FETCH_OUTER = BudgetPart(
    "installer two-fetch outer ceiling",
    120.0,
    "qa-gate 60s allowance per fetch; scripts/install.sh:18-25 has no internal timeout "
    "and scripts/install.sh:74-75 performs the archive and checksum fetches",
)
INSTALL_EXTRACT_COPY = BudgetPart(
    "installer checksum extraction and prefix copy",
    30.0,
    "scripts/install.sh:77-119 checksum, tar extraction, and executable copies",
)
# Registry #94: source version 30; installer outer 120+30; installed version
# 30; ready 30; status 60; stop 20+2 and PID observation 2; cleanup status
# 60 + stop 20+2 + historical PID observation 2. Total = 408s.
budget = (
    VERSION_QUERY
    + INSTALL_FETCH_OUTER
    + INSTALL_EXTRACT_COPY
    + VERSION_QUERY
    + DAEMON_STARTUP
    + STATUS_REQUEST
    + DAEMON_STOP
    + PROCESS_EXIT_GRACE
    + PROCESS_EXIT_GRACE
    + STATUS_REQUEST
    + DAEMON_STOP
    + PROCESS_EXIT_GRACE
    + PROCESS_EXIT_GRACE
)


def _version(stdout: str, product: str) -> str | None:
    line = stdout.strip()
    prefix = f"{product} "
    return line.removeprefix(prefix) if line.startswith(prefix) else None


def _network_failure(stderr: str) -> bool:
    lowered = stderr.lower()
    return any(
        marker in lowered
        for marker in (
            "could not resolve",
            "failed to connect",
            "connection timed out",
            "operation timed out",
            "network is unreachable",
            "http 500",
            "http 502",
            "http 503",
            "http 504",
        )
    )


def run(ctx) -> list[Evidence]:
    install_script = Path(__file__).resolve().parents[4] / "scripts" / "install.sh"
    if not install_script.is_file():
        return [
            Evidence(
                "install_script",
                ENV_BLOCKED,
                f"missing need: scripts/install.sh unavailable path={install_script}",
            )
        ]

    supported = {
        ("Darwin", "arm64"),
        ("Darwin", "aarch64"),
        ("Darwin", "x86_64"),
        ("Linux", "x86_64"),
        ("Linux", "amd64"),
        ("Linux", "aarch64"),
        ("Linux", "arm64"),
    }
    actual_platform = (platform.system(), platform.machine())
    if actual_platform not in supported:
        return [
            Evidence(
                "installer_platform",
                ENV_BLOCKED,
                f"missing need: scripts/install.sh unsupported platform actual={actual_platform!r}",
            )
        ]
    missing_tools = []
    if shutil.which("tar") is None:
        missing_tools.append("tar")
    if shutil.which("curl") is None and shutil.which("wget") is None:
        missing_tools.append("curl-or-wget")
    if shutil.which("sha256sum") is None and shutil.which("shasum") is None:
        missing_tools.append("sha256sum-or-shasum")
    if missing_tools:
        return [
            Evidence(
                "installer_tools",
                ENV_BLOCKED,
                f"missing need: installer tools unavailable actual={missing_tools!r}",
            )
        ]

    failures: list[str] = []
    results = []
    source_version = ctx.run_haider(["--version"], timeout=VERSION_QUERY)
    results.append(("source-version", source_version))
    expected = _version(source_version.stdout, "haider")
    if source_version.timed_out or source_version.returncode != 0 or expected is None:
        failures.append(
            f"source version expected=haider-semver/0 actual={source_version.stdout.strip()!r}/"
            f"{source_version.returncode} timed_out={str(source_version.timed_out).lower()}"
        )
        return [
            Evidence(
                "install_paths",
                FAIL,
                "; ".join(failures),
                [ctx.command_artefact(name, result) for name, result in results],
            )
        ]

    install_home = ctx.root / "install-home"
    install_prefix = ctx.root / "install-prefix" / "bin"
    install_home.mkdir(mode=0o700)
    install_prefix.mkdir(parents=True, mode=0o700)
    install = ctx.run_command(
        ["/bin/sh", install_script],
        timeout=INSTALL_FETCH_OUTER + INSTALL_EXTRACT_COPY,
        env_overrides={
            "HOME": install_home,
            "USERPROFILE": install_home,
            "HAIDER_INSTALL_DIR": install_prefix,
            "HAIDER_VERSION": f"v{expected}",
        },
    )
    results.append(("install-script", install))
    installed_haider = install_prefix / "haider"
    installed_haiderd = install_prefix / "haiderd"
    if install.timed_out or install.returncode != 0:
        status = ENV_BLOCKED if _network_failure(install.stderr) else FAIL
        return [
            Evidence(
                "install_paths",
                status,
                f"install.sh expected_exit=0 actual={install.returncode} "
                f"timed_out={str(install.timed_out).lower()} stderr={install.stderr.strip()!r}",
                [ctx.command_artefact(name, result) for name, result in results],
            )
        ]
    if not installed_haider.is_file() or not installed_haiderd.is_file():
        failures.append(
            "scratch prefix pair expected=haider,haiderd actual="
            f"{installed_haider.is_file()},{installed_haiderd.is_file()} prefix={install_prefix}"
        )
    if failures:
        return [
            Evidence(
                "install_paths",
                FAIL,
                "; ".join(failures),
                [ctx.command_artefact(name, result) for name, result in results],
            )
        ]

    installed_version = ctx.run_binary(
        installed_haider, ["--version"], timeout=VERSION_QUERY
    )
    results.append(("installed-version", installed_version))
    actual_version = _version(installed_version.stdout, "haider")
    if (
        installed_version.timed_out
        or installed_version.returncode != 0
        or actual_version != expected
    ):
        failures.append(
            f"installed version expected={expected!r}/0 actual={actual_version!r}/"
            f"{installed_version.returncode}"
        )

    ready = ctx.run_binary(
        installed_haider, ["--ready"], timeout=DAEMON_STARTUP, may_spawn=True
    )
    results.append(("ready", ready))
    if ready.timed_out or ready.returncode != 0:
        failures.append(
            f"installed --ready expected_exit=0 actual={ready.returncode} "
            f"timed_out={str(ready.timed_out).lower()}"
        )

    status_result = ctx.run_binary(
        installed_haider,
        ["status", "--json"],
        timeout=STATUS_REQUEST,
        may_spawn=True,
    )
    results.append(("status", status_result))
    try:
        status = parse_single_json(status_result.stdout, "installed status")
    except Exception as error:
        status = {}
        failures.append(f"installed status JSON actual={error}")
    if status_result.timed_out or status_result.returncode != 0:
        failures.append(
            f"installed status expected_exit=0 actual={status_result.returncode} "
            f"timed_out={str(status_result.timed_out).lower()}"
        )
    failures.extend(ctx.observe_status(status))
    daemon = status.get("daemon") if isinstance(status.get("daemon"), dict) else {}
    pid = daemon.get("pid")
    if ctx.ownership_refused or not isinstance(pid, int) or isinstance(pid, bool):
        failures.append(f"installed daemon ownership expected=trusted actual_pid={pid!r}")
        return [
            Evidence(
                "install_paths",
                FAIL,
                "; ".join(failures),
                [ctx.command_artefact(name, result) for name, result in results],
            )
        ]
    if daemon.get("ready") is not True or daemon.get("version") != expected:
        failures.append(
            f"installed daemon expected=ready/{expected!r} "
            f"actual={daemon.get('ready')!r}/{daemon.get('version')!r}"
        )

    stop = ctx.run_binary(
        installed_haider,
        ["daemon", "stop", "--json"],
        timeout=DAEMON_STOP + PROCESS_EXIT_GRACE,
    )
    results.append(("stop", stop))
    try:
        stop_document = parse_single_json(stop.stdout, "installed daemon stop")
    except Exception as error:
        stop_document = {}
        failures.append(f"installed daemon stop JSON actual={error}")
    if stop.timed_out or stop.returncode != 0 or stop_document.get("outcome") != "stopped_cleanly":
        failures.append(
            "installed stop expected=stopped_cleanly/0 actual="
            f"{stop_document.get('outcome')!r}/{stop.returncode}"
        )
    if not isinstance(pid, int) or not wait_pid_gone(pid, PROCESS_EXIT_GRACE):
        failures.append(f"installed pid gone expected=true actual=false pid={pid!r}")

    if failures:
        return [
            Evidence(
                "install_paths",
                FAIL,
                "; ".join(failures),
                [ctx.command_artefact(name, result) for name, result in results],
            )
        ]
    return [
        Evidence(
            "install_paths",
            PASS,
            f"installer_exit=0 scratch_home=true prefix={install_prefix} pair_present=true "
            f"version={expected} ready_exit=0 status_ready=true daemon_pid={pid} "
            "stop=stopped_cleanly pid_gone=true",
        )
    ]
