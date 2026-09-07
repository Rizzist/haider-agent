#!/usr/bin/env python3
"""Read-only, exact-SHA workflow evidence lookup; API errors fail closed.

Unlike require-evidence.sh this never dispatches or waits. A well-formed response
with no completed success is a cache miss, not permission to release unchecked.
"""

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import sys


def api(endpoint):
    result = subprocess.run(
        ["gh", "api", "--method", "GET", "--paginate", "--slurp", endpoint],
        check=True, capture_output=True, text=True,
    )
    return json.loads(result.stdout)


def rows(pages, key):
    if not isinstance(pages, list) or not pages:
        raise ValueError("invalid API pages")
    result = []
    for page in pages:
        if not isinstance(page, dict) or not isinstance(page.get(key), list):
            raise ValueError(f"invalid API {key}")
        for row in page[key]:
            if not isinstance(row, dict):
                raise ValueError(f"invalid API {key} entry")
            result.append(row)
    return result


def successful_runs(pages, sha, exclude_run_id):
    matches = []
    for run in rows(pages, "workflow_runs"):
        if (run.get("head_sha") == sha and run.get("status") == "completed"
                and run.get("conclusion") == "success"
                and str(run.get("id")) != exclude_run_id):
            if type(run.get("id")) is not int or run["id"] <= 0:
                raise ValueError("invalid successful run ID")
            matches.append(run)
    return sorted(matches, key=lambda run: run["id"], reverse=True)


def emit(outputs):
    text = "".join(f"{key}={value}\n" for key, value in outputs.items())
    print(text, end="")
    if os.environ.get("GITHUB_OUTPUT"):
        with Path(os.environ["GITHUB_OUTPUT"]).open("a", encoding="utf-8") as output:
            output.write(text)


def artifact_exists(repo, run_id, name, sha):
    # A release's reusable ship-gate uploads into the *caller* run, which is
    # still in progress. This lookup proves artifact provenance, not a verdict.
    pages = api(f"repos/{repo}/actions/runs/{run_id}")
    if (not isinstance(pages, list) or len(pages) != 1
            or not isinstance(pages[0], dict) or pages[0].get("head_sha") != sha
            or pages[0].get("id") != int(run_id)):
        raise ValueError("artifact run does not match candidate")
    artifacts = rows(api(f"repos/{repo}/actions/runs/{run_id}/artifacts?per_page=100"), "artifacts")
    return any(artifact.get("name") == name and artifact.get("expired") is False
               for artifact in artifacts)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("workflow", choices=("ci.yml", "xplat.yml", "ship-gate.yml"))
    parser.add_argument("sha")
    parser.add_argument("--repo", default=os.environ.get("GITHUB_REPOSITORY", ""))
    parser.add_argument("--exclude-run-id", default=os.environ.get("GITHUB_RUN_ID", ""))
    parser.add_argument("--artifact", help="also resolve an unexpired build artifact")
    parser.add_argument("--artifact-run-id", help="look only in this caller run; does not satisfy a gate")
    args = parser.parse_args()
    if not re.fullmatch(r"[0-9a-f]{40}", args.sha):
        parser.error("expected a full commit SHA")
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", args.repo):
        parser.error("expected owner/repo")
    if args.exclude_run_id and not re.fullmatch(r"[1-9][0-9]*", args.exclude_run_id):
        parser.error("invalid excluded run ID")
    if args.artifact_run_id and (not args.artifact or not re.fullmatch(r"[1-9][0-9]*", args.artifact_run_id)):
        parser.error("artifact run ID requires an artifact name and positive integer")
    try:
        if args.artifact_run_id:
            found = artifact_exists(args.repo, args.artifact_run_id, args.artifact, args.sha)
            emit({"artifact_run_id": args.artifact_run_id if found else ""})
            return 0
        # No branch/event filter: wave, main and dispatch evidence are equivalent.
        pages = api(f"repos/{args.repo}/actions/workflows/{args.workflow}/runs"
                    f"?head_sha={args.sha}&per_page=100")
        runs = successful_runs(pages, args.sha, args.exclude_run_id)
        outputs = {"satisfied": "true" if runs else "false",
                   "run_id": str(runs[0]["id"]) if runs else ""}
        if args.artifact:
            outputs["artifact_run_id"] = ""
            for run in runs:
                if artifact_exists(args.repo, run["id"], args.artifact, args.sha):
                    outputs["artifact_run_id"] = str(run["id"])
                    break
        emit(outputs)
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        # Do not print gh stderr (which can include account details).
        print(f"evidence lookup failed: {type(error).__name__}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
