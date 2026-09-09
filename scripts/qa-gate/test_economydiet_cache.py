"""Dialect checks for the cache capture measurement (no network or billing)."""
import unittest
from economydiet_measure import cache_capture_parts


class CacheCaptureTests(unittest.TestCase):
    def test_anthropic_system_shape_and_controls_preserve_logical_input(self):
        before = {"body": {"system": "policy", "tools": [], "messages": []}}
        after = {"body": {"system": [{"type": "text", "text": "policy", "cache_control": {"type": "ephemeral"}}], "tools": [], "messages": []}}
        self.assertEqual(cache_capture_parts(before, "anthropic-messages"), cache_capture_parts(after, "anthropic-messages"))

    def test_tool_argument_named_cache_control_is_logical_input(self):
        record = {"body": {"system": [], "messages": [{"role": "assistant", "content": [{"type": "tool_use", "input": {"cache_control": "retain"}, "cache_control": {"type": "ephemeral"}}]}]}}
        body, _ = cache_capture_parts(record, "anthropic-messages")
        self.assertEqual(body["messages"][0]["content"][0]["input"]["cache_control"], "retain")
        self.assertNotIn("cache_control", body["messages"][0]["content"][0])

    def test_gemini_resource_requires_and_reconstructs_exact_prefix(self):
        cached = {"body": {"cachedContent": "cachedContents/test", "contents": [{"role": "user", "parts": [{"text": "tail"}]}]}}
        with self.assertRaisesRegex(ValueError, "cached_prefix"):
            cache_capture_parts(cached, "gemini-generate-content")
        cached["cached_prefix"] = {"contents": [{"role": "model", "parts": [{"text": "old"}]}], "systemInstruction": {"parts": [{"text": "policy"}]}, "tools": []}
        body, fixed = cache_capture_parts(cached, "gemini-generate-content")
        self.assertEqual(len(body["contents"]), 2)
        self.assertNotIn("cachedContent", body)
        self.assertEqual(fixed["system_instruction"], cached["cached_prefix"]["systemInstruction"])


if __name__ == "__main__":
    unittest.main()
