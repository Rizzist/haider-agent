from __future__ import annotations

import json
from pathlib import Path
from types import SimpleNamespace
import unittest

from turnperf_support import validate_jsonl

from deep_turn_harness import (
    CHECKPOINTS,
    MODEL_ID,
    PromptCacheOracle,
    PROVIDER_COUNT,
    PROVIDER_ID,
    SPILL_TURNS,
    TOOL_TURNS,
    _continuation_arguments,
    _initial_arguments,
    abba_order,
    arms_for,
    fragment_bytes,
    parse_daemon_processes,
    parse_store_records,
    process_command,
    provider_catalog,
    self_check,
    tool_response,
    turn_is_spill,
    turn_is_tool,
)


class DeepTurnHarnessTests(unittest.TestCase):
    def test_cache_oracle_requires_both_key_and_exact_prefix(self) -> None:
        oracle = PromptCacheOracle()
        shared = "shared-system-" + ("x" * 5_000)
        tools = [{"type": "function", "function": {"name": "read"}}]

        first = oracle.observe(
            {
                "prompt_cache_key": "account-a",
                "tools": tools,
                "messages": [
                    {"role": "system", "content": shared},
                    {"role": "user", "content": "first"},
                ],
            }
        )
        same_partition = oracle.observe(
            {
                "prompt_cache_key": "account-a",
                "tools": tools,
                "messages": [
                    {"role": "system", "content": shared},
                    {"role": "user", "content": "second"},
                ],
            }
        )
        other_partition = oracle.observe(
            {
                "prompt_cache_key": "account-b",
                "tools": tools,
                "messages": [
                    {"role": "system", "content": shared},
                    {"role": "user", "content": "second"},
                ],
            }
        )

        self.assertEqual(first["cache_read"], 0)
        self.assertGreater(same_partition["cache_read"], 1_024)
        self.assertEqual(other_partition["cache_read"], 0)
        for usage in (first, same_partition, other_partition):
            self.assertEqual(
                usage["logical"],
                usage["cache_read"] + usage["cache_write"] + usage["fresh"],
            )

    def test_foreign_daemon_snapshot_parser_keeps_only_exact_daemons(self) -> None:
        rows = parse_daemon_processes(
            "  17 /tmp/base/haiderd\n"
            "  18 /tmp/candidate/haiderd\n"
            "  19 /tmp/haiderd-helper\n"
            "bad malformed\n"
            "  20 python3\n"
        )

        self.assertEqual(
            rows,
            [
                {"pid": 17, "executable": "/tmp/base/haiderd"},
                {"pid": 18, "executable": "/tmp/candidate/haiderd"},
            ],
        )

    def test_static_contract_is_pinned(self):
        checks = self_check()
        self.assertEqual(checks["turns"], 40)
        self.assertEqual(tuple(checks["checkpoints"]), CHECKPOINTS)
        self.assertEqual(tuple(checks["tool_turns"]), TOOL_TURNS)
        self.assertEqual(tuple(checks["spill_turns"]), SPILL_TURNS)
        self.assertEqual(checks["abba_n_per_arm_at_three_rounds"], 6)

    def test_catalog_is_populated_and_selected_pair_is_first(self):
        catalog = provider_catalog("http://127.0.0.1:1234/v1")
        self.assertEqual(len(catalog["providers"]), PROVIDER_COUNT)
        selected = catalog["providers"][0]
        self.assertEqual(selected["provider_id"], PROVIDER_ID)
        self.assertEqual(selected["default_model"], MODEL_ID)
        self.assertEqual(len(selected["configured_models"]), 8)
        self.assertEqual(
            len({row["provider_id"] for row in catalog["providers"]}),
            PROVIDER_COUNT,
        )

    def test_turn_plan_repeats_tools_and_forces_three_spills(self):
        self.assertEqual(sum(turn_is_tool(turn) for turn in range(1, 41)), 10)
        self.assertEqual(sum(turn_is_spill(turn) for turn in range(1, 41)), 3)
        for turn in SPILL_TURNS:
            command = process_command(turn)
            self.assertIn("deep-effects.log", command)
            self.assertIn("6000", command)

    def test_fragmentation_is_lossless_and_bounded(self):
        payload = bytes(range(256)) * 3
        fragments = fragment_bytes(payload)
        self.assertEqual(b"".join(fragments), payload)
        self.assertLessEqual(max(map(len, fragments)), 21)
        self.assertGreater(len(fragments), 40)

    def test_tool_arguments_follow_the_advertised_schema_and_are_fragmented(self):
        body = {
            "tools": [
                {
                    "type": "function",
                    "function": {
                        "name": "process_exec",
                        "parameters": {
                            "type": "object",
                            "properties": {"command": {"type": "string"}},
                            "required": ["command"],
                        },
                    },
                }
            ]
        }
        payload = b"".join(tool_response(8, body)).decode()
        data = [
            json.loads(line.removeprefix("data: "))
            for line in payload.splitlines()
            if line.startswith("data: {")
        ]
        calls = [
            choice
            for document in data
            for choice in document["choices"][0]["delta"].get("tool_calls", [])
        ]
        self.assertGreater(len(calls), 3)
        arguments = "".join(call.get("function", {}).get("arguments", "") for call in calls)
        decoded = json.loads(arguments)
        self.assertIn("6000", decoded["command"])
        self.assertEqual(calls[0]["function"]["name"], "process_exec")

    def test_continuations_cannot_override_session_configuration(self):
        initial = _initial_arguments()
        continuation = _continuation_arguments("session-1", 2)
        self.assertIn("--provider", initial)
        self.assertIn("--model", initial)
        self.assertIn("--session", continuation)
        self.assertNotIn("--provider", continuation)
        self.assertNotIn("--model", continuation)

    def test_continuation_jsonl_starts_after_the_pre_submit_head(self):
        output = "\n".join(
            json.dumps(row)
            for row in (
                {"event": "accepted", "session_id": "session-1", "head_seq": 28},
                {
                    "seq": 29,
                    "session_id": "session-1",
                    "run_id": "run-2",
                    "payload": {"terminal_kind": "done"},
                },
            )
        )
        parsed = validate_jsonl(output, "continuation", continuation=True)
        self.assertEqual(parsed["terminal_seq"], 29)
        with self.assertRaisesRegex(Exception, "head_seq"):
            validate_jsonl(output, "fresh")

    def test_abba_has_two_samples_per_arm_per_block(self):
        order = abba_order(3)
        for block in range(1, 4):
            arms = [arm for actual, _position, arm in order if actual == block]
            self.assertEqual(arms, ["a", "b", "b", "a"])

    def test_wall_toolpath_compares_distinct_binary_directories(self):
        args = SimpleNamespace(
            experiment="wall-toolpath",
            bin_dir=Path("base"),
            variant_bin_dir=Path("candidate"),
        )
        arm_a, arm_b = arms_for(args)
        self.assertEqual(arm_a.bin_dir, Path("base").resolve())
        self.assertEqual(arm_b.bin_dir, Path("candidate").resolve())
        self.assertIn("pre-WALL-3+4", arm_a.description)
        self.assertIn("WALL-3+4", arm_b.description)

    def test_store_trace_parser_rejects_partial_and_user_fields(self):
        text = "\n".join(
            (
                "haiderd: trace level=TRACE target=haider.store "
                "queue_wait_micros=7 operation_micros=11",
                "haiderd: trace level=TRACE target=haider.store queue_wait_micros=9",
                "haiderd: trace level=TRACE target=other "
                "queue_wait_micros=1 operation_micros=2 prompt=secret",
            )
        )
        self.assertEqual(
            parse_store_records(text),
            [{"queue_wait_micros": 7, "operation_micros": 11}],
        )


if __name__ == "__main__":
    unittest.main()
