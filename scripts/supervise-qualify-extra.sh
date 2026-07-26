#!/bin/bash
# Additional qualification cases for codex-supervised.sh. Sourced by
# supervise-qualify.sh after the shared fake codex, watchdog, and pass/fail
# helpers exist; it defines run_dirty_git_safety plus the run_extra_cases
# bundle. Direct execution delegates to the complete qualification gate.
if [ "${BASH_SOURCE[0]}" = "$0" ]; then
    exec bash "$(dirname -- "$0")/supervise-qualify.sh"
fi
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
        CODEX_BIN="$FAKE_CODEX" HAIDER_RUN_JOURNAL="$JOURNAL_DIR" \
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
    local brief output original status events marker
    brief="$TMP_ROOT/alias.brief"; marker="$TMP_ROOT/alias.timeout"
    output="$TMP_ROOT/alias-output"
    original='brief must remain intact'
    printf '%s' "$original" > "$brief"
    ln -s "$brief" "$output" || { fail "destination alias refusal" "fixture failed"; return; }
    run_with_watchdog "$marker" env FAKE_MODE=happy CODEX_BIN="$FAKE_CODEX" \
        HAIDER_RUN_JOURNAL="$JOURNAL_DIR" bash "$WRAPPER" "$brief" "$output" \
        --max-stall-secs 3 --max-retries 0
    status=$?; events=$(events_for_brief "$brief")
    if [ "$status" -eq 2 ] && [ "$(cat "$brief")" = "$original" ] &&
       [ -z "$events" ] &&
       grep -q 'paths alias each other' "$CASE_STDERR_LOG"; then
        pass "destination alias refusal"
    else
        fail "destination alias refusal" "status=$status events=[$events]"
    fi
}
run_journal_dir_alias_refusal() {
    local brief output journal_dir status events marker
    brief="$TMP_ROOT/journal-dir-alias.brief"
    journal_dir="$TMP_ROOT/journal-dir-alias"
    output=$journal_dir
    marker="$TMP_ROOT/journal-dir-alias.timeout"
    printf 'The fake must not start.' > "$brief"
    mkdir "$journal_dir" ||
        { fail "journal-directory alias refusal" "fixture failed"; return; }
    run_with_watchdog "$marker" env FAKE_MODE=happy CODEX_BIN="$FAKE_CODEX" \
        HAIDER_RUN_JOURNAL="$journal_dir" bash "$WRAPPER" "$brief" "$output" \
        --max-stall-secs 3 --max-retries 0
    status=$?; events=$(events_for_brief "$brief")
    if [ "$status" -eq 2 ] && [ -z "$events" ] &&
       grep -q 'output and journal-directory paths alias each other' \
            "$CASE_STDERR_LOG"; then
        pass "journal-directory alias refusal"
    else
        fail "journal-directory alias refusal" \
            "status=$status events=[$events]"
    fi
}
run_journal_failure() {
    local brief output bad_journal status marker
    brief="$TMP_ROOT/journal-failure.brief"
    output="$TMP_ROOT/journal-failure.out"
    bad_journal="$TMP_ROOT/journal-as-file"
    marker="$TMP_ROOT/journal-failure.timeout"
    printf 'The fake must not start.' > "$brief"
    : > "$bad_journal" || { fail "journal directory failure" "fixture failed"; return; }
    run_with_watchdog "$marker" env FAKE_MODE=happy CODEX_BIN="$FAKE_CODEX" \
        HAIDER_RUN_JOURNAL="$bad_journal" bash "$WRAPPER" "$brief" "$output" \
        --max-stall-secs 3 --max-retries 0
    status=$?
    if [ "$status" -eq 2 ] &&
       grep -q 'cannot create journal directory' "$CASE_STDERR_LOG"; then
        pass "journal directory failure is fatal"
    else
        fail "journal directory failure is fatal" "status=$status"
    fi
}

run_json_escape_check() {
    local input escaped expected piece i quote_slash journal run_id checked bad
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
    checked=0; bad=0
    for journal in "$JOURNAL_DIR"/*.jsonl; do
        [ -f "$journal" ] || continue
        checked=$((checked + 1))
        run_id=$(awk 'NR == 1 {
            value=$0
            sub(/^.*"run_id":"/, "", value)
            sub(/".*$/, "", value)
            print value
        }' "$journal")
        awk -v id="$run_id" \
            'index($0, "\"run_id\":\"" id "\"") == 0 { exit 1 }' "$journal" || bad=1
        [ "${journal##*/}" = "run-$run_id.jsonl" ] || bad=1
    done
    if [ "$escaped" = "$expected" ] && [ "$quote_slash" = '\"\\' ] &&
       { [ "$checked" -eq 0 ] || [ "$bad" -ne 0 ]; }; then
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
        HAIDER_RUN_JOURNAL="$JOURNAL_DIR" bash -c \
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
    local brief output journal probe_lower probe_upper status marker
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
    printf 'The fake must not start.' > "$brief"
    run_with_watchdog "$marker" env FAKE_MODE=happy CODEX_BIN="$FAKE_CODEX" \
        HAIDER_RUN_JOURNAL="$journal" bash "$WRAPPER" "$brief" "$output" \
        --max-stall-secs 3 --max-retries 0
    status=$?
    if [ "$status" -eq 2 ] && [ ! -e "$output" ] &&
       [ ! -e "$journal" ] &&
       grep -q 'paths alias each other' "$CASE_STDERR_LOG"; then
        pass "case-folded alias refusal"
    else
        fail "case-folded alias refusal" "status=$status"
    fi
}

