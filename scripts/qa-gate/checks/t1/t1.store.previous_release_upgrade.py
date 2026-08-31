"""Create a real previous-release profile and prove installed-binary upgrade."""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import sqlite3
import tarfile
from typing import Any

from gate import (
    DAEMON_DRAIN,
    DAEMON_STARTUP,
    DAEMON_STOP,
    ENV_BLOCKED,
    FAIL,
    PASS,
    PROCESS_EXIT_GRACE,
    RUN_TERMINAL_GRACE,
    RUN_TIMEOUT,
    STATUS_REQUEST,
    VERSION_QUERY,
    BudgetPart,
    Evidence,
)
from gate.context import parse_single_json, wait_pid_gone

id = "t1.store.previous_release_upgrade"
tier = "t1"
area = "store"
needs = ("binary", "daemon", "network:github")
PREVIOUS_VERSION = "0.0.966"
PREVIOUS_SCHEMA_VERSION = 27
TARGET = "aarch64-apple-darwin"
ASSET = f"haider-v{PREVIOUS_VERSION}-{TARGET}.tar.xz"
ASSET_URL = (
    f"https://github.com/Rizzist/haider-agent/releases/download/v{PREVIOUS_VERSION}/{ASSET}"
)
SIDECAR_URL = ASSET_URL + ".sha256"
PINNED_SHA256 = "69735f821cb4406f12baad5d2e10981182a260edf1e52d35081d1e964b30dd6e"
# v0.0.966 instantiates its fake script per session, so both old sessions
# consume its first segment. Distinct durable IDs prove two real old turns.
OLD_SENTINELS = ("QA_GATE_966_TURN", "QA_GATE_966_TURN")
NEW_SENTINEL = "QA_GATE_UPGRADED_TURN"
script = [
    {"step": "emit_text", "text": OLD_SENTINELS[0]},
    {"step": "finish", "reason": "end_turn"},
    {"step": "emit_text", "text": OLD_SENTINELS[1]},
    {"step": "finish", "reason": "end_turn"},
    {"step": "emit_text", "text": NEW_SENTINEL},
    {"step": "finish", "reason": "end_turn"},
]
turns_expected = 3
timed = True

RELEASE_FETCH = BudgetPart(
    "one pinned GitHub release fetch",
    60.0,
    "curl --max-time 60 for the pinned archive or checksum sidecar",
)
ARCHIVE_VERIFY_EXTRACT = BudgetPart(
    "release archive verification and XZ extraction",
    30.0,
    "stdlib SHA-256 plus traversal-checked tarfile r:xz extraction",
)
SQLITE_COMPARE_CAPTURE = BudgetPart(
    "SQLite inspection and next-release fixture capture",
    30.0,
    "read-only PRAGMA/sqlite_master comparison plus stopped-profile XZ capture",
)
LEGACY_IDENTITY = BudgetPart(
    "v0.0.966 PID-file executable identity",
    10.0,
    "one bounded lsof/procfs identity proof for the legacy status-derived PID file",
)
# Registry #94: archive+sidecar 60+60; verify/extract 30; old pair versions
# 30+30; old runs (30+30+2)+(30+2); old sessions 60; old legacy status 60
# + identity 10; old stop 20+2 plus drain 5+PID obs 2; upgraded sessions
# 30+60; current run 30+2; current status 60; stop 20+2 plus PID obs 2;
# fresh status 30+60; stop 20+2 plus PID obs 2; SQLite/capture 30;
# cleanup status 60 + stop 20+2 + three historical PID observations. Total=901s.
budget = (
    RELEASE_FETCH
    + RELEASE_FETCH
    + ARCHIVE_VERIFY_EXTRACT
    + VERSION_QUERY
    + VERSION_QUERY
    + DAEMON_STARTUP
    + RUN_TIMEOUT
    + RUN_TERMINAL_GRACE
    + RUN_TIMEOUT
    + RUN_TERMINAL_GRACE
    + STATUS_REQUEST
    + STATUS_REQUEST
    + LEGACY_IDENTITY
    + DAEMON_STOP
    + PROCESS_EXIT_GRACE
    + DAEMON_DRAIN
    + PROCESS_EXIT_GRACE
    + DAEMON_STARTUP
    + STATUS_REQUEST
    + RUN_TIMEOUT
    + RUN_TERMINAL_GRACE
    + STATUS_REQUEST
    + DAEMON_STOP
    + PROCESS_EXIT_GRACE
    + PROCESS_EXIT_GRACE
    + DAEMON_STARTUP
    + STATUS_REQUEST
    + DAEMON_STOP
    + PROCESS_EXIT_GRACE
    + PROCESS_EXIT_GRACE
    + SQLITE_COMPARE_CAPTURE
    + STATUS_REQUEST
    + DAEMON_STOP
    + PROCESS_EXIT_GRACE
    + PROCESS_EXIT_GRACE
    + PROCESS_EXIT_GRACE
    + PROCESS_EXIT_GRACE
)


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        while chunk := handle.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def _json_lines(stdout: str, label: str) -> list[dict[str, Any]]:
    values = []
    for index, line in enumerate(stdout.splitlines(), start=1):
        if not line.strip():
            continue
        value = json.loads(line)
        if not isinstance(value, dict):
            raise ValueError(f"{label} line={index} expected=object")
        values.append(value)
    if not values:
        raise ValueError(f"{label} expected JSON lines actual=0")
    return values


