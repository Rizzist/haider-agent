#!/usr/bin/env python3
"""Focused regression tests for the optional M1 region snapshot path."""

from __future__ import annotations

import importlib.util
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock


SAMPLER_PATH = Path(__file__).with_name("m1-rss-sampler.py")
SPEC = importlib.util.spec_from_file_location("m1_rss_sampler", SAMPLER_PATH)
assert SPEC is not None and SPEC.loader is not None
SAMPLER = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = SAMPLER
SPEC.loader.exec_module(SAMPLER)


def valid_snapshot(pid: int) -> str:
    return (
        f"# pid={pid} capture_wall_ns=123 rss_bytes=16 footprint_bytes=16 "
        "user_cpu_ns=1 system_cpu_ns=2\n"
        f"{SAMPLER.REGION_SNAPSHOT_HEADER}\n"
        "0x0000000000001000\t16384\t16384\t16384\t0\t16384\t3\t3\t1\t2\t0\t\n"
    )


class RegionSnapshotTests(unittest.TestCase):
    def test_validator_requires_schema_rows_and_exact_pid(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "regions.tsv"
            output.write_text(valid_snapshot(42), encoding="utf-8")
            self.assertIsNone(SAMPLER.validate_region_snapshot(output, 42))
            self.assertIn("pid=7", SAMPLER.validate_region_snapshot(output, 7))
            output.write_text(valid_snapshot(42) + "truncated\n", encoding="utf-8")
            self.assertIn("row 2", SAMPLER.validate_region_snapshot(output, 42))
            output.write_text("", encoding="utf-8")
            self.assertIn("lacks", SAMPLER.validate_region_snapshot(output, 42))

    def test_exit_zero_empty_helper_is_rejected_and_removed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "regions.tmp"
            error = SAMPLER.capture_region_snapshot(Path("/usr/bin/true"), 42, output)
            self.assertIn("lacks", error)
            self.assertFalse(output.exists())

    def test_nonzero_and_missing_helpers_are_bounded_failures(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "regions.tmp"
            error = SAMPLER.capture_region_snapshot(Path("/usr/bin/false"), 42, output)
            self.assertIn("status", error)
            self.assertFalse(output.exists())
            error = SAMPLER.capture_region_snapshot(
                Path(directory) / "missing-helper", 42, output
            )
            self.assertIn("execute", error)
            self.assertFalse(output.exists())

    def test_timeout_is_caught_and_partial_output_removed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / "regions.tmp"
            with mock.patch.object(
                SAMPLER.subprocess,
                "run",
                side_effect=subprocess.TimeoutExpired(["snapshot"], 5),
            ):
                error = SAMPLER.capture_region_snapshot(Path("snapshot"), 42, output)
            self.assertIn("execute", error)
            self.assertFalse(output.exists())


if __name__ == "__main__":
    unittest.main()
