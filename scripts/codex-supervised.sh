#!/bin/bash
#
# Run `codex exec` while watching stdout and stderr for forward progress.
# Compatible with the Bash 3.2 shipped by macOS.

set -u

usage() {
    echo "Usage: codex-supervised.sh <brief-file> <output-file> [--max-stall-secs N] [--max-retries M]" >&2
}

die() {
    echo "codex-supervised.sh: $*" >&2
    exit 2
}

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd) ||
    die "cannot resolve script directory"
# shellcheck source=scripts/supervise-lib.sh
. "$SCRIPT_DIR/supervise-lib.sh" || die "cannot load supervise-lib.sh"

if [ "$#" -lt 2 ]; then
    usage
    exit 2
fi

BRIEF_FILE=$1
OUTPUT_FILE=$2
shift 2
MAX_STALL_SECS=600
MAX_RETRIES=2

while [ "$#" -gt 0 ]; do
    case "$1" in
        --max-stall-secs)
            [ "$#" -ge 2 ] || die "--max-stall-secs requires a value"
            MAX_STALL_SECS=$2
            shift 2
            ;;
        --max-retries)
            [ "$#" -ge 2 ] || die "--max-retries requires a value"
            MAX_RETRIES=$2
            shift 2
            ;;
        *) die "unknown argument: $1" ;;
    esac
done

is_nonnegative_integer "$MAX_STALL_SECS" &&
    [ "$MAX_STALL_SECS" -gt 0 ] ||
    die "--max-stall-secs must be a positive integer"
is_nonnegative_integer "$MAX_RETRIES" ||
    die "--max-retries must be a non-negative integer"
[ -f "$BRIEF_FILE" ] || die "brief file not found: $BRIEF_FILE"
[ -r "$BRIEF_FILE" ] || die "brief file is not readable: $BRIEF_FILE"

if [ "${HAIDER_RUN_JOURNAL+x}" = x ]; then
    [ -n "$HAIDER_RUN_JOURNAL" ] || die "HAIDER_RUN_JOURNAL must not be empty"
    JOURNAL_FILE=$HAIDER_RUN_JOURNAL
else
    JOURNAL_FILE="$SCRIPT_DIR/run-journal.jsonl"
fi
STDERR_FILE="${OUTPUT_FILE}.stderr"
CODEX_COMMAND=${CODEX_BIN:-codex}
RETRY_PREFIX="Previous run stalled and was killed. Inspect current git status/diff first, do not redo completed work, continue from where it stopped."

# Compare resolved paths before opening any destination. This also catches
# existing hard-link aliases through test's -ef operator.
PATH_LABELS=(brief output stderr journal)
PATH_VALUES=("$BRIEF_FILE" "$OUTPUT_FILE" "$STDERR_FILE" "$JOURNAL_FILE")
i=0
while [ "$i" -lt 4 ]; do
    j=$((i + 1))
    while [ "$j" -lt 4 ]; do
        paths_alias "${PATH_VALUES[$i]}" "${PATH_VALUES[$j]}"
        alias_status=$?
        [ "$alias_status" -ne 2 ] ||
            die "cannot resolve ${PATH_LABELS[$i]} or ${PATH_LABELS[$j]} path"
        [ "$alias_status" -ne 0 ] ||
            die "${PATH_LABELS[$i]} and ${PATH_LABELS[$j]} paths alias each other"
        j=$((j + 1))
    done
    i=$((i + 1))
done

PROMPT=$(cat "$BRIEF_FILE") || die "cannot read brief file: $BRIEF_FILE"
: > "$OUTPUT_FILE" || die "cannot write output file: $OUTPUT_FILE"
: > "$STDERR_FILE" || die "cannot write stderr file: $STDERR_FILE"
JOB_STATUS_FILE=$(mktemp "${TMPDIR:-/tmp}/codex-supervised-jobs.XXXXXX") ||
    die "cannot create temporary job-status file"
TREE_FILE=$(mktemp "${TMPDIR:-/tmp}/codex-supervised-tree.XXXXXX") ||
    die "cannot create temporary process-tree file"
