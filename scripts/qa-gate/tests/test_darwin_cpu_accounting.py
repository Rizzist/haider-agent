from __future__ import annotations

import os
from pathlib import Path
import subprocess
import sys
import unittest
from unittest import mock

import turn_wall_harness as harness
import turnperf_support as support


class CpuAccountingTests(unittest.TestCase):
    def test_ticks_to_nanoseconds_uses_supplied_ratio_and_integer_precision(self):
        for ticks, numer, denom, expected in (
            (0, 7, 3, 0), (9, 7, 3, 21), (10, 7, 3, 23),
            (24_000_000, 125, 3, 1_000_000_000),
            (17, 1, 1, 17), (2**63, 3, 2, 3 * 2**62),
        ):
            with self.subTest(ticks=ticks, timebase=(numer, denom)):
                self.assertEqual(support.mach_ticks_to_ns(ticks, numer, denom), expected)

    def test_invalid_ticks_or_timebase_are_rejected(self):
        for values in ((-1, 1, 1), (1, 0, 1), (1, 1, 0), (1, -1, 1)):
            with self.subTest(values=values), self.assertRaises(ValueError):
                support.mach_ticks_to_ns(*values)

    def test_native_self_and_reaped_children_use_timebase_rss_stays_bytes(self):
        info = support._DarwinRusageInfoV4()
        info.ri_user_time, info.ri_system_time = 6_000_000, 3_000_000
        info.ri_child_user_time, info.ri_child_system_time = 12_000_000, 6_000_000
        info.ri_resident_size = 8 * 1_024 * 1_024
        with mock.patch.object(support.sys, "platform", "darwin"), \
                mock.patch.object(support, "_darwin_rusage", return_value=info), \
                mock.patch.object(support, "darwin_timebase", return_value=(7, 3)):
            self.assertEqual(support._process_usage(123), (21.0, 8_192, 8_192))
            self.assertEqual(support.process_cpu_times(123), support.ProcessCpuTimes(21, 42))

    def test_timebase_is_read_and_cached_and_invalid_native_values_fail(self):
        library = mock.Mock()

        def fill(pointer):
            pointer._obj.numer, pointer._obj.denom = 7, 3
            return 0

        library.mach_timebase_info.side_effect = fill
        support.darwin_timebase.cache_clear()
        try:
            with mock.patch.object(support.ctypes, "CDLL", return_value=library):
                self.assertEqual(support.darwin_timebase(), (7, 3))
                self.assertEqual(support.darwin_timebase(), (7, 3))
                self.assertEqual(library.mach_timebase_info.call_count, 1)
                for status, numer, denom in ((1, 1, 1), (0, 0, 1), (0, 1, 0)):
                    def invalid(pointer):
                        pointer._obj.numer, pointer._obj.denom = numer, denom
                        return status
                    support.darwin_timebase.cache_clear()
                    library.mach_timebase_info.side_effect = invalid
                    with self.assertRaisesRegex(support.ProofError, "timebase unavailable"):
                        support.darwin_timebase()
        finally:
            support.darwin_timebase.cache_clear()

    def test_unavailable_counters_cannot_become_zero_cpu(self):
        with mock.patch.object(support.sys, "platform", "darwin"), \
                mock.patch.object(support, "_darwin_rusage", side_effect=OSError("gone")):
            self.assertIsNone(support._process_usage(123))
            with self.assertRaisesRegex(support.ProofError, "CPU accounting unavailable"):
                support.process_cpu_ms(123)

    def test_missing_timebase_fails_loudly_instead_of_reporting_false_cpu(self):
        with mock.patch.object(support.sys, "platform", "darwin"), \
                mock.patch.object(support, "_darwin_rusage", return_value=support._DarwinRusageInfoV4()), \
                mock.patch.object(support, "darwin_timebase", side_effect=support.ProofError("timebase unavailable")):
            with self.assertRaisesRegex(support.ProofError, "timebase unavailable"):
                support._process_usage(123)

    def test_deltas_keep_child_cpu_separate_and_reject_counter_regression(self):
        before = support.ProcessCpuTimes(100, 200)
        self.assertEqual(support.ProcessCpuTimes(112, 207).delta(before), support.ProcessCpuTimes(12, 7))
        for after in (support.ProcessCpuTimes(99, 210), support.ProcessCpuTimes(110, 199)):
            with self.assertRaisesRegex(support.ProofError, "regressed"):
                after.delta(before)

    def test_linux_stat_child_units_and_comm_with_spaces_and_parentheses(self):
        fields = ["S"] + ["0"] * 10 + ["200", "100", "50", "25"]
        with mock.patch.object(support.sys, "platform", "linux"), \
                mock.patch.object(support.Path, "read_text", return_value="123 (a ) weird name) " + " ".join(fields)), \
                mock.patch.object(support.os, "sysconf", return_value=100):
            self.assertEqual(support.process_cpu_times(123), support.ProcessCpuTimes(3000, 750))

    def test_self_check_catches_tick_as_ns_mutation_even_on_unit_timebase_hosts(self):
        with mock.patch.object(support, "mach_ticks_to_ns", side_effect=lambda ticks, numer, denom: ticks):
            with self.assertRaisesRegex(support.ProofError, "ticks treated as nanoseconds"):
                support.cpu_accounting_self_check()

    def test_both_harnesses_run_self_check_before_creating_profiles(self):
        for run in (harness.run_harness, harness.run_one_shot_harness):
            with self.subTest(run=run.__name__), \
                    mock.patch.object(harness, "cpu_accounting_self_check", side_effect=support.ProofError("bad CPU units")), \
                    mock.patch.object(harness, "ThrowawayProfile") as profile:
                with self.assertRaisesRegex(support.ProofError, "bad CPU units"):
                    run(Path("/unused"))
                profile.assert_not_called()

    @unittest.skipUnless(sys.platform == "darwin", "Darwin native accounting")
    def test_live_self_check_agrees_with_getrusage_seconds(self):
        evidence = support.cpu_accounting_self_check()
        self.assertEqual(evidence["synthetic_timebase_check"], "passed")
        self.assertGreater(evidence["native_self_ms"], 0)

    @unittest.skipUnless(sys.platform == "darwin", "Darwin native child accounting")
    def test_live_reaped_child_matches_wait4_without_scaling_wait4(self):
        before = support.process_cpu_times(os.getpid())
        child = subprocess.Popen([sys.executable, "-c", "sum(i*i for i in range(400000))"])
        try:
            _, status, usage = os.wait4(child.pid, 0)
            child.returncode = os.waitstatus_to_exitcode(status)
            self.assertEqual(child.returncode, 0)
            delta = support.process_cpu_times(os.getpid()).delta(before)
            expected_ms = (usage.ru_utime + usage.ru_stime) * 1_000
            self.assertGreater(expected_ms, 5)
            self.assertAlmostEqual(delta.reaped_children_ms, expected_ms, delta=2)
        finally:
            child.wait()


if __name__ == "__main__":
    unittest.main()
