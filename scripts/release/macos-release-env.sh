#!/usr/bin/env bash
# Source for the shared producer, release compiler and manifest verification.
# Keep release-only settings here; the render benchmark retains its own profile.
export CARGO_INCREMENTAL=0
# 970 measured experiment: revert this commit unless CI budgets pass and each
# unsigned runtime binary grows <10% versus the same-source fat-LTO baseline.
export CARGO_PROFILE_RELEASE_LTO=thin
export CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16