def _version(output: str, product: str) -> str | None:
    line = output.strip()
    prefix = f"{product} "
    return line.removeprefix(prefix) if line.startswith(prefix) else None


def _store_version(path: Path) -> int:
    with sqlite3.connect(f"file:{path}?mode=ro", uri=True) as connection:
        return int(connection.execute("PRAGMA user_version").fetchone()[0])


def _schema(path: Path) -> list[tuple[str, str, str, str]]:
    with sqlite3.connect(f"file:{path}?mode=ro", uri=True) as connection:
        return [
            (str(kind), str(name), str(table), str(sql or ""))
            for kind, name, table, sql in connection.execute(
                "SELECT type,name,tbl_name,COALESCE(sql,'') "
                "FROM sqlite_master ORDER BY type,name,tbl_name"
            )
        ]


def _schema_sha(rows: list[tuple[str, str, str, str]]) -> str:
    encoded = json.dumps(rows, separators=(",", ":"), ensure_ascii=False).encode()
    return hashlib.sha256(encoded).hexdigest()


def _extract_archive(archive: Path, destination: Path) -> None:
    destination.mkdir(mode=0o700)
    boundary = os.path.realpath(destination)
    with tarfile.open(archive, mode="r:xz") as bundle:
        for member in bundle.getmembers():
            target = os.path.realpath(destination / member.name)
            if os.path.commonpath((target, boundary)) != boundary:
                raise ValueError(f"archive member escapes extraction root: {member.name!r}")
            if member.issym() or member.islnk():
                raise ValueError(f"archive contains link member: {member.name!r}")
        bundle.extractall(destination)


def _run_turn(ctx, binary: Path, prompt: str, timeout, *, may_spawn: bool):
    result = ctx.run_binary(
        binary,
        [
            "run",
            "-p",
            prompt,
            "--provider",
            "fake",
            "--model",
            "fake-model",
            "--output",
            "json",
            "--timeout",
            "30s",
        ],
        timeout=timeout,
        may_spawn=may_spawn,
    )
    values = _json_lines(result.stdout, prompt)
    return result, values[0], values[-1]


def _network_blocked(returncode: int) -> bool:
    return returncode in {5, 6, 7, 28, 35, 52, 55, 56}


def _command_artefacts(ctx, results) -> list[str]:
    return [ctx.command_artefact(name, result) for name, result in results]


