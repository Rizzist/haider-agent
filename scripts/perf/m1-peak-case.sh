#!/bin/sh
# Exact M1 peak-RSS case from rss/a.md section 4.
set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/../.." && pwd)
SAMPLER="$SCRIPT_DIR/m1-rss-sampler.py"
BENCH_ROOT=${M1_BENCH_ROOT:-/Users/rizzist/Documents/CODING/haidercode-web}
HAIDER_BIN=${M1_HAIDER_BIN:-/Users/rizzist/.claude/jobs/1a0b5b6c/tmp/bench967e/bin/haider}
OUTPUT_ROOT=${M1_OUTPUT_ROOT:-$REPO_ROOT/target/m1-rss}
REGION_SNAPSHOT_TOOL=${M1_REGION_SNAPSHOT_TOOL:-}
REGION_SNAPSHOT_THRESHOLD_BYTES=${M1_REGION_SNAPSHOT_THRESHOLD_BYTES:-0}
REGION_SNAPSHOT_MIN_GROWTH_BYTES=${M1_REGION_SNAPSHOT_MIN_GROWTH_BYTES:-1}
RUNS=5
MAX_LOAD_1M=3
proxy_pid=
sampler_pid=
cli_pid=
self_child=
self_sampler=

cleanup_children() {
    for child in "$cli_pid" "$sampler_pid" "$proxy_pid" "$self_sampler" "$self_child"; do
        if [ -n "$child" ]; then
            kill "$child" 2>/dev/null || true
            wait "$child" 2>/dev/null || true
        fi
    done
}

trap cleanup_children EXIT HUP INT TERM

usage() {
    echo "usage: $0 [--self-test] [--runs 5]" >&2
}

load_1m() {
    python3 - <<'PY'
import os
print(f"{os.getloadavg()[0]:.2f}")
PY
}

self_test() {
    self_root=$(mktemp -d "${TMPDIR:-/tmp}/haider-m1-selftest.XXXXXX")
    self_pid_file="$self_root/root.pid"
    self_stop="$self_root/stop"
    self_samples="$self_root/samples.tsv"
    python3 - "$self_pid_file" <<'PY' &
import os
from pathlib import Path
import sys
import time

allocation = bytearray(8 * 1024 * 1024)
allocation[0] = 1
child = os.fork()
if child == 0:
    child_allocation = bytearray(8 * 1024 * 1024)
    child_allocation[0] = 1
    time.sleep(10)
    raise SystemExit(0)
Path(sys.argv[1]).write_text(f"{os.getpid()}\n", encoding="ascii")
time.sleep(10)
PY
    self_child=$!
    python3 "$SAMPLER" \
        --output "$self_samples" \
        --root-pid-file "$self_pid_file" \
        --stop-file "$self_stop" \
        --label-all-descendants-as-daemon \
        --require-daemon &
    self_sampler=$!
    python3 - "$self_pid_file" "$self_samples" <<'PY'
from pathlib import Path
import sys
import time

path = Path(sys.argv[1])
samples = Path(sys.argv[2])
deadline = time.monotonic() + 5
while not path.exists() and time.monotonic() < deadline:
    time.sleep(0.001)
if not path.exists():
    raise SystemExit("synthetic root PID was not published")
while time.monotonic() < deadline:
    try:
        rows = samples.read_text(encoding="ascii")
    except (FileNotFoundError, OSError):
        rows = ""
    if "\thaider\t" in rows and "\thaiderd\t" in rows:
        break
    time.sleep(0.001)
else:
    raise SystemExit("sampler did not publish root and descendant samples")
PY
    : > "$self_stop"
    sampler_status=0
    wait "$self_sampler" || sampler_status=$?
    kill "$self_child" 2>/dev/null || true
    wait "$self_child" 2>/dev/null || true
    self_child=
    self_sampler=
    if [ "$sampler_status" -ne 0 ]; then
        echo "M1 sampler self-test failed with status $sampler_status" >&2
        return "$sampler_status"
    fi
    python3 - "$self_samples" <<'PY'
from pathlib import Path
import sys

lines = [line.split("\t") for line in Path(sys.argv[1]).read_text(encoding="ascii").splitlines()[1:] if line]
rss = [int(line[4]) for line in lines]
if not rss or max(rss) <= 0:
    raise SystemExit("sampler self-test produced no positive RSS sample")
labels = {line[3] for line in lines}
if not {"haider", "haiderd"}.issubset(labels):
    raise SystemExit(f"sampler self-test missed root/descendant labels: {labels}")
print(f"M1 sampler self-test: PASS samples={len(rss)} max_rss_bytes={max(rss)}")
PY
    rm -rf -- "$self_root"
}

