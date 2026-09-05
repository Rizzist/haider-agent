import json, re
from pathlib import Path
E = Path(__file__).resolve().parent
s = (E / 'workspace-tests.log').read_text()
# Child processes share stdout and can interleave the `test result: ok.` prefix.
# The numeric summary suffix stays intact. The unfiltered workspace command
# has zero filtered tests; nonzero filters identify its reexecuted child probes.
rows = [tuple(map(int, row)) for row in re.findall(r'(\d+) passed; (\d+) failed; (\d+) ignored; (\d+) measured; (\d+) filtered out', s)]
top = [r for r in rows if r[4] == 0]
nested = [r for r in rows if r[4] > 0]
result = {
    'top_level_result_blocks': len(top),
    'top_level_passed': sum(r[0] for r in top),
    'failed': sum(r[1] for r in rows),
    'ignored': sum(r[2] for r in top),
    'nested_result_blocks': len(nested),
    'nested_passed': sum(r[0] for r in nested),
    'all_result_blocks': len(rows),
    'all_passed_including_nested': sum(r[0] for r in rows),
    'cargo_exit': int((E / 'workspace-tests.exit').read_text()),
}
assert result['failed'] == 0 and result['cargo_exit'] == 0
print(json.dumps(result, indent=2))
(E / 'workspace-totals.json').write_text(json.dumps(result, indent=2) + '\n')
