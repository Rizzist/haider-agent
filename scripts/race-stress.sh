#!/usr/bin/env bash
# Repeat cataloged race-window tests under CPU contention. The caller owns the
# machine-wide build slot; this script owns only its load-generator child.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
catalog="$repo_root/scripts/race-stress-cases.tsv"
iterations=20
requested_platform="auto"
case_id=""
results=""
list_only=false
under_load=false

usage() {
  cat <<'EOF'
usage: scripts/race-stress.sh --under-load [--iterations N] [--platform auto|linux|macos]
                              [--case ID] [--results PATH]
       scripts/race-stress.sh --catalog [--platform auto|linux|macos]
EOF
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --under-load) under_load=true; shift ;;
    --iterations) iterations="${2:-}"; shift 2 ;;
    --platform) requested_platform="${2:-}"; shift 2 ;;
    --case) case_id="${2:-}"; shift 2 ;;
    --results) results="${2:-}"; shift 2 ;;
    --catalog) list_only=true; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "race-stress: unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

case "$iterations" in
  ''|*[!0-9]*) echo "race-stress: --iterations must be a positive integer" >&2; exit 2 ;;
  0) echo "race-stress: --iterations must be a positive integer" >&2; exit 2 ;;
esac

if [ "$requested_platform" = auto ]; then
  case "$(uname -s)" in
    Darwin) platform=macos ;;
    Linux) platform=linux ;;
    *) echo "race-stress: unsupported host; pass --platform explicitly" >&2; exit 2 ;;
  esac
elif [ "$requested_platform" = linux ] || [ "$requested_platform" = macos ]; then
  platform="$requested_platform"
else
  echo "race-stress: --platform must be auto, linux, or macos" >&2
  exit 2
fi

selected_file="$(mktemp "${TMPDIR:-/tmp}/haider-race-cases.XXXXXX")"
compile_file="$(mktemp "${TMPDIR:-/tmp}/haider-race-suites.XXXXXX")"
load_pid=""
cleanup() {
  if [ -n "$load_pid" ]; then
    kill "$load_pid" 2>/dev/null || true
    wait "$load_pid" 2>/dev/null || true
  fi
  rm -f "$selected_file" "$compile_file"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

awk -F '\t' -v platform="$platform" -v case_id="$case_id" '
  $0 !~ /^#/ && NF == 6 && ($2 == platform || $2 == "all") && (case_id == "" || $1 == case_id) { print }
' "$catalog" > "$selected_file"

selected_count="$(wc -l < "$selected_file" | tr -d ' ')"
if [ "$selected_count" -eq 0 ]; then
  echo "race-stress: no catalog cases selected for platform=$platform case=${case_id:-all}" >&2
  exit 2
fi

if [ "$list_only" = true ]; then
  printf 'id\tplatform\tpackage\tsuite\ttest\tsource\n'
  cat "$selected_file"
  exit 0
fi
if [ "$under_load" != true ]; then
  echo "race-stress: counted runs require --under-load" >&2
  exit 2
fi

cd "$repo_root"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$repo_root/target}"
export CARGO_INCREMENTAL=0
export RUST_MIN_STACK="${RUST_MIN_STACK:-8388608}"
export HAIDER_DISCOVERY_DISABLED="${HAIDER_DISCOVERY_DISABLED:-1}"
export HAIDER_TEST_DEVICE_NAME="${HAIDER_TEST_DEVICE_NAME:-test-mac}"

if awk -F '\t' '$3 == "haider-cli" { found=1 } END { exit !found }' "$selected_file"; then
  cargo build --locked -p haider-cli -p haider-daemond -p haider-tui-exe --bins
  export HAIDER_TEST_SIBLINGS_PREBUILT=1
fi

# Compile each selected test binary once before contention starts. The counted
# loop then stresses one unchanged executable instead of mixing linker time
# into the race window.
awk -F '\t' '!seen[$3 FS $4]++ { print $3 FS $4 }' "$selected_file" > "$compile_file"
while IFS=$'\t' read -r package suite; do
  compile_args=(test --locked -p "$package")
  if [ "$suite" = lib ]; then
    compile_args+=(--lib)
  else
    compile_args+=(--test "${suite#test:}")
  fi
  cargo "${compile_args[@]}" --no-run
done < "$compile_file"

if [ -n "$results" ]; then
  mkdir -p "$(dirname "$results")"
  printf 'id\tplatform\tpassed\tattempted\tstatus\tduration_seconds\n' > "$results"
fi

# One continuously runnable child supplies scheduler contention without turning
# a two-slot development host into a fork bomb. The EXIT trap reaps this child.
yes >/dev/null &
load_pid=$!

failures=0
while IFS=$'\t' read -r id _case_platform package suite test_name source; do
  start="$(date +%s)"
  passed=0
  attempted=0
  iteration=1
  while [ "$iteration" -le "$iterations" ]; do
    attempted=$iteration
    log="$(mktemp "${TMPDIR:-/tmp}/haider-race-test.XXXXXX")"
    cargo_args=(test --locked -p "$package")
    if [ "$suite" = lib ]; then
      cargo_args+=(--lib)
    else
      cargo_args+=(--test "${suite#test:}")
    fi
    if cargo "${cargo_args[@]}" "$test_name" -- --test-threads=1 >"$log" 2>&1 \
      && grep -E 'test result:' "$log" | tail -n 1 \
        | grep -Eq '^test result: ok\. 1 passed; 0 failed;'; then
      passed=$((passed + 1))
      rm -f "$log"
    else
      echo "race-stress: FAIL $id iteration $iteration ($source::$test_name)" >&2
      sed -n '1,240p' "$log" >&2
      rm -f "$log"
      break
    fi
    iteration=$((iteration + 1))
  done
  end="$(date +%s)"
  status=PASS
  if [ "$passed" -ne "$iterations" ]; then
    status=FAIL
    failures=$((failures + 1))
  fi
  printf 'race-stress: %s %s/%s %s (%ss)\n' "$id" "$passed" "$iterations" "$status" "$((end - start))"
  if [ -n "$results" ]; then
    printf '%s\t%s\t%s\t%s\t%s\t%s\n' \
      "$id" "$platform" "$passed" "$attempted" "$status" "$((end - start))" >> "$results"
  fi
done < "$selected_file"

if [ "$failures" -ne 0 ]; then
  echo "race-stress: $failures of $selected_count cases failed" >&2
  exit 1
fi
echo "race-stress: all $selected_count cases passed $iterations/$iterations under load"