PS_FILE=$(mktemp "${TMPDIR:-/tmp}/codex-supervised-ps.XXXXXX") ||
    die "cannot create temporary process snapshot"

START_EPOCH=$(date +%s) || die "cannot read clock"
RUN_ID="${START_EPOCH}-$$"
JOURNAL_LOCK_DIR="${JOURNAL_FILE}.lock"
JOURNAL_LOCK_HELD=0
ACTIVE_PID=
RETRIES=0
JOURNAL_STARTED=0
IN_JOURNAL=0
PENDING_SIGNAL=
PENDING_SIGNAL_CODE=

cleanup() {
    release_journal_lock >/dev/null 2>&1 || true
    rm -f "$JOB_STATUS_FILE" "$TREE_FILE" "$PS_FILE"
}

job_is_running() {
    local pid job_pid
    pid=$1
    jobs -pr > "$JOB_STATUS_FILE"
    while IFS= read -r job_pid; do
        [ "$job_pid" = "$pid" ] && return 0
    done < "$JOB_STATUS_FILE"
    return 1
}

record_event() {
    local status pending_name pending_code
    IN_JOURNAL=1
    journal_event "$@"
    status=$?
    IN_JOURNAL=0
    if [ "$status" -ne 0 ]; then
        if [ -n "$ACTIVE_PID" ]; then
            terminate_process_tree "$ACTIVE_PID" "$TREE_FILE" "$PS_FILE" || true
            ACTIVE_PID=
        fi
        exit 2
    fi
    if [ -n "$PENDING_SIGNAL" ]; then
        pending_name=$PENDING_SIGNAL
        pending_code=$PENDING_SIGNAL_CODE
        PENDING_SIGNAL=
        PENDING_SIGNAL_CODE=
        handle_signal "$pending_name" "$pending_code"
    fi
}

handle_signal() {
    local signal_name exit_code now output_bytes
    signal_name=$1
    exit_code=$2
    if [ "$IN_JOURNAL" -eq 1 ]; then
        PENDING_SIGNAL=$signal_name
        PENDING_SIGNAL_CODE=$exit_code
        return
    fi
    trap - HUP INT TERM
    if [ -n "$ACTIVE_PID" ]; then
        terminate_process_tree "$ACTIVE_PID" "$TREE_FILE" "$PS_FILE" || true
        ACTIVE_PID=
    fi
    now=$(date +%s)
    output_bytes=$(file_bytes "$OUTPUT_FILE")
    if [ "$JOURNAL_STARTED" -eq 1 ]; then
        journal_event "interrupted" \
            ',"signal":"'"$signal_name"'","exit_code":'"$exit_code"',"duration_s":'"$((now - START_EPOCH))"',"retries":'"$RETRIES"',"output_bytes":'"$output_bytes" ||
            exit 2
    fi
    exit "$exit_code"
}

trap cleanup EXIT
trap 'handle_signal HUP 129' HUP
trap 'handle_signal INT 130' INT
trap 'handle_signal TERM 143' TERM

JOURNAL_STARTED=1
record_event "start" ',"retries":0'

while :; do
    if [ "$RETRIES" -eq 0 ]; then
        ATTEMPT_PROMPT=$PROMPT
    else
        ATTEMPT_PROMPT="${RETRY_PREFIX}
