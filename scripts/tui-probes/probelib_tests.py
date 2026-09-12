#!/usr/bin/env python3
"""Hermetic laws for PTY probe text parsing and the profile guard."""

import contextlib
import io
import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import probelib


class PlainTextTests(unittest.TestCase):
    def test_attach_notification_preserves_composer_before_later_bel(self):
        captured = (
            b"\x1b]7791;attached=session-example\x1b\\"
            b"\x1b[32;1H\x1b[2mmessage haider\x1b[0m"
            b"\x1b]9;turn done\x07"
        )
        self.assertEqual(probelib.plain(captured), b"message haider")

    def test_each_osc_terminator_bounds_only_its_own_payload(self):
        for terminator in (b"\x07", b"\x1b\\"):
            with self.subTest(terminator=terminator):
                captured = (
                    b"before\x1b]title\ncontinued"
                    + terminator
                    + b"between\x1b]other"
                    + terminator
                    + b"after"
                )
                self.assertEqual(probelib.plain(captured), b"beforebetweenafter")


class ThrowawayProfileTests(unittest.TestCase):
    def test_non_throwaway_profile_is_rejected(self):
        with tempfile.TemporaryDirectory(prefix="ordinary-profile-") as profile:
            with self.assertRaisesRegex(RuntimeError, "refused non-throwaway"):
                probelib.require_throwaway_profile(profile)

    def test_named_throwaway_profile_and_descendant_are_accepted(self):
        with tempfile.TemporaryDirectory(prefix="haider-live-probe-") as profile:
            descendant = os.path.join(profile, "profile")
            self.assertEqual(
                probelib.require_throwaway_profile(descendant),
                os.path.realpath(descendant),
            )


class VerdictTests(unittest.TestCase):
    def run_verdict(self, child_clean, checks):
        output = io.StringIO()
        with contextlib.redirect_stdout(output), self.assertRaises(SystemExit) as raised:
            probelib.verdict(
                "probe", b"\x1b[?1049h\x1b[?1049l", child_clean, checks
            )
        return raised.exception.code, output.getvalue().splitlines()

    def test_early_failures_remain_visible_in_ladder_tail(self):
        code, lines = self.run_verdict(
            False,
            [("session surface", False)] + [(f"later {i}", True) for i in range(30)],
        )
        self.assertEqual(code, 1)
        self.assertNotIn("session surface = False", lines[-25:])
        self.assertIn(
            "probe failed checks: child_exited_cleanly; session surface", lines[-25:]
        )
        self.assertEqual(lines[-1], "probe = FAIL")

    def test_passing_checks_and_explicit_skip_keep_success_exit(self):
        code, lines = self.run_verdict(True, [("present", True), ("tiny", "SKIP")])
        self.assertEqual(code, 0)
        self.assertIn("tiny = SKIP (by design at this size)", lines)
        self.assertFalse(any("failed checks:" in line for line in lines))
        self.assertEqual(lines[-1], "probe = PASS")


if __name__ == "__main__":
    unittest.main()