start_proxy() {
    run_root=$1
    proxy_state_file=$2
    proxy_stop_file=$3
    proxy_result_file=$4
    python3 - "$BENCH_ROOT" "$run_root" "$proxy_state_file" "$proxy_stop_file" "$proxy_result_file" <<'PY' &
import json
import os
from pathlib import Path
import secrets
import sys
import time

bench_root = Path(sys.argv[1]).resolve()
run_root = Path(sys.argv[2]).resolve()
state_file = Path(sys.argv[3])
stop_file = Path(sys.argv[4])
result_file = Path(sys.argv[5])
sys.path.insert(0, str(bench_root))

from bench.conformance.fake_proxy import ProxyState, fake_proxy
from bench.conformance.workspace import prepare_workspace

credential = "bench-secret-" + secrets.token_hex(24)
state = ProxyState(
    scenario="patch_truncated",
    expected_model="deepseek-v4-flash",
    credential=credential,
    timeout_seconds=20,
    truncation_bytes=1_114_112,
)
_base, workspace = prepare_workspace(run_root)
home = run_root / "home"
config = run_root / "config" / "haider-agent"
for directory in (
    home,
    config,
    config / "vault",
    config / "xdg",
    run_root / "data",
    run_root / "state",
    run_root / "tmp",
):
    directory.mkdir(parents=True, exist_ok=True)

with fake_proxy(state) as proxy:
    base_url = proxy.base_url + "/v1"
    providers = {
        "providers": [
            {
                "provider_id": "bench-proxy",
                "display_name": "benchmark proxy",
                "api_family": "openai_chat_completions",
                "base_url": base_url,
                "enabled": True,
                "auth_requirement": "api_key",
                "configured_models": ["deepseek-v4-flash"],
                "default_model": "deepseek-v4-flash",
                "provenance": "custom",
            }
        ]
    }
    accounts = [
        {
            "alias": "bench-proxy",
            "provider": "bench-proxy",
            "auth_method": "api_key",
            "identity": "benchmark proxy",
            "status": {"status": "ok"},
            "active": True,
        }
    ]
    (config / "providers.json").write_text(
        json.dumps(providers, separators=(",", ":")) + "\n", encoding="utf-8"
    )
    (config / "accounts.json").write_text(
        json.dumps(accounts, separators=(",", ":")) + "\n", encoding="utf-8"
    )
    vault = config / "vault" / "62656e63682d70726f7879.vault"
    vault.write_text(credential, encoding="utf-8")
    for path in (config / "providers.json", config / "accounts.json", vault):
        path.chmod(0o600)
    published = {
        "workspace": str(workspace),
        "home": str(home),
        "config": str(config),
        "data": str(run_root / "data"),
        "state": str(run_root / "state"),
        "tmp": str(run_root / "tmp"),
        "credential": credential,
        "base_url": base_url,
    }
    temporary = state_file.with_suffix(".tmp")
    temporary.write_text(json.dumps(published), encoding="utf-8")
    temporary.replace(state_file)
    deadline = time.monotonic() + 16 * 60
    while not stop_file.exists() and time.monotonic() < deadline:
        time.sleep(0.005)
    result_file.write_text(
        json.dumps(
            {
                "request_count": len(state.requests),
                "paths": [request.path for request in state.requests],
            },
            separators=(",", ":"),
        ),
        encoding="utf-8",
    )
PY
    proxy_pid=$!
}

