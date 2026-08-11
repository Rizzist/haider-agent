#!/usr/bin/env python3
"""Hermetic laws for the PTY probe profile guard."""

import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import probelib


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
