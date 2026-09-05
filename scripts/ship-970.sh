#!/usr/bin/env bash
# Local evidence pregate; run successfully before creating the release tag.
# Usage: scripts/ship-970.sh [published-candidate-ref [candidate-sha]]
set -euo pipefail
cd "$(dirname "$0")/.."
candidate_ref="${1:-$(git symbolic-ref --quiet --short HEAD)}"
candidate_sha=$(git rev-parse --verify "${2:-HEAD}^{commit}")
repo="${GITHUB_REPOSITORY:-$(gh repo view --json nameWithOwner --jq .nameWithOwner)}"
exec bash scripts/release/require-evidence.sh "$candidate_sha" "$candidate_ref" "$repo"
