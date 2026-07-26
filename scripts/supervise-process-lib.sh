#!/bin/bash
#
# Identity-safe process discovery and reaping. Sourced by supervise-lib.sh.
#
# Owns the PID-identity invariant: every PID recorded in a tree file is
# stamped with its ps lstart (process start time), and no signal or liveness
# verdict is ever applied to a PID whose current lstart differs from the
# recorded one — a mismatch means the kernel reused the PID for an unrelated
# process, so the entry is skipped (and journaled via pid_reuse_skipped).
#
# Tree-file row format, one process per line:
#   <pid> TAB <pgid> TAB <lstart>
# Rows are only appended; entries for exited processes stay in the file and
# are filtered out at read time by the identity check.

# Best-effort journal note that a recorded PID was recycled by the kernel.
# No-op unless the wrapper's journaling is up (JOURNAL_STARTED), not already
# inside a journal write (IN_JOURNAL, avoids re-entry), and the journal is
# not the thing that failed (JOURNAL_BROKEN, avoids recursing during the
# emergency teardown that a journal failure triggers).
pid_reuse_skipped() {
    local pid
    pid=$1
    [ "${JOURNAL_STARTED:-0}" -eq 1 ] || return 0
    [ "${IN_JOURNAL:-0}" -eq 0 ] || return 0
    [ "${JOURNAL_BROKEN:-0}" -eq 0 ] || return 0
    type record_event >/dev/null 2>&1 || return 0
    record_event "pid_reused_skipped" ',"pid":'"$pid"
}

# Append one identity-stamped row for `pid` to tree file `file`. Fails (rc 1)
# if the process is already gone, since no identity can be captured then.
remember_pid() {
    local pid file details ppid pgid identity
    pid=$1
    file=$2
    details=$(LC_ALL=C ps -o ppid=,pgid=,lstart= -p "$pid" 2>/dev/null |
        awk 'NF { $1=$1; print; exit }') || return 1
    read -r ppid pgid identity <<EOF
$details
EOF
    [ -n "$pgid" ] && [ -n "$identity" ] || return 1
    printf '%s\t%s\t%s\n' "$pid" "$pgid" "$identity" >> "$file"
}

# Grow tree file `tree_file` with every live descendant of `root`, using one
# atomic `ps -axo` capture (written to scratch file `ps_file`) so parentage
# and identities come from the same instant. The awk pass re-activates prior
# rows only when pid AND lstart still match (reused PIDs stay inactive, so
# their new children are not adopted), walks child edges to a fixpoint, and
# appends rows for newly discovered PIDs. With include_group=1 it also adopts
# processes whose pgid equals root — but only if root led its own process
# group (pid == pgid) and root's PID was not itself reused.
collect_process_tree() {
    local root tree_file ps_file include_group
    root=$1
    tree_file=$2
    ps_file=$3
    include_group=${4:-0}

    LC_ALL=C ps -axo pid=,ppid=,pgid=,lstart= > "$ps_file" 2>/dev/null ||
        return 1
    awk -v root="$root" -v include_group="$include_group" '
        FILENAME == ARGV[1] {
            if (NF >= 3) {
                identity=$3
                for (i=4; i<=NF; i++) identity=identity " " $i
                saved[$1 SUBSEP identity]=1
                recorded[$1]=1
                if ($1 == root && $2 == root) group_owned=1
                old[++old_count]=$0
            }
            next
        }
        NF >= 4 {
            pid=$1
            order[++count]=pid
            parent[pid]=$2
            group[pid]=$3
            identity=$4
            for (i=5; i<=NF; i++) identity=identity " " $i
            start[pid]=identity
            present[pid]=1
        }
        END {
            for (i=1; i<=count; i++) {
                pid=order[i]
                if (saved[pid SUBSEP start[pid]]) active[pid]=1
            }
            root_reused=recorded[root] && present[root] && !active[root]
            if (!recorded[root] && present[root]) {
                active[root]=1
                added[root]=1
            }
            if (include_group && group_owned && !root_reused)
                for (i=1; i<=count; i++) {
                    pid=order[i]
                    if (group[pid] == root && !active[pid]) {
                        active[pid]=1
                        added[pid]=1
                    }
                }
            changed=1
            while (changed) {
                changed=0
                for (i=1; i<=count; i++) {
                    pid=order[i]
                    if (!active[pid] && active[parent[pid]]) {
                        active[pid]=1
                        added[pid]=1
                        changed=1
                    }
                }
            }
            for (i=1; i<=old_count; i++) print old[i]
            for (i=1; i<=count; i++)
                if (added[order[i]]) {
                    pid=order[i]
                    print pid "\t" group[pid] "\t" start[pid]
                }
        }
    ' "$tree_file" "$ps_file" > "$ps_file.next" || return 1
    mv "$ps_file.next" "$tree_file"
}

# True (rc 0) if any tree-file row still names a live, identity-matching
# process. Rows whose PID vanished or was reused are skipped, not errors.
pid_file_has_survivors() {
    local file pid pgid identity current tab
    file=$1
    tab=$(printf '\t')
    while IFS="$tab" read -r pid pgid identity; do
        [ -n "$pid" ] && [ -n "$identity" ] || continue
        current=$(process_identity "$pid") || current=
        if [ -z "$current" ]; then
            kill -0 "$pid" 2>/dev/null && pid_reuse_skipped "$pid"
            continue
        fi
        if [ "$current" != "$identity" ]; then
            pid_reuse_skipped "$pid"
            continue
        fi
        pid_is_live "$pid" && return 0
    done < "$file"
    return 1
}

