"""Record every golden difference against both parents and their common base."""
import difflib
import json
from pathlib import Path
import re
import subprocess

evidence = Path(__file__).resolve().parent
paths = [
    "crates/haider-cli/tests/fixtures/oneshot_run_golden.jsonl",
    "crates/haider-cli/tests/fixtures/turnhygiene/run_jsonl_text_turn.jsonl",
    "crates/haider-cli/tests/fixtures/turnhygiene/run_jsonl_tool_turn.jsonl",
    "crates/haider-cli/tests/fixtures/turnhygiene/provider_request_no_budget.json",
]

def parse(text):
    # Leave placeholders inside JSON strings (including nested preview JSON)
    # untouched; quote only unquoted normalized numeric values.
    return json.loads(re.sub(r'("(?:\\.|[^"\\])*")|(<[A-Z0-9_]+>)',
        lambda match: match[1] if match[1] is not None else json.dumps(match[2]), text))

def differences(old, new, path=""):
    if old == new:
        return []
    if isinstance(old, dict) and isinstance(new, dict):
        rows = []
        for key in sorted(old.keys() | new.keys()):
            child = path + "/" + key
            if key not in old:
                rows.append({"path": child, "added": new[key]})
            elif key not in new:
                rows.append({"path": child, "removed": old[key]})
            else:
                rows.extend(differences(old[key], new[key], child))
        return rows
    return [{"path": path, "before": old, "after": new}]

base = subprocess.check_output(["git", "merge-base", "HEAD", "MERGE_HEAD"], text=True).strip()
report = {"base": base, "comparisons": []}
for ref in ["HEAD", "MERGE_HEAD", base]:
    for name in paths:
        old_text = subprocess.check_output(["git", "show", f"{ref}:{name}"], text=True)
        new_text = Path(name).read_text()
        assert not re.search(r"^(<<<<<<<|=======|>>>>>>>)", new_text, re.M)
        if name.endswith(".jsonl"):
            old_lines, new_lines = old_text.splitlines(), new_text.splitlines()
        else:
            old_lines = [json.dumps(parse(old_text), sort_keys=True)]
            new_lines = [json.dumps(parse(new_text), sort_keys=True)]
        row = {"parent": ref, "file": name, "records_before": len(old_lines),
               "records_after": len(new_lines), "changed": []}
        # Both parents retain record counts. Relative to their common base,
        # ceilingdecl inserts exactly one durable workspace-receipt pair.
        indexed_new = list(enumerate(new_lines, 1))
        if ref == base and name.endswith(".jsonl"):
            assert len(new_lines) == len(old_lines) + 2
            for index in [12, 13]:
                value = parse(new_lines[index - 1])
                assert value["payload"]["item"]["kind"] == "turn_workspace_before_v1"
                row["changed"].append({"line": index,
                    "differences": [{"path": "/", "added": value}]})
            indexed_new = [(index, line) for index, line in indexed_new if index not in [12, 13]]
        assert len(old_lines) == len(indexed_new), (ref, name, "record count drift")
        for old, (index, new) in zip(old_lines, indexed_new):
            if old != new:
                row["changed"].append({"line": index,
                                       "differences": differences(parse(old), parse(new))})
        report["comparisons"].append(row)
        print(f"{ref[:12]} {name}: {len(row['changed'])}/{len(new_lines)} changed records")
        for item in row["changed"]:
            print(f"  line {item['line']}: " + ", ".join(d["path"] for d in item["differences"]))
        diff_path = evidence / (ref[:12] + "-" + Path(name).name + ".diff")
        diff_path.write_text("".join(difflib.unified_diff(old_text.splitlines(True),
            new_text.splitlines(True), fromfile=f"{ref}:{name}", tofile=name)))
(evidence / "golden-review.json").write_text(json.dumps(report, indent=2) + "\n")
