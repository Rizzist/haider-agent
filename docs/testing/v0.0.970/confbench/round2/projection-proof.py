"""Run from repository root; reads the old benchmark without changing it."""
import json
import sys
from pathlib import Path

sys.dont_write_bytecode = True
sys.path.insert(0, "/Users/rizzist/Documents/CODING/haidercode-web")
from bench.adapters import load_adapter
from bench.adapters.normalize import normalize_events

source = Path("docs/testing/v0.0.970/confbench/diagnostic/malformed_tool.jsonl")
completion = next(json.loads(line) for line in source.read_text().splitlines()
                  if json.loads(line).get("payload", {}).get("reason") == "malformed_tool_call")
assert completion["payload"]["item"]["status"] == "failed"
assert completion["payload"]["failed"] is True
normalized = normalize_events(load_adapter("haider-agent"), [completion]).events
assert len(normalized) == 1
assert normalized[0]["status"] == "failed"
assert "failed" not in normalized[0]
proof = {
    "source": str(source),
    "raw_completion": completion,
    "normalized_events": normalized,
    "old_malformed_failed_predicate": any(event.get("failed") is True for event in normalized),
    "status_failed_is_preserved": True,
    "failed_boolean_is_derived": False,
}
Path(__file__).with_suffix(".json").write_text(json.dumps(proof, indent=2) + "\n")
print("PASS: failed status survives; old normalizer does not derive failed=True")
