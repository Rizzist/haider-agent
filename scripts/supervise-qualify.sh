#!/bin/bash
# Qualification gate: fake Codex cases, private journal, and watchdogs.
set -u
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd) || exit 1
WRAPPER="$SCRIPT_DIR/codex-supervised.sh"
# shellcheck source=scripts/supervise-lib.sh
. "$SCRIPT_DIR/supervise-lib.sh" || exit 1
TMP_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/supervise-qualify.XXXXXX") || exit 1
JOURNAL_DIR="$TMP_ROOT/run-journal"
FAKE_CODEX="$TMP_ROOT/fake-codex.sh"
FAILURES=0
CASE_PID=
WATCHDOG_PID=
CASE_STDERR_LOG=
CASE_JOURNAL=
cleanup() {
    if [ -n "$CASE_PID" ]; then
        kill -TERM -- "-$CASE_PID" 2>/dev/null || true
        sleep 1
        kill -KILL -- "-$CASE_PID" 2>/dev/null || true
    fi
    [ -n "$WATCHDOG_PID" ] && kill "$WATCHDOG_PID" 2>/dev/null || true
    type cleanup_fake_pids >/dev/null 2>&1 && cleanup_fake_pids
    rm -rf "$TMP_ROOT"
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
mkdir "$JOURNAL_DIR" || exit 1
cat > "$FAKE_CODEX" <<'FAKE'
#!/bin/bash
EXPECTED_PREFIX="Previous run stalled and was killed. Inspect current git status/diff first, do not redo completed work, continue from where it stopped."
if [ "$#" -ne 6 ] || [ "$1" != exec ] || [ "$2" != -s ] ||
   [ "$3" != workspace-write ] || [ "$4" != -c ] ||
   [ "$5" != model_reasoning_effort=xhigh ]; then
    echo "unexpected codex arguments" >&2
    exit 90
fi
if IFS= read -r unexpected_input; then
    echo "codex stdin was not /dev/null" >&2
    exit 91
fi
spawn_stall_tree() {
    spawn_after_term() {
        sh -c '
            ps -o pid=,lstart= -p "$$" |
                awk '"'"'{$1=$1; print}'"'"' >> "$1"
            while :; do sleep 60; done
        ' term-child "$FAKE_PID_FILE" &
    }
    trap spawn_after_term TERM
    ps -o pid=,lstart= -p "$$" | awk '{$1=$1; print}' >> "$FAKE_PID_FILE"
    sh -c '
        ps -o pid=,lstart= -p "$$" |
            awk '"'"'{$1=$1; print}'"'"' >> "$1"
        sh -c '"'"'
            ps -o pid=,lstart= -p "$$" |
                awk '"'"'"'"'"'"'"'"'{$1=$1; print}'"'"'"'"'"'"'"'"' >> "$1"
            while :; do sleep 60; done
        '"'"' grandchild "$1" &
        while :; do sleep 60; done
    ' child "$FAKE_PID_FILE" &
}
prompt=$6
case "${FAKE_MODE:-}" in
    happy|no_changes)
        printf 'fake codex completed\n'
        exit 0
        ;;
    stall_then_success)
        if [ -e "$FAKE_STATE_FILE" ]; then
            case "$prompt" in
                "$EXPECTED_PREFIX"*) ;;
                *) echo "retry prompt prefix missing" >&2; exit 92 ;;
            esac
            printf 'fake codex recovered\n'
            exit 0
        fi
        : > "$FAKE_STATE_FILE"
        printf 'fake codex made initial progress\n'
        spawn_stall_tree
        while :; do sleep 60; done
        ;;
    always_stall)
        printf 'fake codex made initial progress\n'
        spawn_stall_tree
        while :; do sleep 60; done
        ;;
    leader_exits)
        printf 'fake leader exited\n'
        sh -c '
            ps -o pid=,lstart= -p "$$" |
                awk '"'"'{$1=$1; print}'"'"' >> "$1"
            while :; do sleep 60; done
        ' orphan "$FAKE_PID_FILE" &
        exit 0
        ;;
    detached_leader_exits|leader_instant_exit)
        printf 'fake detached leader exited\n'
        if command -v setsid >/dev/null 2>&1; then
            setsid sh -c '
                ps -o pid=,lstart= -p "$$" |
                    awk '"'"'{$1=$1; print}'"'"' >> "$1"
                while :; do sleep 60; done
            ' detached "$FAKE_PID_FILE" &
        else
            perl -MPOSIX=setsid -e '
                setsid();
                exec "sh", "-c",
                    q{ps -o pid=,lstart= -p "$$" | awk '"'"'{$1=$1; print}'"'"' >> "$1"; while :; do sleep 60; done},
                    "detached", $ARGV[0]
            ' "$FAKE_PID_FILE" &
        fi
        exit 0
        ;;
    interrupt)
        ps -o pid=,lstart= -p "$$" |
            awk '{$1=$1; print}' >> "$FAKE_PID_FILE"
        printf 'fake codex requests interrupt\n'
        kill -TERM "$PPID"
        while :; do sleep 60; done
        ;;
    *) echo "unknown FAKE_MODE" >&2; exit 93 ;;
