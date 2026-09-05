#!/usr/bin/env bash
# Usage: require-evidence.sh [commit-sha [dispatch-ref [owner/repo]]]
# workflow_call preserves the caller's GITHUB_SHA and GITHUB_REF_NAME.
set -euo pipefail

sha="${1:-${GITHUB_SHA:-}}"
ref="${2:-${GITHUB_REF_NAME:-}}"
repo="${3:-${GITHUB_REPOSITORY:-}}"
workflow='xplat-check+ci'
fail() {
  printf 'evidence: FAIL %s %s: %s\n' "$workflow" "${sha:-<missing-sha>}" "$1" >&2
  exit 1
}

[[ "$sha" =~ ^[0-9a-f]{40}$ ]] || fail 'expected a full commit SHA'
[[ "$repo" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || fail 'expected owner/repo'
command -v gh >/dev/null || fail 'gh is unavailable'
command -v jq >/dev/null || fail 'jq is unavailable'
# Two workflows each get at most 120 polls, 60 s apart (<= 240 min total).
# Overrides only shorten this budget, allowing deterministic, zero-sleep tests.
attempts="${REQUIRE_EVIDENCE_ATTEMPTS:-120}"
interval="${REQUIRE_EVIDENCE_INTERVAL:-60}"
[[ "$attempts" =~ ^[1-9][0-9]{0,2}$ ]] && (( attempts <= 120 )) || fail 'invalid poll count'
[[ "$interval" =~ ^(0|[1-9][0-9]?)$ ]] && (( interval <= 60 )) || fail 'invalid poll interval'

for file in xplat.yml ci.yml; do
  case "$file" in xplat.yml) workflow=xplat-check ;; ci.yml) workflow=ci ;; esac
  dispatched=false
  for ((poll = 1; poll <= attempts; poll++)); do
    # Query by workflow file, not a similarly named check/job. Pagination keeps
    # an older success for this exact SHA visible. Never filter by branch/event.
    if ! runs=$(gh api --method GET --paginate --slurp \
      "repos/$repo/actions/workflows/$file/runs?head_sha=$sha&per_page=100" 2>/dev/null); then
      fail 'API error listing runs'
    fi
    if ! state=$(jq -er --arg sha "$sha" '
      if type != "array" or length == 0 then error("invalid pages") else . end
      | if all(.[]; (.workflow_runs | type) == "array") then . else error("invalid runs") end
      | [.[].workflow_runs[] | select(.head_sha == $sha)]
      | if any(.[]; .status == "completed" and .conclusion == "success") then "success"
        elif length == 0 then "missing"
        elif any(.[]; .status == "in_progress" or .status == "queued" or
                       .status == "requested" or .status == "waiting" or .status == "pending") then "pending"
        else (sort_by(.id) | last | "conclusion=\(.conclusion // "unknown") status=\(.status // "unknown")")
        end
    ' <<< "$runs" 2>/dev/null); then
      fail 'API error: invalid run response'
    fi
    case "$state" in
      success)
        printf 'evidence: PASS %s %s: completed success\n' "$workflow" "$sha"
        break
        ;;
      missing)
        if [[ "$dispatched" == false ]]; then
          [[ -n "$ref" ]] || fail 'missing dispatch ref'
          # A moving branch must not silently test a different candidate.
          encoded_ref=$(jq -rn --arg ref "$ref" '$ref | @uri')
          if ! ref_sha=$(gh api "repos/$repo/commits/$encoded_ref" --jq .sha 2>/dev/null); then
            fail 'API error resolving dispatch ref'
          fi
          [[ "$ref_sha" == "$sha" ]] || fail 'dispatch ref does not resolve to candidate SHA'
          if ! gh workflow run "$file" --repo "$repo" --ref "$ref" >/dev/null 2>&1; then
            fail 'dispatch failed (forbidden or API error)'
          fi
          dispatched=true
        fi
        ;;
      pending) ;;
      *) fail "$state" ;;
    esac
    (( poll < attempts )) || fail "poll timeout (last state: $state)"
    sleep "$interval" || fail 'poll wait failed'
  done
done
