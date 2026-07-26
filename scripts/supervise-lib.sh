#!/bin/bash
#
# Shared, Bash 3.2/BSD-compatible helpers for the Codex supervision harness.
# Sourced (never executed); also pulls in supervise-process-lib.sh.
#
# Owns two invariants:
#   Journal format — one JSON object per line, always carrying
#   {"ts","run_id","brief","event"} plus event-specific fields, appended
#   only while holding the journal lock (see journal_event).
#   Lock ownership — the lock is the directory $JOURNAL_LOCK_DIR; its
#   owner file records "pid TAB lstart" so staleness can be decided by
#   process identity, not by PID liveness alone (see acquire_journal_lock).
#
# The journal/lock functions read globals owned by codex-supervised.sh:
# JOURNAL_FILE, JOURNAL_LOCK_DIR, JOURNAL_LOCK_HELD, STALE_LOCK_RECOVERED,
# RUN_ID, BRIEF_FILE. Pure helpers above process_identity need none.

is_nonnegative_integer() {
    case "$1" in
        ""|*[!0-9]*) return 1 ;;
        *) return 0 ;;
    esac
}

file_bytes() {
    wc -c < "$1" | tr -d '[:space:]'
}

# JSON strings may not contain any unescaped byte below 0x20. NUL cannot
# occur in a shell argument or pathname, so the loop covers every possible
# control byte that can reach this function.
json_escape() {
    local value output char escaped code i
    local LC_ALL
    value=$1
    output=
    i=0
    LC_ALL=C

    while [ "$i" -lt "${#value}" ]; do
        char=${value:$i:1}
        case "$char" in
            '"') output=$output'\"' ;;
            \\) output=$output'\\' ;;
            *)
                printf -v code '%d' "'$char"
                if [ "$code" -lt 32 ]; then
                    printf -v escaped '\\u%04X' "$code"
                    output=$output$escaped
                else
                    output=$output$char
                fi
                ;;
        esac
        i=$((i + 1))
    done
    printf '%s' "$output"
}

# Resolve directory symlinks and a final symlink without requiring GNU
# readlink flags or a realpath utility.
canonical_path() {
    local path directory base target resolved hops
    path=$1
    hops=0

    while [ -L "$path" ]; do
        hops=$((hops + 1))
        [ "$hops" -le 40 ] || return 1
        directory=$(dirname -- "$path") || return 1
        base=$(basename -- "$path") || return 1
        resolved=$(CDPATH= cd -P -- "$directory" 2>/dev/null && pwd) ||
            return 1
        target=$(readlink "$resolved/$base") || return 1
        case "$target" in
            /*) path=$target ;;
            *) path=$resolved/$target ;;
        esac
    done

    directory=$(dirname -- "$path") || return 1
    base=$(basename -- "$path") || return 1
    resolved=$(CDPATH= cd -P -- "$directory" 2>/dev/null && pwd) ||
        return 1
    printf '%s/%s\n' "$resolved" "$base"
}

# Tri-state: 0 = the two names refer to the same file, 1 = distinct,
# 2 = either path cannot be resolved. Callers must handle all three.
paths_alias() {
    local left right left_real right_real left_dir right_dir left_base right_base
    local left_folded right_folded
    left=$1
    right=$2
    left_real=$(canonical_path "$left") || return 2
    right_real=$(canonical_path "$right") || return 2
    [ "$left_real" = "$right_real" ] && return 0

    if [ -e "$left" ] && [ -e "$right" ] && [ "$left" -ef "$right" ]; then
        return 0
    fi
    # If both inodes exist, -ef was conclusive. Otherwise refuse two absent
    # names that a case-insensitive volume would resolve to the same entry.
    if [ ! -e "$left" ] || [ ! -e "$right" ]; then
        left_dir=${left_real%/*}
        right_dir=${right_real%/*}
        left_base=${left_real##*/}
        right_base=${right_real##*/}
        left_folded=$(printf '%s' "$left_base" |
            LC_ALL=C tr '[:upper:]' '[:lower:]')
        right_folded=$(printf '%s' "$right_base" |
            LC_ALL=C tr '[:upper:]' '[:lower:]')
        [ "$left_dir" = "$right_dir" ] &&
            [ "$left_folded" = "$right_folded" ] && return 0
    fi
    return 1
}