esac
FAKE
chmod +x "$FAKE_CODEX" || exit 1
pass() { echo "PASS: $1"; }
fail() { echo "FAIL: $1${2:+ — $2}"; FAILURES=$((FAILURES + 1)); }
# Run a case under a 60s process-group watchdog.
run_with_watchdog() {
    local marker status
    marker=$1
    shift
    rm -f "$marker"
    CASE_STDERR_LOG="${marker}.stderr-log"
    CASE_JOURNAL=
    set -m
    "$@" 2>"$CASE_STDERR_LOG" &
    CASE_PID=$!
    set +m
    (
        sleep 60
        : > "$marker"
        kill -TERM -- "-$CASE_PID" 2>/dev/null || true
        sleep 2
        kill -KILL -- "-$CASE_PID" 2>/dev/null || true
    ) &
    WATCHDOG_PID=$!
    wait "$CASE_PID"
    status=$?
    CASE_PID=
    kill "$WATCHDOG_PID" 2>/dev/null || true
    wait "$WATCHDOG_PID" 2>/dev/null || true
    WATCHDOG_PID=
    CASE_JOURNAL=$(awk '/^journal: / {
        sub(/^journal: /, ""); journal=$0
    } END { print journal }' "$CASE_STDERR_LOG")
    [ ! -e "$marker" ] || { cleanup_fake_pids; return 124; }
    return "$status"
}
events_for_brief() {
    [ -n "$CASE_JOURNAL" ] && [ -f "$CASE_JOURNAL" ] ||
        { printf '\n'; return; }
    awk -v brief="$1" '
        index($0, "\"brief\":\"" brief "\"") {
            event=$0
            sub(/^.*"event":"/, "", event)
            sub(/".*$/, "", event)
            events=events (events == "" ? "" : " ") event
        }
        END { print events }
    ' "$CASE_JOURNAL"
}
event_line() {
    [ -n "$CASE_JOURNAL" ] && [ -f "$CASE_JOURNAL" ] || return
    awk -v brief="$1" -v event="$2" '
        index($0, "\"brief\":\"" brief "\"") &&
        index($0, "\"event\":\"" event "\"") { print; exit }
    ' "$CASE_JOURNAL"
}
pids_survive() {
    local file pid identity current
    file=$1
    [ -f "$file" ] || return 1
    while read -r pid identity; do
        [ -n "$pid" ] && [ -n "$identity" ] || continue
        current=$(process_identity "$pid") || current=
        [ "$current" = "$identity" ] && pid_is_live "$pid" && return 0
    done < "$file"
    return 1
}
all_pids_reaped() {
    local file elapsed
    file=$1
    elapsed=0
    while pids_survive "$file" && [ "$elapsed" -lt 5 ]; do
        sleep 1
        elapsed=$((elapsed + 1))
    done
    pids_survive "$file" && return 1
    rm -f "$file"
    return 0
}
run_happy_path() {
    local brief output status events line marker
    brief="$TMP_ROOT/happy.brief"; output="$TMP_ROOT/happy.out"
    marker="$TMP_ROOT/happy.timeout"
    printf 'Complete the happy-path task.' > "$brief"
    run_with_watchdog "$marker" env FAKE_MODE=happy CODEX_BIN="$FAKE_CODEX" \
        HAIDER_RUN_JOURNAL="$JOURNAL_DIR" bash "$WRAPPER" "$brief" "$output" \
        --max-stall-secs 3 --max-retries 1
    status=$?; events=$(events_for_brief "$brief")
    events=${events// reap_unverifiable/}
    line="$(event_line "$brief" done) $(event_line "$brief" start)"
    if [ "$status" -eq 0 ] && [ "$events" = "start done" ] &&
       printf '%s' "$line" | grep -q '"exit_code":0' &&
       printf '%s' "$line" | grep -Eq '"run_id":"[0-9]+-[0-9]+"' &&
       printf '%s' "$line" | grep -Eq '"launch_mode":"(setsid_perl|plain_spawn)"' &&
       grep -q 'fake codex completed' "$output"; then
        pass "happy path"
    else
        fail "happy path" "status=$status events=[$events]"
    fi
}
run_stall_retry() {
    local brief output state pids status events marker
    brief="$TMP_ROOT/stall-retry.brief"; output="$TMP_ROOT/stall-retry.out"
    state="$TMP_ROOT/stall-retry.flag"; pids="$TMP_ROOT/stall-retry.pids"
    marker="$TMP_ROOT/stall-retry.timeout"
    printf 'Complete the retry task.' > "$brief"
    run_with_watchdog "$marker" env FAKE_MODE=stall_then_success \
        FAKE_STATE_FILE="$state" FAKE_PID_FILE="$pids" CODEX_BIN="$FAKE_CODEX" \
        HAIDER_RUN_JOURNAL="$JOURNAL_DIR" bash "$WRAPPER" "$brief" "$output" \
        --max-stall-secs 3 --max-retries 2
    status=$?; events=$(events_for_brief "$brief")
    events=${events// reap_unverifiable/}
    if [ "$status" -eq 0 ] &&
       [ "$events" = "start stall_kill retry done" ] &&
       grep -q 'fake codex recovered' "$output" && all_pids_reaped "$pids"; then
        pass "stall kill, descendant reap, and retry"
    else
        fail "stall kill, descendant reap, and retry" "status=$status events=[$events]"
    fi
}
run_give_up() {
    local brief output pids status events marker
    brief="$TMP_ROOT/give-up.brief"; output="$TMP_ROOT/give-up.out"
    pids="$TMP_ROOT/give-up.pids"; marker="$TMP_ROOT/give-up.timeout"
    printf 'This fake task will never finish.' > "$brief"
    run_with_watchdog "$marker" env FAKE_MODE=always_stall \
        FAKE_PID_FILE="$pids" CODEX_BIN="$FAKE_CODEX" \
        HAIDER_RUN_JOURNAL="$JOURNAL_DIR" bash "$WRAPPER" "$brief" "$output" \
        --max-stall-secs 3 --max-retries 1
    status=$?; events=$(events_for_brief "$brief")
    if [ "$status" -eq 1 ] &&
       [ "$events" = "start stall_kill retry stall_kill gave_up" ] &&
       all_pids_reaped "$pids"; then
        pass "give up after max retries"
    else
        fail "give up after max retries" "status=$status events=[$events]"
    fi
}
run_orphan_reap() {
    local brief output pids status events marker
    brief="$TMP_ROOT/orphan.brief"; output="$TMP_ROOT/orphan.out"
    pids="$TMP_ROOT/orphan.pids"; marker="$TMP_ROOT/orphan.timeout"
    printf 'Exercise leader-exit cleanup.' > "$brief"
    run_with_watchdog "$marker" env FAKE_MODE=leader_exits FAKE_PID_FILE="$pids" \
        CODEX_BIN="$FAKE_CODEX" HAIDER_RUN_JOURNAL="$JOURNAL_DIR" \
        bash "$WRAPPER" "$brief" "$output" --max-stall-secs 3 --max-retries 0
    status=$?; events=$(events_for_brief "$brief")
    events=${events// reap_unverifiable/}
    if [ "$status" -eq 0 ] &&
       [ "$events" = "start orphans_reaped done" ] && all_pids_reaped "$pids"; then
        pass "leader-exit orphan reap"
    else
        fail "leader-exit orphan reap" "status=$status events=[$events]"
    fi
}
. "$SCRIPT_DIR/supervise-qualify-extra.sh"
if [ ! -x "$WRAPPER" ]; then
    fail "wrapper executable" "$WRAPPER is not executable"
else
    run_happy_path
    run_stall_retry
    run_give_up
    run_orphan_reap
    run_dirty_git_safety
    run_extra_cases
fi
if [ "$FAILURES" -ne 0 ]; then
    echo "$FAILURES qualification case(s) failed."
    exit 1
fi
echo "All supervision qualification cases passed."
