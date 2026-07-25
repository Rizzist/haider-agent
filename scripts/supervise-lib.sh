#!/bin/bash
#
# Shared, Bash 3.2-compatible helpers for the Codex supervision harness.

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

paths_alias() {
    local left right left_real right_real
    left=$1
    right=$2
    left_real=$(canonical_path "$left") || return 2
    right_real=$(canonical_path "$right") || return 2
    [ "$left_real" = "$right_real" ] && return 0
    if [ -e "$left" ] && [ -e "$right" ] && [ "$left" -ef "$right" ]; then
        return 0
    fi
    return 1
}

acquire_journal_lock() {
    local spins
    spins=0
    while ! mkdir "$JOURNAL_LOCK_DIR" 2>/dev/null; do
        if [ ! -d "$JOURNAL_LOCK_DIR" ]; then
            echo "codex-supervised.sh: cannot create journal lock: $JOURNAL_LOCK_DIR" >&2
            return 1
        fi
        spins=$((spins + 1))
        if [ "$spins" -ge 200 ]; then
            echo "codex-supervised.sh: timed out waiting for journal lock: $JOURNAL_LOCK_DIR" >&2
            return 1
        fi
        sleep 0.05
    done
    JOURNAL_LOCK_HELD=1
    return 0
}

release_journal_lock() {
    [ "${JOURNAL_LOCK_HELD:-0}" -eq 1 ] || return 0
    if ! rmdir "$JOURNAL_LOCK_DIR" 2>/dev/null; then
        echo "codex-supervised.sh: cannot release journal lock: $JOURNAL_LOCK_DIR" >&2
        return 1
    fi
    JOURNAL_LOCK_HELD=0
    return 0
}

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
    printf '{"ts":"%s","run_id":"%s","brief":"%s","event":"%s"%s}\n' \
        "$timestamp" "$RUN_ID" "$escaped_brief" "$event" "$extra" \
        >> "$JOURNAL_FILE" || append_status=$?
    if [ "$append_status" -ne 0 ]; then
        echo "codex-supervised.sh: cannot append journal: $JOURNAL_FILE" >&2
    fi
    release_journal_lock || append_status=1
    [ "$append_status" -eq 0 ]
}

group_exists() {
    kill -0 -- "-$1" 2>/dev/null
}

pid_is_live() {
    local state
    kill -0 "$1" 2>/dev/null || return 1
    state=$(ps -o stat= -p "$1" 2>/dev/null | tr -d '[:space:]')
    case "$state" in
        ""|Z*) return 1 ;;
        *) return 0 ;;
    esac
}

remember_pid() {
    local pid file
    pid=$1
    file=$2
    grep -q "^${pid}\$" "$file" 2>/dev/null && return 1
    printf '%s\n' "$pid" >> "$file"
}

# Take one process-table snapshot, then repeatedly add children of every PID
# already known until a complete recursive descendant-tree fixpoint is found.
collect_process_tree() {
    local root tree_file ps_file changed pid ppid
    root=$1
    tree_file=$2
    ps_file=$3
    remember_pid "$root" "$tree_file" >/dev/null 2>&1 || true
    ps -axo pid=,ppid= > "$ps_file" 2>/dev/null || return 1

    changed=1
    while [ "$changed" -eq 1 ]; do
        changed=0
        while read -r pid ppid; do
            [ -n "$pid" ] && [ -n "$ppid" ] || continue
            if grep -q "^${ppid}\$" "$tree_file" 2>/dev/null; then
                if remember_pid "$pid" "$tree_file"; then
                    changed=1
                fi
            fi
        done < "$ps_file"
    done
    return 0
}

pid_file_has_survivors() {
    local file pid
    file=$1
    while IFS= read -r pid; do
        [ -n "$pid" ] || continue
        pid_is_live "$pid" && return 0
    done < "$file"
    return 1
}

signal_pid_file() {
    local signal file pid
    signal=$1
    file=$2
    while IFS= read -r pid; do
        [ -n "$pid" ] || continue
        [ "$pid" = "$$" ] && continue
        kill "-$signal" "$pid" 2>/dev/null || true
    done < "$file"
}

survivors_remain() {
    local pgid tree_file
    pgid=$1
    tree_file=$2
    group_exists "$pgid" || pid_file_has_survivors "$tree_file"
}

poll_survivors() {
    local pgid tree_file seconds elapsed
    pgid=$1
    tree_file=$2
    seconds=$3
    elapsed=0
    while survivors_remain "$pgid" "$tree_file"; do
        [ "$elapsed" -lt "$seconds" ] || return 1
        sleep 1
        elapsed=$((elapsed + 1))
    done
    return 0
}

# Signal the group first. If anything remains, always run the recursive PID
# fallback, even when group signalling itself reported success.
reap_survivors() {
    local pgid tree_file
    pgid=$1
    tree_file=$2

    kill -TERM -- "-$pgid" 2>/dev/null || true
    if ! poll_survivors "$pgid" "$tree_file" 1; then
        signal_pid_file TERM "$tree_file"
        poll_survivors "$pgid" "$tree_file" 2 || true
    fi
    if survivors_remain "$pgid" "$tree_file"; then
        kill -KILL -- "-$pgid" 2>/dev/null || true
        signal_pid_file KILL "$tree_file"
        poll_survivors "$pgid" "$tree_file" 3 || true
    fi
    ! survivors_remain "$pgid" "$tree_file"
}

terminate_process_tree() {
    local root tree_file ps_file result
    root=$1
    tree_file=$2
    ps_file=$3
    collect_process_tree "$root" "$tree_file" "$ps_file" || true
    reap_survivors "$root" "$tree_file"
    result=$?
    wait "$root" 2>/dev/null || true
    if group_exists "$root"; then
        kill -KILL -- "-$root" 2>/dev/null || true
        poll_survivors "$root" "$tree_file" 3 || true
    fi
    survivors_remain "$root" "$tree_file" && result=1
    return "$result"
}