# Send `signal` to each identity-matching tree-file row, never to ourselves
# and never to a PID whose lstart changed (that would hit a reused PID).
signal_pid_file() {
    local signal file pid pgid identity current tab
    signal=$1
    file=$2
    tab=$(printf '\t')
    while IFS="$tab" read -r pid pgid identity; do
        [ -n "$pid" ] && [ -n "$identity" ] || continue
        [ "$pid" = "$$" ] && continue
        current=$(process_identity "$pid") || current=
        if [ -z "$current" ]; then
            kill -0 "$pid" 2>/dev/null && pid_reuse_skipped "$pid"
            continue
        fi
        if [ "$current" != "$identity" ]; then
            pid_reuse_skipped "$pid"
            continue
        fi
        kill "-$signal" "$pid" 2>/dev/null || true
    done < "$file"
}

# True if a non-zombie process still belongs to `pgid`.
process_group_has_survivors() {
    LC_ALL=C ps -axo pgid=,stat= 2>/dev/null |
        awk -v pgid="$1" '$1 == pgid && $2 !~ /^Z/ { found=1; exit }
            END { exit !found }'
}

# Wait up to `seconds` (1s polls) for a group or recorded PIDs to exit.
poll_process_group() {
    local pgid seconds elapsed
    pgid=$1
    seconds=$2
    elapsed=0
    while process_group_has_survivors "$pgid"; do
        [ "$elapsed" -lt "$seconds" ] || return 1
        sleep 1
        elapsed=$((elapsed + 1))
    done
    return 0
}

poll_survivors() {
    local tree_file seconds elapsed
    tree_file=$1
    seconds=$2
    elapsed=0
    while pid_file_has_survivors "$tree_file"; do
        [ "$elapsed" -lt "$seconds" ] || return 1
        sleep 1
        elapsed=$((elapsed + 1))
    done
    return 0
}

# TERM every tree-file survivor, escalate to KILL after 2s, and report
# success (rc 0) only when none survive.
reap_survivors() {
    local tree_file
    tree_file=$1
    signal_pid_file TERM "$tree_file"
    poll_survivors "$tree_file" 2 || true
    if pid_file_has_survivors "$tree_file"; then
        signal_pid_file KILL "$tree_file"
        poll_survivors "$tree_file" 3 || true
    fi
    ! pid_file_has_survivors "$tree_file"
}

# Capture every live process that still matches root's group or an exact
# recorded PID+lstart. The survivor file is the final authority for a pass.
snapshot_reap_survivors() {
    local root tree_file ps_file survivor_file
    root=$1
    tree_file=$2
    ps_file=$3
    survivor_file=$4
    LC_ALL=C ps -axo pid=,pgid=,stat=,lstart= > "$ps_file" 2>/dev/null ||
        return 1
    awk -v root="$root" '
        FILENAME == ARGV[1] {
            identity=$3
            for (i=4; i<=NF; i++) identity=identity " " $i
            saved[$1 SUBSEP identity]=1
            next
        }
        NF >= 4 {
            identity=$4
            for (i=5; i<=NF; i++) identity=identity " " $i
            if ($3 !~ /^Z/ &&
                ($2 == root || saved[$1 SUBSEP identity]) && !seen[$1]++)
                print $1 "\t" $2 "\t" identity
        }
    ' "$tree_file" "$ps_file" > "$survivor_file"
}

survivor_pid_json() {
    awk 'BEGIN { printf "[" }
        { printf "%s%s", separator, $1; separator="," }
        END { print "]" }' "$1"
}

# Reap in three ordered layers, retrying the complete sequence at most three
# times: process group, identity-stamped escapees, then a fresh verification.
terminate_process_tree() {
    local root tree_file ps_file survivor_file pass survivors
    root=$1
    tree_file=$2
    ps_file=$3
    survivor_file="${ps_file}.survivors"
    pass=1
    while [ "$pass" -le 3 ]; do
        collect_process_tree "$root" "$tree_file" "$ps_file" 1 || true

        kill -TERM -- "-$root" 2>/dev/null || true
        poll_process_group "$root" 2 || true
        if process_group_has_survivors "$root"; then
            kill -KILL -- "-$root" 2>/dev/null || true
            poll_process_group "$root" 3 || true
        fi

        reap_survivors "$tree_file" || true
        wait "$root" 2>/dev/null || true
        collect_process_tree "$root" "$tree_file" "$ps_file" 1 || true
        if snapshot_reap_survivors \
            "$root" "$tree_file" "$ps_file" "$survivor_file" &&
           [ ! -s "$survivor_file" ]; then
            rm -f "$survivor_file"
            return 0
        fi
        pass=$((pass + 1))
    done

    survivors=$(survivor_pid_json "$survivor_file")
    if [ "${JOURNAL_STARTED:-0}" -eq 1 ] &&
       [ "${JOURNAL_BROKEN:-0}" -eq 0 ] &&
       type record_event >/dev/null 2>&1; then
        record_event "reap_incomplete" ',"survivors":'"$survivors"
    fi
    echo "codex-supervised.sh: incomplete reap; survivors=$survivors" >&2
    return 1
}
