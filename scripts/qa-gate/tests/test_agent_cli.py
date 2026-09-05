"""Mutation pins for the public delegation check, independent of Rust builds."""

from __future__ import annotations

import copy
import json
from pathlib import Path
from types import SimpleNamespace
import unittest

from gate.contract import FAIL, PASS
from gate.loader import load_check


class AgentCliCheckTests(unittest.TestCase):
    def setUp(self):
        self.check = load_check(
            Path(__file__).resolve().parents[1] / "checks/t0/t0.agent.spawn_result.py", "t0"
        )
        self.spawn = {
            "schema": "haider.agent.spawn.v1", "ok": True, "error": None,
            "result": {
                "session_id": "parent-session", "run_id": "parent-run", "agent_id": "child-agent",
                "child_session_id": "child-session", "child_run_id": "child-run",
            },
        }
        self.wait = {
            "schema": "haider.agent.wait.v1", "ok": True, "error": None,
            "result": {
                "session_id": "parent-session", "agent_id": "child-agent",
                "child_session_id": "child-session", "child_run_id": "child-run",
                "state": "done", "terminal_seq": 27, "child_result_seq": 13,
                "report_source": "child_result",
                "report": {"agent": "child-agent", "summary": self.check.module.SENTINEL,
                           "verified": "unverified"},
            },
        }

    def evaluate(self, spawn=None, wait=None):
        documents = [spawn or self.spawn, wait or self.wait]
        commands = []

        def run_haider(args, *, timeout, env_overrides=None):
            commands.append((args, timeout, env_overrides))
            return SimpleNamespace(
                stdout=json.dumps(documents.pop(0)) + "\n", stderr="", returncode=0, timed_out=False
            )

        context = SimpleNamespace(
            run_haider=run_haider,
            command_artefact=lambda label, _command: f"retained/{label}.json",
        )
        return self.check.run(context), commands

    def test_round_trip_consumes_one_segment_and_uses_derived_bounds(self):
        evidence, commands = self.evaluate()
        self.assertEqual(evidence[0].status, PASS)
        self.assertEqual(self.check.segments, 1)
        self.assertEqual(self.check.turns_expected, 1)
        self.assertEqual(self.check.budget.milliseconds, 288_000)
        self.assertEqual(len(commands), 2)
        self.assertEqual(commands[0][0][:2], ["agent", "spawn"])
        self.assertEqual(commands[0][2], {"HAIDER_TOOL_EXPOSURE": "spawn_subagent"})
        self.assertEqual(commands[1][0][:4], ["agent", "wait", "parent-session", "child-agent"])
        self.assertIsNone(commands[1][2])
        self.assertIn("--no-spawn", commands[1][0])
        self.assertTrue(all(timeout.seconds == 102 for _, timeout, _ in commands))

    def test_result_without_durable_anchors_or_nonce_cannot_pass(self):
        mutations = [
            ("child_result_seq", None), ("terminal_seq", None), ("terminal_seq", True),
            ("state", "running"), ("child_run_id", "stale-child-run"),
            ("report_source", "child_journal"),
            ("report", {"agent": "child-agent", "summary": "invented report"}),
            ("report", {"agent": "another-agent", "summary": self.check.module.SENTINEL}),
        ]
        for field, value in mutations:
            with self.subTest(field=field, value=value):
                changed = copy.deepcopy(self.wait)
                changed["result"][field] = value
                evidence, _commands = self.evaluate(wait=changed)
                self.assertEqual(evidence[0].status, FAIL)
                self.assertEqual(len(evidence[0].artefacts), 2)

    def test_failed_spawn_contract_stops_before_wait(self):
        for field, value in (("schema", "haider.run.v1"), ("ok", False)):
            with self.subTest(field=field):
                changed = copy.deepcopy(self.spawn)
                changed[field] = value
                evidence, commands = self.evaluate(spawn=changed)
                self.assertEqual(evidence[0].status, FAIL)
                self.assertEqual(len(commands), 1)


if __name__ == "__main__":
    unittest.main()
