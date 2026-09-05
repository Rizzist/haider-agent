#!/usr/bin/env bash
# No network/token required: every gh invocation must match the stub contract.
set -euo pipefail
root=$(cd "$(dirname "$0")/../../.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/bin" "$tmp/fixtures"
export GH_STUB_ROOT="$tmp"
export GH_STUB_SHA=0123456789abcdef0123456789abcdef01234567
export GITHUB_SHA="$GH_STUB_SHA" GITHUB_REF_NAME=v0.0.970 GITHUB_REPOSITORY=owner/repo
export REQUIRE_EVIDENCE_ATTEMPTS=4 REQUIRE_EVIDENCE_INTERVAL=0
export PATH="$tmp/bin:$PATH"

for conclusion in success failure cancelled timed_out skipped neutral action_required; do
  printf '[{"workflow_runs":[{"id":1,"head_sha":"%s","status":"completed","conclusion":"%s"}]}]\n' \
    "$GH_STUB_SHA" "$conclusion" > "$tmp/fixtures/$conclusion.json"
done
for status in queued in_progress waiting requested pending; do
  printf '[{"workflow_runs":[{"id":1,"head_sha":"%s","status":"%s","conclusion":null}]}]\n' \
    "$GH_STUB_SHA" "$status" > "$tmp/fixtures/$status.json"
done
printf '[{"workflow_runs":[]}]\n' > "$tmp/fixtures/missing.json"
printf '{"message":"bad response"}\n' > "$tmp/fixtures/invalid.json"
printf '[{"workflow_runs":[{"head_sha":"ffffffffffffffffffffffffffffffffffffffff","status":"completed","conclusion":"success"}]}]\n' > "$tmp/fixtures/wrong_sha.json"
# Completed+success must both hold. Success on a later API page still counts.
jq '.[0].workflow_runs[0].status = "in_progress"' "$tmp/fixtures/success.json" > "$tmp/fixtures/nonterminal_success.json"
jq -s 'add' "$tmp/fixtures/failure.json" "$tmp/fixtures/success.json" > "$tmp/fixtures/older_success.json"

cat > "$tmp/bin/gh" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >> "$GH_STUB_ROOT/calls"
case "$*" in
  'api --method GET --paginate --slurp repos/owner/repo/actions/workflows/'*"/runs?head_sha=$GH_STUB_SHA&per_page=100")
    endpoint="${!#}"
    file="${endpoint#*/workflows/}"
    file="${file%%/*}"
    count_file="$GH_STUB_ROOT/count-$file"
    count=0
    [[ ! -f "$count_file" ]] || read -r count < "$count_file"
    count=$((count + 1))
    printf '%s\n' "$count" > "$count_file"
    fixture=success
    if [[ "$file" == "$GH_STUB_WORKFLOW" ]]; then
      case "$GH_STUB_CASE" in
        success|failure|cancelled|timed_out|skipped|neutral|action_required|invalid|older_success) fixture="$GH_STUB_CASE" ;;
        api_error) exit 1 ;;
        pending_api_error) (( count == 1 )) || exit 1; fixture=in_progress ;;
        progress_success) if (( count == 1 )); then fixture=in_progress; fi ;;
        progress_failure) if (( count == 1 )); then fixture=in_progress; else fixture=failure; fi ;;
        queued|waiting|requested|pending) if (( count == 1 )); then fixture="$GH_STUB_CASE"; fi ;;
        nonterminal_success) if (( count == 1 )); then fixture=nonterminal_success; else fixture=failure; fi ;;
        timeout) fixture=in_progress ;;
        missing_timeout|dispatch_forbidden|ref_mismatch|ref_api_error) fixture=missing ;;
        missing_dispatch)
          case "$count" in 1|2) fixture=missing ;; 3) fixture=queued ;; esac ;;
        wrong_sha) if (( count == 1 )); then fixture=wrong_sha; fi ;;
        *) exit 99 ;;
      esac
    fi
    cat "$GH_STUB_ROOT/fixtures/$fixture.json"
    ;;
  "api repos/owner/repo/commits/$GITHUB_REF_NAME --jq .sha")
    case "$GH_STUB_CASE" in
      ref_api_error) exit 1 ;;
      ref_mismatch) printf '%040d\n' 0 ;;
      *) printf '%s\n' "$GH_STUB_SHA" ;;
    esac
    ;;
  "workflow run $GH_STUB_WORKFLOW --repo owner/repo --ref $GITHUB_REF_NAME")
    printf 'dispatch\n' >> "$GH_STUB_ROOT/dispatches"
    [[ "$GH_STUB_CASE" != dispatch_forbidden ]]
    ;;
  *) printf 'unexpected gh command: %s\n' "$*" >&2; exit 99 ;;