def run(ctx) -> list[Evidence]:
    if platform.system() != "Darwin" or platform.machine().lower() not in {"arm64", "aarch64"}:
        return [
            Evidence(
                "previous_release_platform",
                ENV_BLOCKED,
                "missing need: pinned previous-release fixture requires "
                f"Darwin arm64 actual={platform.system()} {platform.machine()}",
            )
        ]
    curl = shutil.which("curl")
    if curl is None:
        return [
            Evidence(
                "previous_release_download",
                ENV_BLOCKED,
                "missing need: curl unavailable for bounded GitHub release download",
            )
        ]
    if shutil.which("lsof") is None:
        return [
            Evidence(
                "previous_release_identity",
                ENV_BLOCKED,
                "missing need: lsof unavailable for exact legacy daemon identity",
            )
        ]

    failures: list[str] = []
    results = []
    download_dir = ctx.root / "download"
    download_dir.mkdir(mode=0o700)
    archive = download_dir / ASSET
    sidecar = download_dir / f"{ASSET}.sha256"
    for label, url, destination in (
        ("release-archive", ASSET_URL, archive),
        ("release-sidecar", SIDECAR_URL, sidecar),
    ):
        fetched = ctx.run_command(
            [
                curl,
                "--fail",
                "--location",
                "--silent",
                "--show-error",
                "--max-time",
                "60",
                "--output",
                destination,
                url,
            ],
            timeout=RELEASE_FETCH,
        )
        results.append((label, fetched))
        if fetched.timed_out or fetched.returncode != 0:
            status = ENV_BLOCKED if _network_blocked(fetched.returncode) else FAIL
            return [
                Evidence(
                    "previous_release_download",
                    status,
                    f"release download expected_exit=0 actual={fetched.returncode} "
                    f"timed_out={str(fetched.timed_out).lower()} url={url!r} "
                    f"stderr={fetched.stderr.strip()!r}",
                    _command_artefacts(ctx, results),
                )
            ]

    try:
        fields = sidecar.read_text(encoding="utf-8").split()
        sidecar_sha = fields[0].lower() if fields else ""
        actual_sha = _sha256(archive)
    except OSError as error:
        return [Evidence("previous_release_checksum", FAIL, f"checksum read actual={error}")]
    if sidecar_sha != PINNED_SHA256 or actual_sha != PINNED_SHA256:
        return [
            Evidence(
                "previous_release_checksum",
                FAIL,
                f"archive sha256 expected={PINNED_SHA256} "
                f"sidecar={sidecar_sha!r} actual={actual_sha!r}",
            )
        ]

    extracted = ctx.root / "previous"
    try:
        _extract_archive(archive, extracted)
    except (OSError, tarfile.TarError, ValueError) as error:
        return [Evidence("previous_release_extract", FAIL, f"archive extract actual={error}")]
    bundle = extracted / f"haider-v{PREVIOUS_VERSION}-{TARGET}"
    old_haider = bundle / "haider"
    old_haiderd = bundle / "haiderd"
    if not old_haider.is_file() or not old_haiderd.is_file():
        return [
            Evidence(
                "previous_release_extract",
                FAIL,
                f"archive pair expected=haider,haiderd actual={old_haider.is_file()},"
                f"{old_haiderd.is_file()} bundle={bundle}",
            )
        ]

    old_cli_version = ctx.run_binary(old_haider, ["--version"], timeout=VERSION_QUERY)
    old_daemon_version = ctx.run_binary(old_haiderd, ["--version"], timeout=VERSION_QUERY)
    results.extend(
        (("old-haider-version", old_cli_version), ("old-haiderd-version", old_daemon_version))
    )
    if _version(old_cli_version.stdout, "haider") != PREVIOUS_VERSION:
        failures.append(
            f"old haider version expected={PREVIOUS_VERSION} actual={old_cli_version.stdout.strip()!r}"
        )
    if _version(old_daemon_version.stdout, "haiderd") != PREVIOUS_VERSION:
        failures.append(
            f"old haiderd version expected={PREVIOUS_VERSION} "
            f"actual={old_daemon_version.stdout.strip()!r}"
        )

    try:
        first_result, first_accepted, first_turn = _run_turn(
            ctx,
            old_haider,
            "previous turn one",
            DAEMON_STARTUP + RUN_TIMEOUT + RUN_TERMINAL_GRACE,
            may_spawn=True,
        )
        second_result, second_accepted, second_turn = _run_turn(
            ctx,
            old_haider,
            "previous turn two",
            RUN_TIMEOUT + RUN_TERMINAL_GRACE,
            may_spawn=True,
        )
        results.extend((("old-turn-one", first_result), ("old-turn-two", second_result)))
    except Exception as error:
        failures.append(f"old fake turns JSON actual={error}")
        first_accepted = second_accepted = first_turn = second_turn = {}
        first_result = second_result = None
    old_session_ids = [first_accepted.get("session_id"), second_accepted.get("session_id")]
    for index, (result, turn, sentinel) in enumerate(
        (
            (first_result, first_turn, OLD_SENTINELS[0]),
            (second_result, second_turn, OLD_SENTINELS[1]),
        ),
        start=1,
    ):
        if result is None or result.timed_out or result.returncode != 0:
            failures.append(
                f"old turn {index} expected_exit=0 actual={getattr(result, 'returncode', None)!r} "
                f"timed_out={getattr(result, 'timed_out', None)!r}"
            )
        if turn.get("outcome") != "done" or turn.get("response") != sentinel:
            failures.append(
                f"old turn {index} expected=done/{sentinel!r} "
                f"actual={turn.get('outcome')!r}/{turn.get('response')!r}"
            )
    if not all(isinstance(value, str) and value for value in old_session_ids):
        failures.append(f"old session ids expected=two nonempty actual={old_session_ids!r}")

    old_sessions_result = ctx.run_binary(
        old_haider, ["sessions", "--json"], timeout=STATUS_REQUEST
    )
    results.append(("old-sessions", old_sessions_result))
    try:
        old_sessions = parse_single_json(old_sessions_result.stdout, "old sessions")
    except Exception as error:
        old_sessions = {}
        failures.append(f"old sessions JSON actual={error}")
    if old_sessions_result.timed_out or old_sessions_result.returncode != 0:
        failures.append(
            f"old sessions expected_exit=0 actual={old_sessions_result.returncode} "
            f"timed_out={str(old_sessions_result.timed_out).lower()}"
        )
    listed_old_ids = {
        row.get("id") for row in old_sessions.get("sessions", []) if isinstance(row, dict)
    }
    if not set(old_session_ids).issubset(listed_old_ids):
        failures.append(
            f"old sessions expected_ids={old_session_ids!r} "
            f"actual={sorted(map(str, listed_old_ids))!r}"
        )

    old_status_result = ctx.run_binary(
        old_haider, ["status", "--json"], timeout=STATUS_REQUEST, may_spawn=True
    )
    results.append(("old-status", old_status_result))
    try:
        old_status = parse_single_json(old_status_result.stdout, "old status")
    except Exception as error:
        old_status = {}
        failures.append(f"old status JSON actual={error}")
    if old_status_result.timed_out or old_status_result.returncode != 0:
        failures.append(
            f"old status expected_exit=0 actual={old_status_result.returncode} "
            f"timed_out={str(old_status_result.timed_out).lower()}"
        )
    old_pid, legacy_problems = ctx.observe_legacy_status(
        old_status,
        daemon_binary=old_haiderd,
        expected_version=PREVIOUS_VERSION,
        identity_timeout=LEGACY_IDENTITY,
    )
    failures.extend(legacy_problems)
    if old_pid is None:
        return [
            Evidence(
                "previous_release_upgrade",
                FAIL,
                "; ".join(failures or ["legacy ownership expected=trusted actual=untrusted"]),
                _command_artefacts(ctx, results),
            )
        ]
    old_stop = ctx.run_haider(
        ["daemon", "stop", "--json"], timeout=DAEMON_STOP + PROCESS_EXIT_GRACE
    )
    results.append(("old-daemon-stop", old_stop))
    try:
        old_stop_document = parse_single_json(old_stop.stdout, "old daemon stop")
    except Exception as error:
        old_stop_document = {}
        failures.append(f"old daemon stop JSON actual={error}")
    old_stop_compatible = (
        old_stop.returncode == 0 and old_stop_document.get("outcome") == "stopped_cleanly"
    ) or (
        old_stop.returncode == 124
        and old_stop_document.get("outcome") == "did_not_stop"
        and old_stop_document.get("phase") == "completion_receipt"
    )
    if old_stop.timed_out or not old_stop_compatible:
        failures.append(
            "old daemon stop expected=stopped_cleanly/0 or legacy completion_receipt/124 "
            f"actual={old_stop_document.get('outcome')!r}/{old_stop_document.get('phase')!r}/"
            f"{old_stop.returncode}"
        )
    if not isinstance(old_pid, int) or not wait_pid_gone(
        old_pid, DAEMON_DRAIN + PROCESS_EXIT_GRACE
    ):
        failures.append(f"old daemon pid gone expected=true actual=false pid={old_pid!r}")

    store_path = ctx.profile_dir / "store.sqlite"
    try:
        old_schema_version = _store_version(store_path)
    except (OSError, sqlite3.Error, TypeError) as error:
        failures.append(f"old PRAGMA user_version actual={error}")
        old_schema_version = None
    if old_schema_version != PREVIOUS_SCHEMA_VERSION:
        failures.append(
            f"v{PREVIOUS_VERSION} profile user_version expected={PREVIOUS_SCHEMA_VERSION} "
            f"actual={old_schema_version!r}"
        )

    ctx.set_fake_provider_script(script[4:])
    upgraded_sessions_result = ctx.run_haider(
        ["sessions", "--json"], timeout=DAEMON_STARTUP + STATUS_REQUEST
    )
    results.append(("upgraded-sessions", upgraded_sessions_result))
    try:
        upgraded_sessions = parse_single_json(
            upgraded_sessions_result.stdout, "upgraded sessions"
        )
    except Exception as error:
        upgraded_sessions = {}
        failures.append(f"upgraded sessions JSON actual={error}")
    if upgraded_sessions_result.timed_out or upgraded_sessions_result.returncode != 0:
        failures.append(
            f"upgraded sessions expected_exit=0 actual={upgraded_sessions_result.returncode} "
            f"timed_out={str(upgraded_sessions_result.timed_out).lower()}"
        )
    upgraded_ids = {
        row.get("id") for row in upgraded_sessions.get("sessions", []) if isinstance(row, dict)
    }
    if not set(old_session_ids).issubset(upgraded_ids):
        failures.append(
            f"upgraded sessions expected_old_ids={old_session_ids!r} "
            f"actual={sorted(map(str, upgraded_ids))!r}"
        )

    try:
        new_run, new_accepted, new_turn = _run_turn(
            ctx,
            ctx.haider_bin,
            "turn after previous-release upgrade",
            RUN_TIMEOUT + RUN_TERMINAL_GRACE,
            may_spawn=True,
        )
        results.append(("upgraded-turn", new_run))
    except Exception as error:
        failures.append(f"upgraded turn JSON actual={error}")
        new_run = None
        new_accepted = new_turn = {}
    if new_run is None or new_run.timed_out or new_run.returncode != 0:
        failures.append(
            f"upgraded turn expected_exit=0 actual={getattr(new_run, 'returncode', None)!r}"
        )
    if new_turn.get("outcome") != "done" or new_turn.get("response") != NEW_SENTINEL:
        failures.append(
            f"upgraded turn expected=done/{NEW_SENTINEL!r} "
            f"actual={new_turn.get('outcome')!r}/{new_turn.get('response')!r}"
        )

    current_status_result = ctx.run_haider(["status", "--json"], timeout=STATUS_REQUEST)
    results.append(("current-status", current_status_result))
    try:
        current_status = parse_single_json(current_status_result.stdout, "current status")
    except Exception as error:
        current_status = {}
        failures.append(f"current status JSON actual={error}")
    if current_status_result.timed_out or current_status_result.returncode != 0:
        failures.append(
            f"current status expected_exit=0 actual={current_status_result.returncode} "
            f"timed_out={str(current_status_result.timed_out).lower()}"
        )
    failures.extend(ctx.observe_status(current_status))
    current_daemon = (
        current_status.get("daemon") if isinstance(current_status.get("daemon"), dict) else {}
    )
    current_pid = current_daemon.get("pid")
    current_version = current_daemon.get("version")
    if ctx.ownership_refused or not isinstance(current_pid, int) or isinstance(current_pid, bool):
        failures.append(f"current daemon ownership expected=trusted actual_pid={current_pid!r}")
        return [
            Evidence(
                "previous_release_upgrade",
                FAIL,
                "; ".join(failures),
                _command_artefacts(ctx, results),
            )
        ]
    current_stop = ctx.run_haider(
        ["daemon", "stop", "--json"], timeout=DAEMON_STOP + PROCESS_EXIT_GRACE
    )
    results.append(("current-stop", current_stop))
    try:
        current_stop_document = parse_single_json(current_stop.stdout, "current stop")
    except Exception as error:
        current_stop_document = {}
        failures.append(f"current stop JSON actual={error}")
    if (
        current_stop.timed_out
        or current_stop.returncode != 0
        or current_stop_document.get("outcome") != "stopped_cleanly"
    ):
        failures.append(
            "current stop expected=stopped_cleanly/0 actual="
            f"{current_stop_document.get('outcome')!r}/{current_stop.returncode}"
        )
    if not isinstance(current_pid, int) or not wait_pid_gone(current_pid, PROCESS_EXIT_GRACE):
        failures.append(f"current pid gone expected=true actual=false pid={current_pid!r}")

    try:
        upgraded_schema_version = _store_version(store_path)
        upgraded_schema = _schema(store_path)
    except (OSError, sqlite3.Error, TypeError) as error:
        failures.append(f"upgraded SQLite inspection actual={error}")
        upgraded_schema_version = None
        upgraded_schema = []

    fresh_profile = ctx.root / "fresh-profile"
    fresh_runtime = ctx.root / "fresh-runtime"
    fresh_home = ctx.root / "fresh-home"
    for directory in (fresh_profile, fresh_runtime, fresh_home):
        directory.mkdir(mode=0o700)
    fresh_env = {
        "HAIDER_PROFILE_DIR": fresh_profile,
        "HAIDER_RUNTIME_DIR": fresh_runtime,
        "HOME": fresh_home,
        "USERPROFILE": fresh_home,
        "XDG_CACHE_HOME": fresh_home / ".cache",
        "XDG_CONFIG_HOME": fresh_home / ".config",
        "XDG_DATA_HOME": fresh_home / ".local" / "share",
        "XDG_STATE_HOME": fresh_home / ".local" / "state",
        "HAIDER_TEST_FAKE_PROVIDER": json.dumps(
            [{"step": "finish", "reason": "end_turn"}], separators=(",", ":")
        ),
    }
    fresh_status_result = ctx.run_haider(
        ["status", "--json"],
        timeout=DAEMON_STARTUP + STATUS_REQUEST,
        env_overrides=fresh_env,
    )
    results.append(("fresh-status", fresh_status_result))
    try:
        fresh_status = parse_single_json(fresh_status_result.stdout, "fresh status")
    except Exception as error:
        fresh_status = {}
        failures.append(f"fresh status JSON actual={error}")
    if fresh_status_result.timed_out or fresh_status_result.returncode != 0:
        failures.append(
            f"fresh status expected_exit=0 actual={fresh_status_result.returncode} "
            f"timed_out={str(fresh_status_result.timed_out).lower()}"
        )
    failures.extend(
        ctx.observe_status(
            fresh_status, profile_dir=fresh_profile, runtime_root=fresh_runtime
        )
    )
    fresh_daemon = (
        fresh_status.get("daemon") if isinstance(fresh_status.get("daemon"), dict) else {}
    )
    fresh_pid = fresh_daemon.get("pid")
    if ctx.ownership_refused or not isinstance(fresh_pid, int) or isinstance(fresh_pid, bool):
        failures.append(f"fresh daemon ownership expected=trusted actual_pid={fresh_pid!r}")
        return [
            Evidence(
                "previous_release_upgrade",
                FAIL,
                "; ".join(failures),
                _command_artefacts(ctx, results),
            )
        ]
    fresh_stop = ctx.run_haider(
        ["daemon", "stop", "--json"],
        timeout=DAEMON_STOP + PROCESS_EXIT_GRACE,
        env_overrides=fresh_env,
    )
    results.append(("fresh-stop", fresh_stop))
    try:
        fresh_stop_document = parse_single_json(fresh_stop.stdout, "fresh stop")
    except Exception as error:
        fresh_stop_document = {}
        failures.append(f"fresh stop JSON actual={error}")
    if (
        fresh_stop.timed_out
        or fresh_stop.returncode != 0
        or fresh_stop_document.get("outcome") != "stopped_cleanly"
    ):
        failures.append(
            f"fresh stop expected=stopped_cleanly/0 actual="
            f"{fresh_stop_document.get('outcome')!r}/{fresh_stop.returncode}"
        )
    if not isinstance(fresh_pid, int) or not wait_pid_gone(fresh_pid, PROCESS_EXIT_GRACE):
        failures.append(f"fresh pid gone expected=true actual=false pid={fresh_pid!r}")

    fresh_store = fresh_profile / "store.sqlite"
    try:
        fresh_schema_version = _store_version(fresh_store)
        fresh_schema = _schema(fresh_store)
    except (OSError, sqlite3.Error, TypeError) as error:
        failures.append(f"fresh SQLite inspection actual={error}")
        fresh_schema_version = None
        fresh_schema = []
    if upgraded_schema_version != fresh_schema_version:
        failures.append(
            f"upgraded user_version expected=new_binary({fresh_schema_version!r}) "
            f"actual={upgraded_schema_version!r}"
        )
    if upgraded_schema != fresh_schema:
        mismatch = next(
            (
                (index, left, right)
                for index, (left, right) in enumerate(zip(upgraded_schema, fresh_schema))
                if left != right
            ),
            None,
        )
        failures.append(
            f"sqlite_master expected=fresh exact actual_mismatch={mismatch!r} "
            f"upgraded_rows={len(upgraded_schema)} fresh_rows={len(fresh_schema)}"
        )

    published: list[str] = []
    if not isinstance(current_version, str) or not current_version:
        failures.append(f"binary under test version expected=nonempty actual={current_version!r}")
    if not failures:
        fixture_name = (
            f"next-release-profile-v{current_version}-from-v{PREVIOUS_VERSION}.tar.xz"
        )
        fixture = ctx.root / fixture_name
        with tarfile.open(fixture, mode="w:xz") as bundle_file:
            bundle_file.add(ctx.profile_dir, arcname="profile", recursive=True)
        fixture_sha = _sha256(fixture)
        fixture_sidecar = ctx.root / f"{fixture_name}.sha256"
        fixture_sidecar.write_text(f"{fixture_sha}  {fixture_name}\n", encoding="utf-8")
        manifest = ctx.root / f"{fixture_name}.json"
        manifest.write_text(
            json.dumps(
                {
                    "schema": "haider.qa-gate.store-fixture.v1",
                    "source": {"url": ASSET_URL, "sha256": PINNED_SHA256},
                    "previous_version": PREVIOUS_VERSION,
                    "binary_under_test_version": current_version,
                    "previous_schema_version": old_schema_version,
                    "captured_schema_version": upgraded_schema_version,
                    "sqlite_master_sha256": _schema_sha(upgraded_schema),
                    "old_session_ids": old_session_ids,
                    "new_session_id": new_accepted.get("session_id"),
                    "fixture_sha256": fixture_sha,
                },
                indent=2,
                sort_keys=True,
            )
            + "\n",
            encoding="utf-8",
        )
        published = [
            ctx.publish_artefact(fixture.name, fixture),
            ctx.publish_artefact(fixture_sidecar.name, fixture_sidecar),
            ctx.publish_artefact(manifest.name, manifest),
        ]

    if failures:
        return [
            Evidence(
                "previous_release_upgrade",
                FAIL,
                "; ".join(failures),
                [*_command_artefacts(ctx, results), *published],
            )
        ]
    return [
        Evidence(
            "previous_release_upgrade",
            PASS,
            f"source=v{PREVIOUS_VERSION} sha256={actual_sha} old_user_version="
            f"{old_schema_version} old_sessions=2 listed_after_upgrade=true "
            f"current_turn=done new_user_version={upgraded_schema_version} "
            f"fresh_user_version={fresh_schema_version} sqlite_master_equal=true "
            f"schema_sha256={_schema_sha(upgraded_schema)} fixture={Path(published[0]).name}",
            published,
        )
    ]
