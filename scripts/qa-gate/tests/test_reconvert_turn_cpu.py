from __future__ import annotations

import copy
import unittest

import reconvert_turn_cpu as offline
from turnperf_support import ProofError


def report():
    rows = [{"index": i, "case_id": i, "client_cpu_ms": client, "daemon_cpu_ms": daemon,
             "combined_cpu_ms": client + daemon, "client_sampled_cpu_lower_bound_ms": 0.5}
            for i, (client, daemon) in enumerate(((1, 100), (100, 1), (2, 3)))]
    return {"schema": "haider.turn-wall.v1", "host": {"platform": "macOS-test"},
            "binaries": {"proxy_source_sha256": offline.LEGACY_SUPPORT_SHA256},
            "samples": {"single": rows, "tool": rows}}


class OfflineCpuTests(unittest.TestCase):
    def test_scale_daemon_only_then_take_median_of_sample_sums(self):
        original = report()
        snapshot = copy.deepcopy(original)
        result = offline.reconvert(original, 2, 1)["shapes"]["tool"]
        self.assertEqual(result["client_plus_daemon_self_cpu_ms"], {"median": 102, "mad": 94, "total": 311})
        self.assertEqual(result["client_cpu_ms"]["total"], 103)
        self.assertEqual(result["samples"][0]["client_sampled_cpu_lower_bound_ms"], 1)
        self.assertIsNone(result["samples"][0]["daemon_reaped_children_cpu_ms"])
        self.assertEqual(original, snapshot)

    def test_refuse_lifecycle_unknown_method_corrected_or_non_darwin_reports(self):
        for patch in ({"mode": "one-shot"}, {"cpu_accounting": {"version": 2}},
                      {"binaries": {}}, {"host": {"platform": "Linux"}}, {"schema": "other"}):
            with self.subTest(patch=patch), self.assertRaises(ProofError):
                offline.reconvert({**report(), **patch}, 7, 3)

    def test_refuse_invalid_ratio_missing_samples_or_inconsistent_total(self):
        for numer, denom in ((0, 1), (1, 0), (-1, 1)):
            with self.assertRaises(ProofError):
                offline.reconvert(report(), numer, denom)
        document = report()
        document["samples"]["single"][0]["combined_cpu_ms"] = 999
        with self.assertRaises(ProofError):
            offline.reconvert(document, 7, 3)
        document["samples"] = {}
        with self.assertRaises(ProofError):
            offline.reconvert(document, 7, 3)


if __name__ == "__main__":
    unittest.main()