analyze_run() {
    run_dir=$1
    load=$2
    python3 - "$run_dir" "$load" <<'PY'
import json
from pathlib import Path
import shutil
import sys

run = Path(sys.argv[1])
load = float(sys.argv[2])
jsonl = run / "m1-case.jsonl"
samples_path = run / "m1-rss.tsv"
proxy_result = json.loads((run / "proxy-result.json").read_text(encoding="utf-8"))

raw = jsonl.read_bytes()
if not (3_374_303 - 65_536 <= len(raw) <= 3_374_303 + 65_536):
    raise SystemExit(f"JSONL size {len(raw)} is outside 3,374,303 +/- 65,536")

def large_x(value):
    if isinstance(value, str):
        return len(value) >= 1_000_000 and set(value) == {"x"}
    if isinstance(value, list):
        return any(large_x(item) for item in value)
    if isinstance(value, dict):
        return any(large_x(item) for item in value.values())
    return False

t_delta = None
t_item = None
t_end = None
terminal_done = False
for line in raw.splitlines():
    event = json.loads(line)
    payload = event.get("payload", {})
    committed = event.get("committed_at_ms")
    if not isinstance(committed, int):
        continue
    if (
        payload.get("type") == "item"
        and payload.get("event") == "delta"
        and large_x(payload)
    ):
        t_delta = committed
    item = payload.get("item")
    if (
        payload.get("type") == "item"
        and payload.get("event") == "completed"
        and isinstance(item, dict)
        and item.get("item") == "agent_message"
        and len(item.get("text", "")) == 1_114_112
    ):
        t_item = committed
    if payload.get("type") == "run_state" and payload.get("state") == "done":
        t_end = committed
        terminal_done = True
if None in (t_delta, t_item, t_end) or not terminal_done:
    raise SystemExit("JSONL lacks the exact large delta/item/done anchors")

samples = []
for line in samples_path.read_text(encoding="ascii").splitlines()[1:]:
    if not line:
        continue
    wall, mono, pid, label, rss = line.split("\t")
    samples.append((int(wall), int(mono), int(pid), label, int(rss)))
daemon_by_pid = {}
for sample in samples:
    if sample[3] == "haiderd":
        daemon_by_pid.setdefault(sample[2], []).append(sample)
delta_ns = t_delta * 1_000_000
item_ns = t_item * 1_000_000
end_ns = t_end * 1_000_000
candidates = [
    values
    for values in daemon_by_pid.values()
    if min(row[0] for row in values) <= delta_ns - 50_000_000
    and max(row[0] for row in values) >= end_ns
]
if len(candidates) != 1:
    raise SystemExit(
        f"sampler did not identify exactly one daemon alive from t_delta-50ms through t_end: {len(candidates)}"
    )
daemon = candidates[0]
pre = [row for row in daemon if row[0] <= delta_ns - 1_000_000]
at_item = [row for row in daemon if row[0] >= item_ns]
window = [row for row in daemon if item_ns <= row[0] <= item_ns + 30_000_000]
cli = [row for row in samples if row[3] == "haider"]
if not pre or not at_item or not window or not cli:
    raise SystemExit("sampler missed one or more M1 metric windows")
r_pre = max(pre, key=lambda row: row[0])[4]
r_item = min(at_item, key=lambda row: row[0])[4]
r_max = max(row[4] for row in window)
r_cli_max = max(row[4] for row in cli)
region_snapshot = run / "daemon-regions.tsv"
selected_region = None
if region_snapshot.exists():
    prefix = f"{region_snapshot.stem}-"
    region_candidates = []
    for path in run.glob(f"{prefix}*{region_snapshot.suffix}"):
        identity = path.stem.removeprefix(prefix)
        try:
            wall_text, rss_text = identity.rsplit("-", 1)
            snapshot_wall_ns = int(wall_text)
            snapshot_rss = int(rss_text)
        except ValueError:
            continue
        metadata_fields = path.read_text(encoding="utf-8").splitlines()[0].split()
        metadata = {}
        try:
            for field in metadata_fields[1:]:
                key, value = field.split("=", 1)
                metadata[key] = int(value)
        except ValueError:
            continue
        capture_wall_ns = metadata.get("capture_wall_ns")
        if (
            item_ns <= snapshot_wall_ns <= item_ns + 30_000_000
            and isinstance(capture_wall_ns, int)
            and item_ns <= capture_wall_ns <= item_ns + 30_000_000
            and metadata.get("pid") == daemon[0][2]
        ):
            region_candidates.append(
                (snapshot_rss, snapshot_wall_ns, path, metadata)
            )
    if not region_candidates:
        raise SystemExit(
            "region helper did not preserve a capture inside the "
            "item..item+30ms window"
        )
    selected_region = max(
        region_candidates,
        key=lambda row: (row[3]["rss_bytes"], row[3]["capture_wall_ns"]),
    )
    shutil.copyfile(selected_region[2], run / "daemon-regions-peak.tsv")
summary = {
    "load_1m": load,
    "load_start_1m": load,
    "jsonl_bytes": len(raw),
    "proxy_request_count": proxy_result.get("request_count"),
    "daemon_pid": daemon[0][2],
    "t_delta_ms": t_delta,
    "t_item_ms": t_item,
    "t_end_ms": t_end,
    "R_pre_bytes": r_pre,
    "R_item_bytes": r_item,
    "R_max_bytes": r_max,
    "R_cli_max_bytes": r_cli_max,
    "delta_post_bytes": r_max - r_item,
    "sanity_growth_bytes": r_max - r_pre,
}
if selected_region is not None:
    summary.update(
        {
            "region_snapshot_file": "daemon-regions-peak.tsv",
            "region_snapshot_source_file": selected_region[2].name,
            "region_snapshot_trigger_wall_ns": selected_region[1],
            "region_snapshot_trigger_rss_bytes": selected_region[0],
            "region_snapshot_capture_wall_ns": selected_region[3]["capture_wall_ns"],
            "region_snapshot_capture_rss_bytes": selected_region[3]["rss_bytes"],
            "region_snapshot_capture_minus_sampler_rmax_bytes": (
                selected_region[3]["rss_bytes"] - r_max
            ),
            "region_snapshot_capture_footprint_bytes": selected_region[3][
                "footprint_bytes"
            ],
        }
    )
if proxy_result.get("request_count") != 2:
    raise SystemExit(f"fake proxy saw {proxy_result.get('request_count')} requests, expected 2")
(run / "summary.json").write_text(json.dumps(summary, indent=2) + "\n", encoding="utf-8")
print(json.dumps(summary, sort_keys=True))
PY
}