# Print the whitespace-normalized start time (lstart) of a PID — the
# identity stamp used everywhere to detect PID reuse. Empty if the PID is
# gone.
process_identity() {
    LC_ALL=C ps -o lstart= -p "$1" 2>/dev/null |
        awk 'NF { $1=$1; print; exit }'
}

# Read and normalize one complete "pid TAB lstart" owner record.
lock_owner_record() {
    local owner_pid owner_identity
    [ -r "$1" ] || return 1
    read -r owner_pid owner_identity < "$1" || return 1
    is_nonnegative_integer "$owner_pid" || return 1
    [ -n "$owner_identity" ] || return 1
    printf '%s\t%s' "$owner_pid" "$owner_identity"
}

lock_record_is_current() {
    local record owner_pid owner_identity current tab
    record=$1
    tab=$(printf '\t')
    IFS="$tab" read -r owner_pid owner_identity <<EOF
$record
EOF
    pid_is_live "$owner_pid" || return 1
    current=$(process_identity "$owner_pid") || return 1
    [ -n "$current" ] && [ "$current" = "$owner_identity" ]
}

# Take the journal lock (mkdir is the atomic acquire; the owner file is
# written just after, so a fresh lock briefly has no owner — the 4-spin
# grace below tolerates that window). After ~0.2s of contention a lock
# whose recorded owner is dead is stolen by renaming the directory aside;
# the steal is journaled as "stale_lock_recovered" on the next append.
# Gives up (rc 1) after ~10s of live contention.
acquire_journal_lock() {
    local spins owner_file identity stale_dir stale_owner moved_owner restore_spins
    spins=0
    owner_file="$JOURNAL_LOCK_DIR/owner"
    identity=$(process_identity "$$") || identity=
    if [ -z "$identity" ]; then
        echo "codex-supervised.sh: cannot identify journal lock owner" >&2
        return 1
    fi
    while ! mkdir "$JOURNAL_LOCK_DIR" 2>/dev/null; do
        if [ ! -d "$JOURNAL_LOCK_DIR" ]; then
            echo "codex-supervised.sh: cannot create journal lock: $JOURNAL_LOCK_DIR" >&2
            return 1
        fi
        spins=$((spins + 1))
        stale_owner=
        if [ "$spins" -ge 4 ]; then
            stale_owner=$(lock_owner_record "$owner_file") || stale_owner=
        fi
        if [ -n "$stale_owner" ] &&
           ! lock_record_is_current "$stale_owner"; then
            stale_dir="${JOURNAL_LOCK_DIR}.stale.$$.$spins.$RANDOM"
            if [ ! -e "$stale_dir" ] &&
               mv "$JOURNAL_LOCK_DIR" "$stale_dir" 2>/dev/null; then
                moved_owner=$(lock_owner_record "$stale_dir/owner") ||
                    moved_owner=
                if [ "$moved_owner" = "$stale_owner" ]; then
                    rm -f "$stale_dir/owner"
                    rmdir "$stale_dir" 2>/dev/null || true
                    STALE_LOCK_RECOVERED=1
                    spins=0
                    continue
                fi
                restore_spins=0
                while [ -e "$JOURNAL_LOCK_DIR" ] &&
                      [ "$restore_spins" -lt 200 ]; do
                    sleep 0.05
                    restore_spins=$((restore_spins + 1))
                done
                if [ -e "$JOURNAL_LOCK_DIR" ] ||
                   ! mv "$stale_dir" "$JOURNAL_LOCK_DIR" 2>/dev/null; then
                    echo "codex-supervised.sh: cannot restore changed journal lock" >&2
                    return 1
                fi
                sleep 0.05
                continue
            fi
        fi
        if [ "$spins" -ge 200 ]; then
            echo "codex-supervised.sh: timed out waiting for journal lock: $JOURNAL_LOCK_DIR" >&2
            return 1
        fi
        sleep 0.05
    done

    if ! printf '%s\t%s\n' "$$" "$identity" > "$owner_file"; then
        rm -f "$owner_file"
        rmdir "$JOURNAL_LOCK_DIR" 2>/dev/null || true
        echo "codex-supervised.sh: cannot write journal lock owner" >&2
        return 1
    fi
    JOURNAL_LOCK_HELD=1
    JOURNAL_LOCK_IDENTITY=$identity
    return 0
}

