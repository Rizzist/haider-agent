#!/bin/bash
#
# Shared, Bash 3.2/BSD-compatible helpers for the Codex supervision harness.
# Sourced (never executed); also pulls in supervise-process-lib.sh.
#
# Owns two invariants:
#   Journal format — one JSON object per line, always carrying
#   {"ts","run_id","brief","event"} plus event-specific fields. Each run
#   writes only its own file, so appends need no cross-process lock.
#
# The journal function reads globals owned by codex-supervised.sh:
# JOURNAL_FILE, RUN_ID, and BRIEF_FILE.

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

# Append one journal line for `event`. `extra` must be empty or a raw JSON
# fragment starting with a comma (e.g. ',"retries":1').
journal_event() {
    local event extra timestamp escaped_brief
    event=$1
    extra=${2:-}
    timestamp=$(date -u +%Y-%m-%dT%H:%M:%SZ) || {
        echo "codex-supervised.sh: cannot create journal timestamp" >&2
        return 1
    }
    escaped_brief=$(json_escape "$BRIEF_FILE") || return 1
    if ! printf '{"ts":"%s","run_id":"%s","brief":"%s","event":"%s"%s}\n' \
        "$timestamp" "$RUN_ID" "$escaped_brief" "$event" "$extra" \
        >> "$JOURNAL_FILE"; then
        echo "codex-supervised.sh: cannot append journal: $JOURNAL_FILE" >&2
        return 1
    fi
    return 0
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

# create_run_journal: alias-validate and no-clobber-create $JOURNAL_FILE.
# Uses wrapper globals (BRIEF_FILE/OUTPUT_FILE/STDERR_FILE/JOURNAL_DIR/RUN_ID/
# JOURNAL_FILE) and may randomize RUN_ID on collision. Dies via wrapper's die.
check_journal_aliases() {
    for _dest in "$BRIEF_FILE" "$OUTPUT_FILE" "$STDERR_FILE"; do
        paths_alias "$_dest" "$1"
        case $? in
            2) die "cannot resolve destination or journal file path" ;;
            0) die "destination and journal file paths alias each other" ;;
        esac
    done
}
create_run_journal() {
    check_journal_aliases "$JOURNAL_FILE"
    if ! (set -o noclobber; : > "$JOURNAL_FILE") 2>/dev/null; then
        RUN_ID="${RUN_ID}-$RANDOM"
        JOURNAL_FILE="$JOURNAL_DIR/run-$RUN_ID.jsonl"
        check_journal_aliases "$JOURNAL_FILE"
        (set -o noclobber; : > "$JOURNAL_FILE") 2>/dev/null || return 1
        echo "journal: $JOURNAL_FILE" >&2
    fi
    return 0
}
