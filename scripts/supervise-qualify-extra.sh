#!/bin/bash
#
# Focused regression cases supplementing the four original qualification
# cases. This file is sourced after the shared fake and watchdog are ready.

cleanup_fake_pids() {
    local file
    for file in "$TMP_ROOT"/*.pids; do
        [ -f "$file" ] || continue
        signal_pid_file TERM "$file"
    done
    sleep 1
    for file in "$TMP_ROOT"/*.pids; do
        [ -f "$file" ] || continue
        signal_pid_file KILL "$file"
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

run_extra_cases() {
    run_interrupt_journal
    run_alias_refusal
    run_journal_failure
    run_json_escape_check
}