${PROMPT}"
    fi

    : > "$OUTPUT_FILE" || die "cannot reset output file: $OUTPUT_FILE"
    : > "$STDERR_FILE" || die "cannot reset stderr file: $STDERR_FILE"
    : > "$TREE_FILE" || die "cannot reset process-tree file"

    ATTEMPT_START=$(date +%s)
    LAST_ACTIVITY=$ATTEMPT_START
    PREVIOUS_OUTPUT_BYTES=0
    PREVIOUS_STDERR_BYTES=0

    set -m
    "$CODEX_COMMAND" exec -s workspace-write \
        -c model_reasoning_effort=xhigh "$ATTEMPT_PROMPT" \
        </dev/null >"$OUTPUT_FILE" 2>"$STDERR_FILE" &
    ACTIVE_PID=$!
    set +m
    remember_pid "$ACTIVE_PID" "$TREE_FILE" >/dev/null 2>&1 || true

    STALLED=0
    while :; do
        sleep 1
        NOW=$(date +%s)
        CURRENT_OUTPUT_BYTES=$(file_bytes "$OUTPUT_FILE")
        CURRENT_STDERR_BYTES=$(file_bytes "$STDERR_FILE")
        collect_process_tree "$ACTIVE_PID" "$TREE_FILE" "$PS_FILE" || true

        if [ "$CURRENT_OUTPUT_BYTES" -gt "$PREVIOUS_OUTPUT_BYTES" ] ||
           [ "$CURRENT_STDERR_BYTES" -gt "$PREVIOUS_STDERR_BYTES" ]; then
            LAST_ACTIVITY=$NOW
            PREVIOUS_OUTPUT_BYTES=$CURRENT_OUTPUT_BYTES
            PREVIOUS_STDERR_BYTES=$CURRENT_STDERR_BYTES
        fi

        if ! job_is_running "$ACTIVE_PID"; then
            break
        fi

        if [ $((NOW - LAST_ACTIVITY)) -ge "$MAX_STALL_SECS" ]; then
            STALLED=1
            if ! terminate_process_tree "$ACTIVE_PID" "$TREE_FILE" "$PS_FILE"; then
                echo "codex-supervised.sh: process tree survived forced termination" >&2
                TREE_REAP_FAILED=1
            else
                TREE_REAP_FAILED=0
            fi
            ACTIVE_PID=
            break
        fi
    done

    if [ "$STALLED" -eq 1 ]; then
        NOW=$(date +%s)
        OUTPUT_BYTES=$(file_bytes "$OUTPUT_FILE")
        record_event "stall_kill" \
            ',"duration_s":'"$((NOW - ATTEMPT_START))"',"retries":'"$RETRIES"',"output_bytes":'"$OUTPUT_BYTES"

        if [ "$TREE_REAP_FAILED" -eq 1 ] ||
           [ "$RETRIES" -ge "$MAX_RETRIES" ]; then
            record_event "gave_up" \
                ',"exit_code":1,"duration_s":'"$((NOW - START_EPOCH))"',"retries":'"$RETRIES"',"output_bytes":'"$OUTPUT_BYTES"
            exit 1
        fi

        RETRIES=$((RETRIES + 1))
        record_event "retry" ',"retries":'"$RETRIES"
        continue
    fi

    LEADER_PID=$ACTIVE_PID
    wait "$LEADER_PID"
    EXIT_CODE=$?

    if group_exists "$LEADER_PID" || pid_file_has_survivors "$TREE_FILE"; then
        if reap_survivors "$LEADER_PID" "$TREE_FILE"; then
            REMAINING=0
        else
            REMAINING=1
        fi
        NOW=$(date +%s)
        record_event "orphans_reaped" \
            ',"remaining":'"$REMAINING"',"duration_s":'"$((NOW - ATTEMPT_START))"',"retries":'"$RETRIES"
        if [ "$REMAINING" -ne 0 ]; then
            echo "codex-supervised.sh: survivors remain after leader exit" >&2
            ACTIVE_PID=
            OUTPUT_BYTES=$(file_bytes "$OUTPUT_FILE")
            record_event "gave_up" \
                ',"exit_code":1,"duration_s":'"$((NOW - START_EPOCH))"',"retries":'"$RETRIES"',"output_bytes":'"$OUTPUT_BYTES"
            exit 1
        fi
    fi
    ACTIVE_PID=

    NOW=$(date +%s)
    OUTPUT_BYTES=$(file_bytes "$OUTPUT_FILE")
    record_event "done" \
        ',"exit_code":'"$EXIT_CODE"',"duration_s":'"$((NOW - START_EPOCH))"',"retries":'"$RETRIES"',"output_bytes":'"$OUTPUT_BYTES"
    exit "$EXIT_CODE"
done
