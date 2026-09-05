#!/usr/bin/env python3
"""Hermetic laws for PTY probe text parsing and the profile guard."""

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


if __name__ == "__main__":
    unittest.main()