esac
STUB
chmod +x "$tmp/bin/gh"

count_tests=0
entrypoint="$root/scripts/release/require-evidence.sh"
run_case() {
  export GH_STUB_CASE="$1"
  local expected_code="$2" reason="$3" expected_polls="$4" expected_dispatches="$5"
  local output code=0 expected name dispatches=0 polls
  rm -f "$tmp"/count-* "$tmp/calls" "$tmp/dispatches"
  output=$(bash "$entrypoint" 2>&1) || code=$?
  name=xplat-check
  [[ "$GH_STUB_WORKFLOW" != ci.yml ]] || name=ci
  if (( expected_code == 0 )); then
    expected=$(printf 'evidence: PASS xplat-check %s: completed success\nevidence: PASS ci %s: completed success' "$GH_STUB_SHA" "$GH_STUB_SHA")
  else
    expected="evidence: FAIL $name $GH_STUB_SHA: $reason"
    if [[ "$name" == ci ]]; then
      expected="$(printf 'evidence: PASS xplat-check %s: completed success\n' "$GH_STUB_SHA")"$'\n'"$expected"
    fi
  fi
  read -r polls < "$tmp/count-$GH_STUB_WORKFLOW"
  [[ ! -f "$tmp/dispatches" ]] || dispatches=$(wc -l < "$tmp/dispatches")
  if [[ "$code" != "$expected_code" || "$output" != "$expected" || "$polls" != "$expected_polls" ]] || (( dispatches != expected_dispatches )); then
    printf 'FAIL %s/%s: code=%s polls=%s dispatches=%s\nexpected: %s\nactual: %s\n' \
      "$name" "$1" "$code" "$polls" "$dispatches" "$expected" "$output" >&2
    cat "$tmp/calls" >&2
    exit 1
  fi
  count_tests=$((count_tests + 1))
  printf 'ok %s/%s\n' "$name" "$1"
}

for GH_STUB_WORKFLOW in xplat.yml ci.yml; do
  export GH_STUB_WORKFLOW
  run_case success 0 '' 1 0
  run_case older_success 0 '' 1 0
  run_case progress_success 0 '' 2 0
  run_case progress_failure 1 'conclusion=failure status=completed' 2 0
  for status in queued waiting requested pending; do run_case "$status" 0 '' 2 0; done
  for conclusion in failure cancelled timed_out skipped neutral action_required; do
    run_case "$conclusion" 1 "conclusion=$conclusion status=completed" 1 0
  done
  run_case nonterminal_success 1 'conclusion=failure status=completed' 2 0
  run_case missing_dispatch 0 '' 4 1
  run_case wrong_sha 0 '' 2 1
  run_case dispatch_forbidden 1 'dispatch failed (forbidden or API error)' 1 1
  run_case api_error 1 'API error listing runs' 1 0
  run_case pending_api_error 1 'API error listing runs' 2 0
  run_case invalid 1 'API error: invalid run response' 1 0
  run_case ref_mismatch 1 'dispatch ref does not resolve to candidate SHA' 1 0
  run_case ref_api_error 1 'API error resolving dispatch ref' 1 0
  run_case timeout 1 'poll timeout (last state: pending)' 4 0
  run_case missing_timeout 1 'poll timeout (last state: missing)' 4 1
done
# Push-to-main and the local pregate use the same policy and candidate SHA.
export GITHUB_REF_NAME=main
run_case missing_dispatch 0 '' 4 1
cat > "$tmp/bin/git" <<'STUB'
#!/usr/bin/env bash
set -euo pipefail
case "$*" in
  'symbolic-ref --quiet --short HEAD') printf 'main\n' ;;
  'rev-parse --verify HEAD^{commit}') printf '%s\n' "$GH_STUB_SHA" ;;
  *) exit 99 ;;
esac
STUB
chmod +x "$tmp/bin/git"
entrypoint="$root/scripts/ship-970.sh"
# Deliberately wrong ambient SHA: the wrapper must resolve HEAD and pass it.
export GITHUB_SHA=ffffffffffffffffffffffffffffffffffffffff
run_case missing_dispatch 0 '' 4 1
run_case failure 1 'conclusion=failure status=completed' 1 0
printf 'PASS: %s release evidence policy tests\n' "$count_tests"