run_once() {
    index=$1
    load=$2
    timestamp=$(date -u +%Y%m%dT%H%M%SZ)
    run_dir="$OUTPUT_ROOT/$timestamp-run$index"
    mkdir -p "$run_dir"
    case_root="$run_dir/case"
    mkdir -p "$case_root"
    proxy_state="$run_dir/proxy-state.json"
    proxy_stop="$run_dir/proxy-stop"
    proxy_result="$run_dir/proxy-result.json"
    sampler_stop="$run_dir/sampler-stop"
    root_pid_file="$run_dir/root.pid"
    samples="$run_dir/m1-rss.tsv"
    region_snapshot="$run_dir/daemon-regions.tsv"
    jsonl="$run_dir/m1-case.jsonl"
    stderr="$run_dir/m1-case.stderr"

    start_proxy "$case_root" "$proxy_state" "$proxy_stop" "$proxy_result"
    python3 - "$proxy_state" <<'PY'
from pathlib import Path
import sys
import time

path = Path(sys.argv[1])
deadline = time.monotonic() + 5
while not path.exists() and time.monotonic() < deadline:
    time.sleep(0.005)
if not path.exists():
    raise SystemExit("fake proxy did not publish its state")
PY
    state_values=$(python3 - "$proxy_state" <<'PY'
import json
from pathlib import Path
import sys

value = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
for key in ("workspace", "home", "config", "data", "state", "tmp", "credential", "base_url"):
    print(value[key])
PY
)
    old_ifs=$IFS
    IFS='
'
    set -- $state_values
    IFS=$old_ifs
    workspace=$1
    home=$2
    config=$3
    data=$4
    state=$5
    case_tmp=$6
    credential=$7
    base_url=$8

    if [ -n "$REGION_SNAPSHOT_TOOL" ]; then
        python3 "$SAMPLER" \
            --output "$samples" \
            --root-pid-file "$root_pid_file" \
            --stop-file "$sampler_stop" \
            --daemon-pid-dir "/tmp/haider-$(id -u)" \
            --daemon-pid-dir "$home/.haider/runtime" \
            --require-daemon \
            --region-snapshot-tool "$REGION_SNAPSHOT_TOOL" \
            --region-snapshot-output "$region_snapshot" \
            --region-snapshot-threshold-bytes "$REGION_SNAPSHOT_THRESHOLD_BYTES" \
            --region-snapshot-min-growth-bytes "$REGION_SNAPSHOT_MIN_GROWTH_BYTES" &
    else
        python3 "$SAMPLER" \
            --output "$samples" \
            --root-pid-file "$root_pid_file" \
            --stop-file "$sampler_stop" \
            --daemon-pid-dir "/tmp/haider-$(id -u)" \
            --daemon-pid-dir "$home/.haider/runtime" \
            --require-daemon &
    fi
    sampler_pid=$!
    prompt='Follow the model response and complete the conformance task in this repository.'
    (
        cd "$workspace"
        exec env -i \
            PATH="${PATH:-/usr/bin:/bin}" \
            LANG="${LANG:-C}" \
            CI=1 \
            NO_COLOR=1 \
            HOME="$home" \
            XDG_CONFIG_HOME="$config/xdg" \
            XDG_DATA_HOME="$data" \
            XDG_STATE_HOME="$state" \
            TMPDIR="$case_tmp" \
            HAIDER_PROFILE_DIR="$config" \
            HAIDER_DISCOVERY_DISABLED=1 \
            HAIDER_DEVICE_DISCOVERY_DISABLED=1 \
            HAIDER_TEST_DEVICE_NAME=test-mac \
            HAIDER_NO_UPDATE_CHECK=1 \
            RUST_MIN_STACK=8388608 \
            BENCH_PROXY_API_KEY="$credential" \
            BENCH_PROXY_BASE_URL="$base_url" \
            "$HAIDER_BIN" run "$prompt" \
                --output jsonl \
                --timeout 15m \
                --provider bench-proxy \
                --model deepseek-v4-flash \
                --auto-allow \
                --allow-writes \
                --allow-exec
    ) >"$jsonl" 2>"$stderr" &
    cli_pid=$!
    echo "$cli_pid" > "$root_pid_file"
    cli_status=0
    wait "$cli_pid" || cli_status=$?
    cli_pid=
    : > "$sampler_stop"
    sampler_status=0
    wait "$sampler_pid" || sampler_status=$?
    sampler_pid=
    : > "$proxy_stop"
    wait "$proxy_pid"
    proxy_pid=

    if [ "$cli_status" -ne 0 ]; then
        echo "M1 CLI failed with status $cli_status; see $stderr" >&2
        return "$cli_status"
    fi
    if [ "$sampler_status" -ne 0 ]; then
        echo "M1 sampler failed with status $sampler_status" >&2
        return "$sampler_status"
    fi
    python3 - "$workspace/value.txt" <<'PY'
from pathlib import Path
import sys

value = Path(sys.argv[1]).read_bytes()
if value != b"fixed\n":
    raise SystemExit(f"value.txt mismatch: {value!r}")
PY
    analyze_run "$run_dir" "$load"
}