run_detached_orphan_reap() {
    local mode stem label brief output pids status events marker recorded outcome
    mode=$1; stem=$2; label=$3
    brief="$TMP_ROOT/$stem.brief"; output="$TMP_ROOT/$stem.out"
    pids="$TMP_ROOT/$stem.pids"; marker="$TMP_ROOT/$stem.timeout"
    printf 'Exercise detached leader-exit cleanup.' > "$brief"
    run_with_watchdog "$marker" env FAKE_MODE="$mode" \
        FAKE_PID_FILE="$pids" CODEX_BIN="$FAKE_CODEX" \
        HAIDER_RUN_JOURNAL="$JOURNAL_DIR" bash "$WRAPPER" "$brief" "$output" \
        --max-stall-secs 3 --max-retries 0
    status=$?; events=$(events_for_brief "$brief")
    wait_for_pid_record "$pids" || true
    recorded=0; outcome=0
    [ ! -s "$pids" ] || recorded=1
    if all_pids_reaped "$pids"; then
        outcome=1
    else
        case " $events " in
            *" reap_unverifiable "*) outcome=1; cleanup_fake_pids ;;
        esac
    fi
    if [ "$status" -eq 0 ] && [ "$recorded" -eq 1 ] &&
       [ "$outcome" -eq 1 ] && [ "${events% done}" != "$events" ]; then
        pass "$label"
    else
        fail "$label" \
            "status=$status events=[$events]"
    fi
}

wait_for_pid_record() {
    local file elapsed
    file=$1
    elapsed=0
    while [ ! -s "$file" ] && [ "$elapsed" -lt 5 ]; do
        sleep 1
        elapsed=$((elapsed + 1))
    done
    [ -s "$file" ]
}

run_instant_exit_discrimination() {
    local brief output pids marker status events recorded leaked
    local unverifiable reaped_case unverifiable_case outcomes
    brief="$TMP_ROOT/instant.brief"; output="$TMP_ROOT/instant.out"
    pids="$TMP_ROOT/instant.pids"; marker="$TMP_ROOT/instant.timeout"
    printf 'Exercise instant leader-exit cleanup.' > "$brief"
    run_with_watchdog "$marker" env FAKE_MODE=leader_instant_exit \
        FAKE_PID_FILE="$pids" CODEX_BIN="$FAKE_CODEX" \
        HAIDER_RUN_JOURNAL="$JOURNAL_DIR" bash "$WRAPPER" "$brief" "$output" \
        --max-stall-secs 3 --max-retries 0
    status=$?; events=$(events_for_brief "$brief")
    wait_for_pid_record "$pids" || true

    recorded=0; leaked=0; reaped_case=0; unverifiable_case=0
    [ ! -s "$pids" ] || recorded=1
    pids_survive "$pids" && leaked=1
    if [ -f "$CASE_JOURNAL" ]; then
        unverifiable=$(awk '
            index($0, "\"event\":\"reap_unverifiable\"") { count++ }
            END { print count + 0 }
        ' "$CASE_JOURNAL")
    else
        unverifiable=-1
    fi
    if [ "$recorded" -eq 1 ] && [ "$leaked" -eq 0 ] &&
       [ "$unverifiable" -eq 0 ]; then
        reaped_case=1
        rm -f "$pids"
    fi
    if [ "$recorded" -eq 1 ] && [ "$leaked" -eq 1 ] &&
       [ "$unverifiable" -gt 0 ]; then
        cleanup_fake_pids
        if all_pids_reaped "$pids"; then
            unverifiable_case=1
        fi
    fi
    outcomes=$((reaped_case + unverifiable_case))
    if [ "$leaked" -eq 1 ] && [ "$unverifiable_case" -eq 0 ]; then
        cleanup_fake_pids
    fi

    if [ "$status" -eq 0 ] && [ "$outcomes" -eq 1 ] &&
       [ "${events% done}" != "$events" ]; then
        pass "leader-instant-exit: reaped XOR admitted-and-cleaned"
    else
        fail "leader-instant-exit: reaped XOR admitted-and-cleaned" \
            "status=$status events=[$events] recorded=$recorded leaked=$leaked outcomes=$outcomes"
    fi
}

run_extra_cases() {
    run_interrupt_journal
    run_alias_refusal
    run_journal_dir_alias_refusal
    run_journal_failure
    run_json_escape_check
    run_case_folded_alias_refusal
    run_detached_orphan_reap detached_leader_exits detached \
        "setsid-detached leader-exit: reaped or admitted"
    run_instant_exit_discrimination
}