append_lock_not_ours() {
    local timestamp escaped_brief
    timestamp=$(date -u +%Y-%m-%dT%H:%M:%SZ) || return 1
    escaped_brief=$(json_escape "$BRIEF_FILE") || return 1
    printf '{"ts":"%s","run_id":"%s","brief":"%s","event":"lock_not_ours"}\n' \
        "$timestamp" "$RUN_ID" "$escaped_brief" >> "$JOURNAL_FILE"
}

release_journal_lock() {
    local owner expected
    [ "${JOURNAL_LOCK_HELD:-0}" -eq 1 ] || return 0
    owner=$(lock_owner_record "$JOURNAL_LOCK_DIR/owner") || owner=
    expected=$(printf '%s\t%s' "$$" "${JOURNAL_LOCK_IDENTITY:-}")
    if [ "$owner" != "$expected" ]; then
        JOURNAL_LOCK_HELD=0
        append_lock_not_ours || true
        echo "codex-supervised.sh: journal lock is not ours; leaving it" >&2
        return 1
    fi
    rm -f "$JOURNAL_LOCK_DIR/owner"
    if ! rmdir "$JOURNAL_LOCK_DIR" 2>/dev/null; then
        echo "codex-supervised.sh: cannot release journal lock: $JOURNAL_LOCK_DIR" >&2
        return 1
    fi
    JOURNAL_LOCK_HELD=0
    JOURNAL_LOCK_IDENTITY=
    return 0
}

# Append one journal line for `event` under the lock. `extra` must be empty
# or a raw JSON fragment starting with a comma (e.g. ',"retries":1'). A
# pending stale-lock recovery is journaled first, in the same lock hold.
journal_event() {
    local event extra timestamp escaped_brief append_status
    event=$1
    extra=${2:-}
    timestamp=$(date -u +%Y-%m-%dT%H:%M:%SZ) || {
        echo "codex-supervised.sh: cannot create journal timestamp" >&2
        return 1
    }
    escaped_brief=$(json_escape "$BRIEF_FILE") || return 1
    acquire_journal_lock || return 1

    append_status=0
    if [ "${STALE_LOCK_RECOVERED:-0}" -eq 1 ]; then
        printf '{"ts":"%s","run_id":"%s","brief":"%s","event":"stale_lock_recovered"}\n' \
            "$timestamp" "$RUN_ID" "$escaped_brief" >> "$JOURNAL_FILE" ||
            append_status=$?
        STALE_LOCK_RECOVERED=0
    fi
    if [ "$append_status" -eq 0 ]; then
        printf '{"ts":"%s","run_id":"%s","brief":"%s","event":"%s"%s}\n' \
            "$timestamp" "$RUN_ID" "$escaped_brief" "$event" "$extra" \
            >> "$JOURNAL_FILE" || append_status=$?
    fi
    if [ "$append_status" -ne 0 ]; then
        echo "codex-supervised.sh: cannot append journal: $JOURNAL_FILE" >&2
    fi
    release_journal_lock || append_status=1
    [ "$append_status" -eq 0 ]
}

# True if the PID exists and is not a zombie (kill -0 alone would count
# zombies as alive and stall the reap loops forever).
pid_is_live() {
    local state
    kill -0 "$1" 2>/dev/null || return 1
    state=$(ps -o stat= -p "$1" 2>/dev/null | tr -d '[:space:]')
    case "$state" in
        ""|Z*) return 1 ;;
        *) return 0 ;;
    esac
}

SUPERVISE_LIB_DIR=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd) ||
    return 1
# shellcheck source=scripts/supervise-process-lib.sh
. "$SUPERVISE_LIB_DIR/supervise-process-lib.sh" || return 1
unset SUPERVISE_LIB_DIR