mode=run
while [ "$#" -gt 0 ]; do
    case "$1" in
        --self-test)
            mode=self-test
            shift
            ;;
        --runs)
            [ "$#" -ge 2 ] || { usage; exit 2; }
            RUNS=$2
            shift 2
            ;;
        *)
            usage
            exit 2
            ;;
    esac
done

if [ "$mode" = self-test ]; then
    self_test
    exit 0
fi

case "$RUNS" in
    ''|*[!0-9]*) usage; exit 2 ;;
esac
if [ "$RUNS" -ne 5 ]; then
    echo "M1 conformance measurement requires exactly 5 runs" >&2
    exit 2
fi
initial_load=$(load_1m)
if python3 - "$initial_load" "$MAX_LOAD_1M" <<'PY'
import sys
raise SystemExit(0 if float(sys.argv[1]) < float(sys.argv[2]) else 1)
PY
then
    mkdir -p "$OUTPUT_ROOT"
else
    self_test
    echo "not measured, load too high (load_1m=$initial_load, required < $MAX_LOAD_1M)"
    exit 0
fi

if [ ! -x "$HAIDER_BIN" ]; then
    echo "M1 haider executable is missing: $HAIDER_BIN" >&2
    exit 2
fi
HAIDERD_BIN=$(dirname -- "$HAIDER_BIN")/haiderd
if [ ! -x "$HAIDERD_BIN" ]; then
    echo "M1 sibling haiderd executable is missing: $HAIDERD_BIN" >&2
    exit 2
fi
daemon_bytes=$(wc -c < "$HAIDERD_BIN" | tr -d ' ')
if [ "$daemon_bytes" -le 10485760 ]; then
    echo "M1 haiderd is only $daemon_bytes bytes; registry #64 rejects a truncated binary" >&2
    exit 2
fi
if [ ! -f "$BENCH_ROOT/bench/conformance/fake_proxy.py" ]; then
    echo "M1 benchmark checkout is missing: $BENCH_ROOT" >&2
    exit 2
fi

index=1
while [ "$index" -le "$RUNS" ]; do
    run_load=$(load_1m)
    if ! python3 - "$run_load" "$MAX_LOAD_1M" <<'PY'
import sys
raise SystemExit(0 if float(sys.argv[1]) < float(sys.argv[2]) else 1)
PY
    then
        echo "not measured, load too high before run $index (load_1m=$run_load, required < $MAX_LOAD_1M)" >&2
        exit 3
    fi
    run_once "$index" "$run_load"
    end_load=$(load_1m)
    if ! python3 - "$end_load" "$MAX_LOAD_1M" <<'PY'
import sys
raise SystemExit(0 if float(sys.argv[1]) < float(sys.argv[2]) else 1)
PY
    then
        echo "not measured, load too high after run $index (load_1m=$end_load, required < $MAX_LOAD_1M)" >&2
        exit 4
    fi
    python3 - "$run_dir/summary.json" "$end_load" <<'PY'
import json
from pathlib import Path
import sys

path = Path(sys.argv[1])
summary = json.loads(path.read_text(encoding="utf-8"))
summary["load_end_1m"] = float(sys.argv[2])
path.write_text(json.dumps(summary, indent=2) + "\n", encoding="utf-8")
PY
    index=$((index + 1))
done
