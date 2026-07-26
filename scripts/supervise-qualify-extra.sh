#!/bin/bash
#
# Additional qualification cases for codex-supervised.sh. Sourced by
# supervise-qualify.sh after the shared fake codex, watchdog, and pass/fail
# helpers exist; it defines run_dirty_git_safety plus the run_extra_cases
# bundle, and executes nothing on its own (running this file directly is a
# no-op).

# Last-resort, identity-checked kill of every helper process recorded in
# the suite's *.pids files; invoked from the main suite's EXIT trap.
cleanup_fake_pids() {
    cleanup_fake_pids_with_signal TERM
    sleep 1
    cleanup_fake_pids_with_signal KILL
}

cleanup_fake_pids_with_signal() {
    local signal file pid identity current
    signal=$1
    for file in "$TMP_ROOT"/*.pids; do
        [ -f "$file" ] || continue
        while read -r pid identity; do
            [ -n "$pid" ] && [ -n "$identity" ] || continue
            current=$(process_identity "$pid") || current=
            [ "$current" = "$identity" ] || continue
            kill "-$signal" "$pid" 2>/dev/null || true
        done < "$file"
    done
}

run_interrupt_journal() {
    local brief output pids status events marker
    brief="$TMP_ROOT/interrupt.brief"; output="$TMP_ROOT/interrupt.out"
    pids="$TMP_ROOT/interrupt.pids"; marker="$TMP_ROOT/interrupt.timeout"
    printf 'Exercise signal cleanup.' > "$brief"
    run_with_watchdog "$marker" env FAKE_MODE=interrupt FAKE_PID_FILE="$pids" \
        CODEX_BIN="$FAKE_CODEX" HAIDER_RUN_JOURNAL="$JOURNAL" \
        bash "$WRAPPER" "$brief" "$output" --max-stall-secs 30 --max-retries 0
    status=$?; events=$(events_for_brief "$brief")
    if [ "$status" -eq 143 ] && [ "$events" = "start interrupted" ] &&
       all_pids_reaped "$pids"; then
        pass "signal cleanup and interrupted journal"
    else
        fail "signal cleanup and interrupted journal" \
            "status=$status events=[$events]"
    fi
}

run_alias_refusal() {
    local brief output original status events marker log
    brief="$TMP_ROOT/alias.brief"; marker="$TMP_ROOT/alias.timeout"
    output="$TMP_ROOT/alias-output"; log="$TMP_ROOT/alias.log"
    original='brief must remain intact'
    printf '%s' "$original" > "$brief"
    ln -s "$brief" "$output" || { fail "destination alias refusal" "fixture failed"; return; }
    run_with_watchdog "$marker" env FAKE_MODE=happy CODEX_BIN="$FAKE_CODEX" \
        HAIDER_RUN_JOURNAL="$JOURNAL" bash "$WRAPPER" "$brief" "$output" \
        --max-stall-secs 3 --max-retries 0 > "$log" 2>&1
    status=$?; events=$(events_for_brief "$brief")
    if [ "$status" -eq 2 ] && [ "$(cat "$brief")" = "$original" ] &&
       [ -z "$events" ] && grep -q 'paths alias each other' "$log"; then
        pass "destination alias refusal"
    else
        fail "destination alias refusal" "status=$status events=[$events]"
    fi
}

run_journal_failure() {
    local brief output bad_journal status marker log
    brief="$TMP_ROOT/journal-failure.brief"
    output="$TMP_ROOT/journal-failure.out"
    bad_journal="$TMP_ROOT/journal-as-directory"
    marker="$TMP_ROOT/journal-failure.timeout"
    log="$TMP_ROOT/journal-failure.log"
    printf 'The fake must not start.' > "$brief"
    mkdir "$bad_journal" || { fail "journal append failure" "fixture failed"; return; }
    run_with_watchdog "$marker" env FAKE_MODE=happy CODEX_BIN="$FAKE_CODEX" \
        HAIDER_RUN_JOURNAL="$bad_journal" bash "$WRAPPER" "$brief" "$output" \
        --max-stall-secs 3 --max-retries 0 > "$log" 2>&1
    status=$?
    if [ "$status" -eq 2 ] && grep -q 'cannot append journal' "$log"; then
        pass "journal append failure is fatal"
    else
        fail "journal append failure is fatal" "status=$status"
    fi
}

run_json_escape_check() {
    local input escaped expected piece i quote_slash
    input=$(printf '\001\002\003\004\005\006\007\010\011\012\013\014\015\016\017\020\021\022\023\024\025\026\027\030\031\032\033\034\035\036\037')
    escaped=$(json_escape "$input")
    expected=
    i=1
    while [ "$i" -lt 32 ]; do
        printf -v piece '\\u%04X' "$i"
        expected=$expected$piece
        i=$((i + 1))
    done
    quote_slash=$(json_escape '"\')
    # The awk pass exits nonzero when any journal line lacks a run_id.
    if [ "$escaped" = "$expected" ] && [ "$quote_slash" = '\"\\' ] &&
       ! awk 'index($0, "\"run_id\":\"") == 0 { bad=1 } END { exit bad }' \
            "$JOURNAL"; then
        fail "JSON escaping and run IDs" "journal record missing run_id"
    elif [ "$escaped" = "$expected" ] && [ "$quote_slash" = '\"\\' ]; then
        pass "JSON escaping and run IDs"
    else
        fail "JSON escaping and run IDs" "escaped bytes differ"
    fi
}

run_dirty_git_safety() {
    local brief output repo before after status marker
    brief="$TMP_ROOT/dirty.brief"; output="$TMP_ROOT/dirty.out"
    repo="$TMP_ROOT/dirty-repo"; marker="$TMP_ROOT/dirty.timeout"
    printf 'Make no repository changes.' > "$brief"
    git init -q "$repo" ||
        { fail "dirty-git safety" "git init failed"; return; }
    printf 'staged\n' > "$repo/tracked.txt"
    git -C "$repo" add tracked.txt ||
        { fail "dirty-git safety" "git add failed"; return; }
    printf 'dirty\n' >> "$repo/tracked.txt"
    before=$(git -C "$repo" status --porcelain=v1 --untracked-files=all)
    run_with_watchdog "$marker" env FAKE_MODE=no_changes CODEX_BIN="$FAKE_CODEX" \
        HAIDER_RUN_JOURNAL="$JOURNAL" bash -c \
        'cd "$1" && exec bash "$2" "$3" "$4" --max-stall-secs 3 --max-retries 0' \
        case "$repo" "$WRAPPER" "$brief" "$output"
    status=$?
    after=$(git -C "$repo" status --porcelain=v1 --untracked-files=all)
    if [ "$status" -eq 0 ] && [ -n "$before" ] && [ "$before" = "$after" ]; then
        pass "dirty-git safety"
    else
        fail "dirty-git safety" "status=$status or git status changed"
    fi
}

run_case_folded_alias_refusal() {
    local brief output journal probe_lower probe_upper status marker log
    probe_lower="$TMP_ROOT/case-fold-probe"
    probe_upper="$TMP_ROOT/CASE-FOLD-PROBE"
    : > "$probe_lower" || { fail "case-folded alias refusal" "probe failed"; return; }
    if [ ! -e "$probe_upper" ] || [ ! "$probe_lower" -ef "$probe_upper" ]; then
        rm -f "$probe_lower"
        pass "case-folded alias refusal (case-sensitive volume: skipped)"
        return
    fi
    rm -f "$probe_lower"

    brief="$TMP_ROOT/case-fold.brief"
    output="$TMP_ROOT/CASE-FOLD-ALIAS"
    journal="$TMP_ROOT/case-fold-alias"
    marker="$TMP_ROOT/case-fold.timeout"
    log="$TMP_ROOT/case-fold.log"
    printf 'The fake must not start.' > "$brief"
    run_with_watchdog "$marker" env FAKE_MODE=happy CODEX_BIN="$FAKE_CODEX" \
        HAIDER_RUN_JOURNAL="$journal" bash "$WRAPPER" "$brief" "$output" \
        --max-stall-secs 3 --max-retries 0 > "$log" 2>&1
    status=$?
    if [ "$status" -eq 2 ] && [ ! -e "$output" ] &&
       [ ! -e "$journal" ] && grep -q 'paths alias each other' "$log"; then
        pass "case-folded alias refusal"
    else
        fail "case-folded alias refusal" "status=$status"
    fi
}

run_detached_orphan_reap() {
    local brief output pids status events marker
    brief="$TMP_ROOT/detached.brief"; output="$TMP_ROOT/detached.out"
    pids="$TMP_ROOT/detached.pids"; marker="$TMP_ROOT/detached.timeout"
    printf 'Exercise detached leader-exit cleanup.' > "$brief"
    run_with_watchdog "$marker" env FAKE_MODE=detached_leader_exits \
        FAKE_PID_FILE="$pids" CODEX_BIN="$FAKE_CODEX" \
        HAIDER_RUN_JOURNAL="$JOURNAL" bash "$WRAPPER" "$brief" "$output" \
        --max-stall-secs 3 --max-retries 0
    status=$?; events=$(events_for_brief "$brief")
    if [ "$status" -eq 0 ] &&
       [ "$events" = "start orphans_reaped done" ] &&
       [ -s "$pids" ] && all_pids_reaped "$pids"; then
        pass "setsid-detached leader-exit orphan reap"
    else
        fail "setsid-detached leader-exit orphan reap" \
            "status=$status events=[$events]"
    fi
}

run_extra_cases() {
    run_interrupt_journal
    run_alias_refusal
    run_journal_failure
    run_json_escape_check
    run_case_folded_alias_refusal
    run_detached_orphan_reap
}
